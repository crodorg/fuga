//! Process-global rate-limit governor for the Spotify Web API.
//!
//! Spotify's Web API rate limit is per-app (client_id), over a rolling 30s
//! window, with no published numbers. When tripped it returns 429 with a
//! `Retry-After` that *escalates punitively* — community reports of multi-hour
//! values from a single 429, and observed ~9000s on this app. The penalty
//! grows every time you call during the window, so the only safe response is
//! to stop calling entirely until it passes.
//!
//! This governor is the single choke point every Web API call passes through:
//! - **Pacing** — a ≥250ms minimum interval between call *starts* (mirrors
//!   spotatui's `SPOTIFY_API_MIN_INTERVAL`), guard held across the sleep so
//!   concurrent callers serialize.
//! - **Fail-fast gate** — once a 429 is seen, `blocked_until` is set and every
//!   subsequent `enter()` returns [`RateLimited`] *without touching the
//!   network* until the window passes. This is what stops fuga escalating its
//!   own ban (the thing spotatui lacks).
//! - **Persistence** — the cooldown deadline is written to disk so a restart
//!   doesn't immediately re-ping a multi-hour ban (also absent upstream).
//!
//! Unlike spotatui (and fuga's previous `raw.rs`), we never *sleep* inline for
//! the Retry-After — a 9000s value would block a task for hours. We record the
//! deadline and fail fast.

use std::path::{Path, PathBuf};
use std::sync::Mutex as StdMutex;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::Mutex as AsyncMutex;

/// Minimum interval between Web API call starts.
const MIN_INTERVAL: Duration = Duration::from_millis(250);

/// Display-only hint shown for a 429 surfaced by an rspotify-direct call when
/// no real cooldown has been measured yet (its error type buries the
/// `reqwest::Response`, on a different reqwest version than ours, so the real
/// `Retry-After` is unreadable there). It does NOT set the gate — only the
/// header-reading `raw::get_normalized` path does — so it can't "poison" the
/// gate short and block the real measurement.
pub const FALLBACK_COOLDOWN: Duration = Duration::from_secs(30);

/// Error returned by [`ApiGovernor::enter`] while a rate-limit window is
/// active, and by the call wrappers when a 429 is first detected. Carries the
/// remaining cooldown so the UI can show an honest countdown.
#[derive(Debug, Clone)]
pub struct RateLimited(pub Duration);

impl std::fmt::Display for RateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Spotify rate-limited — retry in {}", fmt_dur(self.0))
    }
}

impl std::error::Error for RateLimited {}

/// Human-readable duration: `2h31m`, `4m05s`, or `45s`.
pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else if s >= 60 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else {
        format!("{}s", s.max(1))
    }
}

/// True if a `Display`able error (anyhow or `rspotify::ClientError`) looks like
/// a 429. rspotify renders it as `http error: status code 429`.
pub fn is_rate_limit_err<E: std::fmt::Display>(e: &E) -> bool {
    let s = e.to_string();
    s.contains("429") || s.contains("Too Many Requests") || s.contains("TOO_MANY_REQUESTS")
}

pub struct ApiGovernor {
    /// Last call start, for pacing. Held across the pacing sleep so callers
    /// serialize ≥`MIN_INTERVAL` apart.
    last_call: AsyncMutex<Option<Instant>>,
    /// When set and in the future, all calls fail fast until it passes.
    blocked_until: StdMutex<Option<Instant>>,
    /// Where the wall-clock cooldown deadline persists across restarts.
    cooldown_path: PathBuf,
}

impl ApiGovernor {
    pub fn new(cooldown_path: PathBuf) -> Self {
        let blocked_until = StdMutex::new(load_cooldown(&cooldown_path));
        Self {
            last_call: AsyncMutex::new(None),
            blocked_until,
            cooldown_path,
        }
    }

    /// Remaining cooldown, or `None` if not currently rate-limited. Clears the
    /// gate once the window has passed.
    pub fn remaining_block(&self) -> Option<Duration> {
        let mut g = self.blocked_until.lock().unwrap();
        match *g {
            Some(until) => {
                let now = Instant::now();
                if until > now {
                    Some(until - now)
                } else {
                    *g = None;
                    None
                }
            }
            None => None,
        }
    }

