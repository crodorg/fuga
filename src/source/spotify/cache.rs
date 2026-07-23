//! Disk + in-memory cache for `SpotifySource::browse` results.
//!
//! Browse views (Saved Albums, Liked Songs, artist pages, etc.) are
//! near-static catalog data. Hitting the Web API on every tab switch was the
//! main first-open slowness — pagination walks for hundreds of albums block
//! the UI thread until the stream completes.
//!
//! Strategy: serialize each browse result to `<dir>/<sha256(path)>.json`,
//! mirror the most-recent entries in a small in-memory LRU. On `get`, return
//! `Fresh` if the entry is within FRESH_TTL (1h), `Stale` if older (caller
//! decides to refetch synchronously and replace), `Miss` if the path was never
//! cached. Empty results are never persisted — they're almost always a
//! transient failure, and caching one would blank the view for FRESH_TTL.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use anyhow::Result;
use lru::LruCache;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::types::Entry;

/// 1h fresh TTL. Browse views (saved albums, playlists, library trees) are
/// near-static — bumped from 5min so the user doesn't re-pay the slow
/// hydration walk on every casual tab-switch. Playlist hydration can run
/// 20-30s for 500-track playlists when mercury misses force per-track Web
/// API fallback; the cost is unbearable to repeat every 5min.
const FRESH_TTL: Duration = Duration::from_secs(3600);

#[derive(Serialize, Deserialize, Clone)]
struct CachedEntry {
    fetched_at: SystemTime,
    entries: Vec<Entry>,
    /// Mercury playlist revision (hex) or any opaque snapshot id. Used by
    /// the playlist-full cache to compare against a freshly-fetched mercury
    /// revision and either delta-hydrate or short-circuit re-fetch.
    /// `None` for non-playlist caches that pre-date this field.
    #[serde(default)]
    revision: Option<String>,
}

pub enum CacheHit {
    Fresh(Vec<Entry>),
    Stale(Vec<Entry>),
    Miss,
}

pub struct BrowseCache {
    dir: PathBuf,
    mem: Mutex<LruCache<String, CachedEntry>>,
}

impl BrowseCache {
    pub fn new(dir: PathBuf, capacity: usize) -> Self {
        if let Err(e) = std::fs::create_dir_all(&dir) {
            tracing::warn!("spotify cache dir create failed at {}: {e}", dir.display());
        }
        let cap = std::num::NonZeroUsize::new(capacity)
            .unwrap_or(std::num::NonZeroUsize::new(64).unwrap());
        Self {
            dir,
            mem: Mutex::new(LruCache::new(cap)),
        }
    }

    fn path_for(&self, key: &str) -> PathBuf {
        let mut h = Sha256::new();
        h.update(key.as_bytes());
        let hex = hex_lower(&h.finalize());
        self.dir.join(format!("{hex}.json"))
    }

