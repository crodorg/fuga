//! Synced lyrics via the public lrclib.net API.
//!
//! LRC fetch + timestamp parsing adapted from LargeModGames/spotatui (MIT).
//! lrclib needs only track title, artist, and duration — there is no Spotify
//! coupling — so this works for every fuga source whose `ItemDisplay` carries
//! those fields (local, Spotify, YouTube, SomaFM, radio).

use std::sync::OnceLock;
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LyricsStatus {
    Loading,
    /// Timestamped lyrics — rendered with the active line centered + advanced
    /// against playback position.
    Synced,
    /// Untimed lyrics — every `lines` timestamp is `0`; rendered as a static
    /// top-aligned block with no highlight.
    Plain,
    NotFound,
}

/// One track's lyrics. `lines` is `(timestamp_ms, text)`. `uri` tags the owning
/// track so a delivery that lands after the user skipped on is dropped instead
/// of shown against the wrong song.
#[derive(Debug, Clone)]
pub struct TrackLyrics {
    pub uri: String,
    pub status: LyricsStatus,
    pub lines: Vec<(u128, String)>,
}

impl TrackLyrics {
    fn bare(uri: String, status: LyricsStatus) -> Self {
        Self {
            uri,
            status,
            lines: Vec::new(),
        }
    }
    pub fn loading(uri: String) -> Self {
        Self::bare(uri, LyricsStatus::Loading)
    }
    pub fn not_found(uri: String) -> Self {
        Self::bare(uri, LyricsStatus::NotFound)
    }
}

#[derive(Deserialize, Debug)]
#[allow(non_snake_case)]
struct LrcResponse {
    syncedLyrics: Option<String>,
    plainLyrics: Option<String>,
}

/// Process-wide client carrying a UA that identifies fuga — lrclib asks clients
/// to identify themselves.
fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .user_agent(concat!(
                "fuga/",
                env!("CARGO_PKG_VERSION"),
                " (https://github.com/crodorg/fuga)"
            ))
            .build()
            .unwrap_or_default()
    })
}

/// Fetch lyrics for a track from lrclib. Never errors — transport / parse
/// failures collapse to `NotFound` so the UI always reaches a terminal state.
pub async fn fetch(uri: String, title: &str, artist: &str, duration: Duration) -> TrackLyrics {
    let secs = duration.as_secs().to_string();
    let url = match url::Url::parse_with_params(
        "https://lrclib.net/api/get",
        &[
            ("track_name", title),
            ("artist_name", artist),
            ("duration", secs.as_str()),
        ],
    ) {
        Ok(u) => u,
        Err(_) => return TrackLyrics::not_found(uri),
    };
    let resp = match client().get(url).send().await {
        Ok(r) if r.status().is_success() => r,
        _ => return TrackLyrics::not_found(uri),
    };
    let body: LrcResponse = match resp.json().await {
        Ok(b) => b,
        Err(_) => return TrackLyrics::not_found(uri),
    };

    // Prefer synced (timestamped) lyrics. Fall back to plain only when synced
    // is absent — spotatui ran plain text through the LRC parser too, which
    // dropped every line (no brackets) and showed plain-only tracks as "not
    // found"; splitting plain into untimed lines fixes that.
    if let Some(synced) = body.syncedLyrics.filter(|s| !s.trim().is_empty()) {
        let lines = parse_lrc(&synced);
        if !lines.is_empty() {
            return TrackLyrics {
                uri,
                status: LyricsStatus::Synced,
                lines,
            };
        }
    }
    if let Some(plain) = body.plainLyrics.filter(|s| !s.trim().is_empty()) {
        let lines: Vec<(u128, String)> = plain
            .lines()
            .map(|l| (0u128, l.trim_end().to_string()))
            .collect();
        if !lines.is_empty() {
            return TrackLyrics {
                uri,
                status: LyricsStatus::Plain,
                lines,
            };
        }
    }
    TrackLyrics::not_found(uri)
}

/// Build lyrics from a raw blob of unknown type (an embedded metadata tag).
/// LRC-timestamped → `Synced`; otherwise each line → `Plain`; all-blank →
/// `NotFound`.
pub fn from_text(uri: String, blob: &str) -> TrackLyrics {
    let synced = parse_lrc(blob);
    if !synced.is_empty() {
        return TrackLyrics {
            uri,
            status: LyricsStatus::Synced,
            lines: synced,
        };
    }
    let plain: Vec<(u128, String)> = blob
        .lines()
        .map(|l| (0u128, l.trim_end().to_string()))
        .collect();
    if plain.iter().any(|(_, t)| !t.is_empty()) {
        TrackLyrics {
            uri,
            status: LyricsStatus::Plain,
            lines: plain,
        }
    } else {
        TrackLyrics::not_found(uri)
    }
}

/// Parse an LRC blob into `(timestamp_ms, text)` pairs sorted by time. Lines
/// without a leading `[mm:ss.xx]` tag (blank lines, `[ar:..]` metadata) are
/// skipped.
fn parse_lrc(text: &str) -> Vec<(u128, String)> {
    let mut out: Vec<(u128, String)> = text.lines().filter_map(parse_lrc_line).collect();
    out.sort_by_key(|(ms, _)| *ms);
    out
}

/// Parse one `[mm:ss.xx] text` line. Ported from spotatui `utils.rs`: handles
/// 2- or 3-digit fractional seconds (`.xx` → centiseconds, `.xxx` →
/// milliseconds).
fn parse_lrc_line(line: &str) -> Option<(u128, String)> {
    let idx = line.find(']')?;
    if idx <= 1 || !line.starts_with('[') {
        return None;
    }
    let timestamp = &line[1..idx];
    let content = line[idx + 1..].trim().to_string();

    let (mm, rest) = timestamp.split_once(':')?;
    let mins = mm.parse::<u64>().ok()?;
    let (ss, frac) = match rest.split_once('.') {
        Some((s, f)) => (s, Some(f)),
        None => (rest, None),
    };
    let secs = ss.parse::<u64>().ok()?;
    let ms = match frac {
        Some(f) => {
            let v = f.parse::<u64>().unwrap_or(0);
            if f.len() == 2 {
                v * 10
            } else {
                v
            }
        }
        None => 0,
    };
    let total = (mins * 60 * 1000) + (secs * 1000) + ms;
    Some((total as u128, content))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synced_lrc_and_skips_non_timed_lines() {
        let blob = "[ar:Some Artist]\n\
                    [00:12.34]Hello world\n\
                    [00:15.00]Second line\n\
                    not a lyric line\n\
                    [01:05.50]Third";
        let lines = parse_lrc(blob);
        assert_eq!(
            lines,
            vec![
                (12_340u128, "Hello world".to_string()),
                (15_000u128, "Second line".to_string()),
                (65_500u128, "Third".to_string()),
            ]
        );
    }

    #[test]
    fn handles_three_digit_milliseconds() {
        let lines = parse_lrc("[00:01.250]X");
        assert_eq!(lines, vec![(1_250u128, "X".to_string())]);
    }

    #[test]
    fn sorts_out_of_order_timestamps() {
        let lines = parse_lrc("[00:20.00]b\n[00:10.00]a");
        assert_eq!(
            lines,
            vec![(10_000u128, "a".to_string()), (20_000u128, "b".to_string())]
        );
    }

    #[test]
    fn empty_blob_yields_no_lines() {
        assert!(parse_lrc("").is_empty());
    }
}