    /// Gate + pace. Returns `Err(RateLimited)` immediately (no network) while a
    /// cooldown is active; otherwise waits out the pacing interval and returns.
    pub async fn enter(&self) -> Result<(), RateLimited> {
        if let Some(rem) = self.remaining_block() {
            return Err(RateLimited(rem));
        }
        let mut last = self.last_call.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_INTERVAL {
                tokio::time::sleep(MIN_INTERVAL - elapsed).await;
            }
        }
        *last = Some(Instant::now());
        Ok(())
    }

    /// Record a rate-limit window of `retry_after`. Takes the max with any
    /// existing window (a real `Retry-After` read from a header must never be
    /// shortened by a later [`FALLBACK_COOLDOWN`]). Persists the deadline.
    pub fn note_rate_limited(&self, retry_after: Duration) {
        let now = Instant::now();
        let until = now + retry_after;
        let updated = {
            let mut g = self.blocked_until.lock().unwrap();
            if g.is_none_or(|existing| until > existing) {
                *g = Some(until);
                true
            } else {
                false
            }
        };
        if updated {
            if let Ok(epoch) = SystemTime::now().duration_since(UNIX_EPOCH) {
                let deadline = epoch.as_secs() + retry_after.as_secs();
                let _ = std::fs::write(&self.cooldown_path, deadline.to_string());
            }
            tracing::warn!(
                retry_after_s = retry_after.as_secs(),
                "spotify rate-limited; gating web API"
            );
        }
    }
}

/// Read a persisted cooldown deadline (Unix epoch seconds). Returns an
/// `Instant` only if the deadline is still in the future.
fn load_cooldown(path: &Path) -> Option<Instant> {
    let deadline_epoch: u64 = std::fs::read_to_string(path).ok()?.trim().parse().ok()?;
    let now_epoch = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    if deadline_epoch > now_epoch {
        Some(Instant::now() + Duration::from_secs(deadline_epoch - now_epoch))
    } else {
        None
    }
}

static GOVERNOR: OnceLock<ApiGovernor> = OnceLock::new();

/// Initialize the process-global governor with the persistence path. Called
/// once when the Spotify source is built; a no-op if already configured.
pub fn configure(cooldown_path: PathBuf) {
    let _ = GOVERNOR.set(ApiGovernor::new(cooldown_path));
}

/// The process-global governor. Falls back to a temp-dir cooldown file if
/// `configure` was never called (only happens outside the normal app path).
pub fn instance() -> &'static ApiGovernor {
    GOVERNOR
        .get_or_init(|| ApiGovernor::new(std::env::temp_dir().join("fuga-spotify-ratelimit.json")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("fuga-gov-test-{}-{}.txt", name, std::process::id()));
        let _ = std::fs::remove_file(&p);
        p
    }

    #[test]
    fn no_block_by_default() {
        let g = ApiGovernor::new(tmp("default"));
        assert!(g.remaining_block().is_none());
    }

    #[test]
    fn note_sets_and_reports_block() {
        let g = ApiGovernor::new(tmp("note"));
        g.note_rate_limited(Duration::from_secs(60));
        let rem = g.remaining_block().expect("should be blocked");
        assert!(rem.as_secs() > 55 && rem.as_secs() <= 60);
    }

    #[test]
    fn note_takes_max_not_latest() {
        let g = ApiGovernor::new(tmp("max"));
        g.note_rate_limited(Duration::from_secs(600));
        g.note_rate_limited(Duration::from_secs(10)); // must not shorten
        assert!(g.remaining_block().unwrap().as_secs() > 500);
    }

    #[test]
    fn persists_and_reloads_cooldown() {
        let path = tmp("persist");
        {
            let g = ApiGovernor::new(path.clone());
            g.note_rate_limited(Duration::from_secs(300));
        }
        // A fresh governor on the same path should still be blocked.
        let g2 = ApiGovernor::new(path.clone());
        let rem = g2.remaining_block().expect("reloaded block");
        assert!(rem.as_secs() > 280 && rem.as_secs() <= 300);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn expired_persisted_cooldown_is_ignored() {
        let path = tmp("expired");
        // Deadline one second in the past.
        let past = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            - 1;
        std::fs::write(&path, past.to_string()).unwrap();
        let g = ApiGovernor::new(path.clone());
        assert!(g.remaining_block().is_none());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn detects_rate_limit_errors() {
        assert!(is_rate_limit_err(&"http error: status code 429"));
        assert!(is_rate_limit_err(&"Too Many Requests"));
        assert!(!is_rate_limit_err(&"status code 404"));
    }

    #[test]
    fn fmt_dur_formats() {
        assert_eq!(fmt_dur(Duration::from_secs(9110)), "2h31m");
        assert_eq!(fmt_dur(Duration::from_secs(125)), "2m05s");
        assert_eq!(fmt_dur(Duration::from_secs(5)), "5s");
    }
}