    pub async fn get(&self, key: &str) -> CacheHit {
        // Memory hit first (zero I/O).
        if let Some(entry) = self.mem.lock().ok().and_then(|mut m| m.get(key).cloned()) {
            return classify(entry);
        }
        // Disk fallback.
        let path = self.path_for(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<CachedEntry>(&bytes) {
                Ok(entry) => {
                    if let Ok(mut m) = self.mem.lock() {
                        m.put(key.to_string(), entry.clone());
                    }
                    classify(entry)
                }
                Err(e) => {
                    tracing::debug!("spotify cache decode {key}: {e}");
                    CacheHit::Miss
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => CacheHit::Miss,
            Err(e) => {
                tracing::debug!("spotify cache read {key}: {e}");
                CacheHit::Miss
            }
        }
    }

    pub async fn put(&self, key: &str, entries: Vec<Entry>) -> Result<()> {
        // Never persist an empty result. A `browse` that failed transiently
        // (auth error, rate-limit, dropped stream) returns `Ok(vec![])`, and
        // caching that would serve a blank view as `Fresh` for FRESH_TTL (1h)
        // even after the fault clears — the bug behind "playlists show 0 after
        // a bad token". A genuinely empty view just re-walks the API next open,
        // which is cheap and rare.
        if entries.is_empty() {
            return Ok(());
        }
        let entry = CachedEntry {
            fetched_at: SystemTime::now(),
            entries,
            revision: None,
        };
        if let Ok(mut m) = self.mem.lock() {
            m.put(key.to_string(), entry.clone());
        }
        let path = self.path_for(key);
        let bytes = serde_json::to_vec(&entry)?;
        if let Err(e) = tokio::fs::write(&path, bytes).await {
            tracing::warn!("spotify cache write {}: {e}", path.display());
        }
        Ok(())
    }

    /// Variant that records an opaque revision string alongside the entries.
    /// Used by the playlist-full cache; the caller decides freshness by
    /// comparing the stored revision against a freshly-fetched one.
    pub async fn put_with_revision(
        &self,
        key: &str,
        entries: Vec<Entry>,
        revision: Option<String>,
    ) -> Result<()> {
        let entry = CachedEntry {
            fetched_at: SystemTime::now(),
            entries,
            revision,
        };
        if let Ok(mut m) = self.mem.lock() {
            m.put(key.to_string(), entry.clone());
        }
        let path = self.path_for(key);
        let bytes = serde_json::to_vec(&entry)?;
        if let Err(e) = tokio::fs::write(&path, bytes).await {
            tracing::warn!("spotify cache write {}: {e}", path.display());
        }
        Ok(())
    }

    /// Drop the cached entry for `key` (memory + disk) so the next browse
    /// refetches live. Used by force-refresh and the open-view change poller.
    pub async fn invalidate(&self, key: &str) {
        if let Ok(mut m) = self.mem.lock() {
            m.pop(key);
        }
        let path = self.path_for(key);
        let _ = tokio::fs::remove_file(&path).await;
    }

    /// Raw access bypassing the TTL classifier. Returns the stored entries
    /// and revision regardless of age. Playlist-full callers use this to
    /// decide freshness from the revision rather than a clock-based TTL.
    pub async fn get_raw(&self, key: &str) -> Option<(Vec<Entry>, Option<String>)> {
        if let Some(entry) = self.mem.lock().ok().and_then(|mut m| m.get(key).cloned()) {
            return Some((entry.entries, entry.revision));
        }
        let path = self.path_for(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => match serde_json::from_slice::<CachedEntry>(&bytes) {
                Ok(entry) => {
                    if let Ok(mut m) = self.mem.lock() {
                        m.put(key.to_string(), entry.clone());
                    }
                    Some((entry.entries, entry.revision))
                }
                Err(e) => {
                    tracing::debug!("spotify cache_raw decode {key}: {e}");
                    None
                }
            },
            Err(_) => None,
        }
    }
}

fn classify(entry: CachedEntry) -> CacheHit {
    let age = SystemTime::now()
        .duration_since(entry.fetched_at)
        .unwrap_or(Duration::from_secs(0));
    if age < FRESH_TTL {
        CacheHit::Fresh(entry.entries)
    } else {
        CacheHit::Stale(entry.entries)
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Paths whose results are NOT safe to cache: anything tied to current
/// playback state or session-bound endpoints. These are rare in browse() but
/// the gate is here so we can't accidentally cache one if it gets added.
pub fn is_cacheable(path: &str) -> bool {
    !path.is_empty()
        && !path.contains("current_playback")
        && !path.contains("devices")
        && path.starts_with("spotify:")
        // Artist sub-views (top tracks / albums) come from mercury and are
        // small + fast. Skipping cache here means rebuilds + sort changes
        // take effect immediately instead of waiting for the 5 min TTL.
        && !path.starts_with("spotify:artistview:")
        // Playlist paths own their cache: `browse_playlist_via_mercury`
        // keeps a permanent revision-keyed full-playlist cache under
        // `spotify:playlistfull:{id}` and decides freshness from the
        // mercury revision rather than a clock TTL. Letting the per-page
        // cache shadow that with stale `?offset=N` snapshots defeats the
        // whole point — the user would see stale paginated data for up
        // to FRESH_TTL even after the background hydrate filled the full
        // cache.
        && !path.starts_with("spotify:playlist:")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Entry, EntryKind};

    fn tmp_cache() -> BrowseCache {
        let dir = std::env::temp_dir().join(format!(
            "fuga-cache-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        BrowseCache::new(dir, 8)
    }

    fn sample() -> Vec<Entry> {
        vec![Entry {
            uri: "spotify:playlist:x".into(),
            label: "x".into(),
            kind: EntryKind::Playlist,
            display: None,
        }]
    }

    #[tokio::test]
    async fn empty_result_is_not_persisted() {
        // Regression: a failed browse returns Ok(vec![]); caching it would
        // serve a blank view as Fresh for an hour. put() must drop empties.
        let c = tmp_cache();
        c.put("spotify:view:playlists", vec![]).await.unwrap();
        assert!(matches!(
            c.get("spotify:view:playlists").await,
            CacheHit::Miss
        ));
    }

    #[tokio::test]
    async fn non_empty_result_is_fresh() {
        let c = tmp_cache();
        c.put("spotify:view:playlists", sample()).await.unwrap();
        assert!(matches!(
            c.get("spotify:view:playlists").await,
            CacheHit::Fresh(v) if v.len() == 1
        ));
    }
}
