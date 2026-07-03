//! YouTube source via `yt-dlp` shell-out.
//!
//! v0.2.0 scope: search + play + seek + local-only saved-tracks
//! bookmarking + download. No cookie-based library access. The user must
//! have `yt-dlp` installed on PATH (or via the configured binary path).
//! fuga itself never speaks to YouTube — it only invokes the local binary
//! and consumes its JSON output.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use mpd_client::{client::Client, commands};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::timeout;

use crate::source::MusicSource;
use crate::source::mpd_shared::{mpd_set_volume, mpd_status};
use crate::types::{ArtSize, Entry, EntryKind, Item, ItemDisplay, Playable, PlaybackStatus};

/// Per-call timeout for short yt-dlp invocations (search / resolve).
/// Cold runs are 1-3 s in normal conditions; the ceiling here is generous
/// to absorb network jitter without locking the UI forever. The download
/// subcommand uses its own much longer timeout.
const YTDLP_TIMEOUT: Duration = Duration::from_secs(20);

/// Timeout for the download subcommand. Audio download + opus re-encode
/// typically finishes in 10-30 s; allow up to 5 minutes for slow networks.
const YTDLP_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

const SAVED_FILENAME: &str = "youtube_saved.json";
/// Legacy filename from the v0.2 prototype when the source was called
/// `ytmusic`. Renamed to `youtube_saved.json` on first load when the new
/// path is missing — keeps a user's likes intact across the rename.
const LEGACY_SAVED_FILENAME: &str = "ytmusic_saved.json";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SavedTrack {
    id: String,
    title: String,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    art_uri: Option<String>,
    #[serde(default)]
    art_uri_full: Option<String>,
}

#[derive(Debug, Deserialize)]
struct YtThumbnail {
    url: String,
    #[serde(default)]
    width: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct YtSearchRecord {
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    channel: Option<String>,
    #[serde(default)]
    uploader: Option<String>,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    thumbnails: Vec<YtThumbnail>,
}

pub struct YouTubeSource {
    mpd: Client,
    http: reqwest::Client,
    yt_dlp_bin: String,
    saved_path: PathBuf,
    saved: RwLock<Vec<SavedTrack>>,
    /// Memoized SavedTrack stubs keyed by video id. Search populates this;
    /// `save()` consults it so the persisted entry carries title / channel
    /// / art rather than appearing as a blank row in the Saved view.
    memo: RwLock<HashMap<String, SavedTrack>>,
    /// Destination for downloads. Resolved at construction: prefer the
    /// explicit `[youtube] download_dir`, then MPD `music_directory`,
    /// then the user's XDG Downloads dir, last resort `~/Downloads`.
    download_dir: PathBuf,
}

impl YouTubeSource {
    pub fn new(
        mpd: Client,
        http: reqwest::Client,
        yt_dlp_bin: String,
        data_dir: PathBuf,
        mpd_music_dir: Option<PathBuf>,
        download_dir_override: Option<PathBuf>,
    ) -> Self {
        let saved_path = data_dir.join(SAVED_FILENAME);
        let legacy = data_dir.join(LEGACY_SAVED_FILENAME);
        if !saved_path.exists() && legacy.exists() {
            if let Err(e) = std::fs::rename(&legacy, &saved_path) {
                tracing::warn!("youtube: legacy saved-file rename failed: {e}");
            }
        }
        let saved = load_saved(&saved_path);
        let download_dir = download_dir_override.or(mpd_music_dir).unwrap_or_else(|| {
            dirs::download_dir()
                .or_else(|| dirs::home_dir().map(|h| h.join("Downloads")))
                .unwrap_or_else(|| PathBuf::from("."))
        });
        Self {
            mpd,
            http,
            yt_dlp_bin,
            saved_path,
            saved: RwLock::new(saved),
            memo: RwLock::new(HashMap::new()),
            download_dir,
        }
    }

    async fn run_yt_dlp(&self, args: &[&str], deadline: Duration) -> Result<String> {
        let mut cmd = Command::new(&self.yt_dlp_bin);
        cmd.args(args).kill_on_drop(true);
        let fut = cmd.output();
        let out = timeout(deadline, fut)
            .await
            .map_err(|_| anyhow!("yt-dlp timed out after {:?}", deadline))?
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => anyhow!(
                    "yt-dlp not found at `{}` — install yt-dlp on PATH or set [youtube] yt_dlp_bin",
                    self.yt_dlp_bin
                ),
                _ => anyhow!("yt-dlp spawn: {e}"),
            })?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(classify_ytdlp_error(stderr.as_ref()));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    async fn resolve_stream_url(&self, video_id: &str) -> Result<String> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let stdout = self
            .run_yt_dlp(
                &[
                    "-f",
                    "bestaudio[ext=webm][acodec=opus]/bestaudio",
                    "--no-warnings",
                    "--no-playlist",
                    "--print",
                    "%(url)s",
                    "--",
                    url.as_str(),
                ],
                YTDLP_TIMEOUT,
            )
            .await?;
        stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(|s| s.trim().to_string())
            .ok_or_else(|| anyhow!("yt-dlp returned no stream URL for {video_id}"))
    }

    async fn write_saved(&self) -> Result<()> {
        let g = self.saved.read().await;
        let bytes = serde_json::to_vec_pretty(&*g).context("encode saved list")?;
        if let Some(parent) = self.saved_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        tokio::fs::write(&self.saved_path, bytes)
            .await
            .with_context(|| format!("write {}", self.saved_path.display()))
    }

    /// Download the track to the configured directory as an Opus file with
    /// embedded metadata + thumbnail. Returns the path written. Drives
    /// yt-dlp as a streaming process so progress percentages can be
    /// surfaced to the UI in real time.
    pub async fn do_download(&self, uri: &str, progress: Option<Arc<AtomicU8>>) -> Result<PathBuf> {
        let video_id = uri
            .strip_prefix("youtube:")
            .ok_or_else(|| anyhow!("not a youtube URI: {uri}"))?;
        if video_id.is_empty() {
            return Err(anyhow!("youtube download: empty video id"));
        }
        tokio::fs::create_dir_all(&self.download_dir)
            .await
            .with_context(|| format!("mkdir {}", self.download_dir.display()))?;
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let template = format!(
            "{}/%(artist,channel,uploader|Unknown)s - %(title)s [%(id)s].%(ext)s",
            self.download_dir.display()
        );
        // --newline forces one-line-per-progress-update so the line reader
        // never spins on an unflushed line. --progress-template gives us a
        // machine-friendly `DLPCT:nn` string so we don't have to parse the
        // human progress format.
        let mut child = Command::new(&self.yt_dlp_bin)
            .args([
                "-x",
                "--audio-format",
                "opus",
                "--audio-quality",
                "0",
                "--embed-metadata",
                "--embed-thumbnail",
                "--no-warnings",
                "--no-playlist",
                "--newline",
                "--progress-template",
                "DLPCT:%(progress.percent).0f",
                "--print",
                "after_move:filepath",
                "-o",
                template.as_str(),
                "--",
                url.as_str(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => anyhow!(
                    "yt-dlp not found at `{}` — install yt-dlp or set [youtube] yt_dlp_bin",
                    self.yt_dlp_bin
                ),
                _ => anyhow!("yt-dlp spawn: {e}"),
            })?;

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let stdout_task = tokio::spawn(async move {
            let mut last = String::new();
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if !line.trim().is_empty() {
                    last = line;
                }
            }
            last
        });

        let progress_task = {
            let progress = progress.clone();
            tokio::spawn(async move {
                let mut last_err = String::new();
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(rest) = line.strip_prefix("DLPCT:") {
                        if let Some(p) = &progress {
                            if let Ok(n) = rest.trim().parse::<f64>() {
                                p.store(n.clamp(0.0, 100.0) as u8, Ordering::Relaxed);
                            }
                        }
                    } else if line.contains("ERROR") {
                        last_err = line;
                    }
                }
                last_err
            })
        };

        let status = match timeout(YTDLP_DOWNLOAD_TIMEOUT, child.wait()).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => return Err(anyhow!("yt-dlp wait: {e}")),
            Err(_) => return Err(anyhow!("yt-dlp download timed out")),
        };
        let stdout_str = stdout_task.await.unwrap_or_default();
        let stderr_err = progress_task.await.unwrap_or_default();

        if !status.success() {
            return Err(classify_ytdlp_error(&stderr_err));
        }
        let path = stdout_str
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("yt-dlp returned no path"))?;
        Ok(path)
    }

    pub fn download_dir(&self) -> &PathBuf {
        &self.download_dir
    }

    /// One-shot metadata lookup via yt-dlp's `--dump-json`. Used to back-fill
    /// saved entries that were liked outside a search context (or written
    /// by an earlier build that didn't memoize search results).
    async fn lookup_metadata(&self, video_id: &str) -> Result<SavedTrack> {
        let url = format!("https://www.youtube.com/watch?v={video_id}");
        let stdout = self
            .run_yt_dlp(
                &[
                    "--dump-json",
                    "--skip-download",
                    "--no-warnings",
                    "--no-playlist",
                    "--flat-playlist",
                    "--",
                    url.as_str(),
                ],
                YTDLP_TIMEOUT,
            )
            .await?;
        let line = stdout
            .lines()
            .find(|l| !l.trim().is_empty())
            .ok_or_else(|| anyhow!("yt-dlp returned no metadata for {video_id}"))?;
        parse_search_record(line).context("decode yt-dlp metadata json")
    }
}

/// Parse one yt-dlp `--dump-json` JSON line into a `SavedTrack`. Pure (no I/O);
/// the fuzz target drives it on arbitrary bytes and `lookup_metadata` reuses it.
pub fn parse_search_record(line: &str) -> Result<SavedTrack> {
    let rec: YtSearchRecord = serde_json::from_str(line)?;
    Ok(record_to_saved(&rec))
}

fn load_saved(path: &PathBuf) -> Vec<SavedTrack> {
    let Ok(bytes) = std::fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice(&bytes).unwrap_or_else(|e| {
        tracing::warn!("youtube_saved.json parse failed: {e}; starting empty");
        Vec::new()
    })
}

/// Map yt-dlp stderr text to a user-actionable anyhow error. The CLI has
/// no machine codes, only English error lines, so substring-match.
fn classify_ytdlp_error(stderr: &str) -> anyhow::Error {
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("http error 429") {
        return anyhow!("YouTube rate-limited; try again in a moment");
    }
    if lower.contains("sign in to confirm your age") {
        return anyhow!("age-gated video — sign-in required (not supported in v0.2)");
    }
    if lower.contains("private video") {
        return anyhow!("video is private");
    }
    if lower.contains("video unavailable") {
        return anyhow!("video unavailable");
    }
    if lower.contains("not available in your country") || lower.contains("geo restrict") {
        return anyhow!("video not available in your region");
    }
    if lower.contains("nsig extraction failed") || lower.contains("unable to extract") {
        return anyhow!("yt-dlp signature extraction failed — run `yt-dlp -U`");
    }
    let snippet: String = stderr
        .lines()
        .find(|l| l.contains("ERROR"))
        .unwrap_or(stderr.lines().next().unwrap_or(""))
        .chars()
        .take(200)
        .collect();
    anyhow!("yt-dlp: {snippet}")
}

fn pick_thumb(thumbs: &[YtThumbnail], target_w: u32) -> Option<&YtThumbnail> {
    if thumbs.is_empty() {
        return None;
    }
    let mut best: Option<&YtThumbnail> = None;
    let mut best_score = u32::MAX;
    for t in thumbs {
        let w = t.width.unwrap_or(0);
        let score = (w as i64 - target_w as i64).unsigned_abs() as u32;
        if score < best_score {
            best_score = score;
            best = Some(t);
        }
    }
    best.or_else(|| thumbs.first())
}

fn record_to_saved(r: &YtSearchRecord) -> SavedTrack {
    let thumb_small = pick_thumb(&r.thumbnails, 120).map(|t| t.url.clone());
    let thumb_big = pick_thumb(&r.thumbnails, 1080)
        .map(|t| t.url.clone())
        .or_else(|| thumb_small.clone());
    SavedTrack {
        id: r.id.clone(),
        title: r.title.clone().unwrap_or_else(|| r.id.clone()),
        channel: r.channel.clone().or_else(|| r.uploader.clone()),
        duration_ms: r.duration.map(|s| (s * 1000.0) as u64),
        art_uri: thumb_small,
        art_uri_full: thumb_big,
    }
}

fn saved_to_item(s: &SavedTrack) -> Item {
    Item {
        uri: format!("youtube:{}", s.id),
        display: ItemDisplay {
            title: if s.title.is_empty() {
                s.id.clone()
            } else {
                s.title.clone()
            },
            artist: s.channel.clone(),
            album: None,
            art_uri: s.art_uri.clone(),
            art_uri_full: s.art_uri_full.clone().or_else(|| s.art_uri.clone()),
            duration: s.duration_ms.map(Duration::from_millis),
            sort_hint: None,
            track_no: None,
            year_hint: None,
        },
    }
}

fn saved_to_entry(s: &SavedTrack) -> Entry {
    let item = saved_to_item(s);
    Entry {
        uri: item.uri,
        label: item.display.title.clone(),
        kind: EntryKind::Track,
        display: Some(item.display),
    }
}

#[async_trait]
impl MusicSource for YouTubeSource {
    fn scheme(&self) -> &'static str {
        "youtube"
    }

    fn display_name(&self) -> &'static str {
        "YouTube"
    }

    async fn search(&self, query: &str) -> Result<Vec<Item>> {
        if query.trim().is_empty() {
            return Ok(Vec::new());
        }
        let q = format!("ytsearch20:{query}");
        let stdout = self
            .run_yt_dlp(
                &[
                    "--flat-playlist",
                    "--dump-json",
                    "--no-warnings",
                    "--",
                    q.as_str(),
                ],
                YTDLP_TIMEOUT,
            )
            .await?;
        let mut items = Vec::new();
        let mut memo_writes: Vec<SavedTrack> = Vec::new();
        for line in stdout.lines() {
            let l = line.trim();
            if l.is_empty() {
                continue;
            }
            match serde_json::from_str::<YtSearchRecord>(l) {
                Ok(rec) => {
                    let st = record_to_saved(&rec);
                    items.push(saved_to_item(&st));
                    memo_writes.push(st);
                }
                Err(e) => tracing::warn!("ytsearch record skipped: {e}"),
            }
        }
        if !memo_writes.is_empty() {
            let mut g = self.memo.write().await;
            for st in memo_writes {
                g.insert(st.id.clone(), st);
            }
        }
        Ok(items)
    }

    async fn browse(&self, path: &str) -> Result<Vec<Entry>> {
        match path {
            "" | "youtube:" | "youtube:landing" | "youtube:saved" => {
                // Hydrate entries whose title is empty (legacy saves from
                // before the search-time memo landed, or saves done via
                // IPC where no search ran). One yt-dlp `--print` lookup
                // per blank id; cached back to disk on success so a
                // future browse is instant.
                let blank_ids: Vec<String> = {
                    let g = self.saved.read().await;
                    g.iter()
                        .filter(|s| s.title.is_empty())
                        .map(|s| s.id.clone())
                        .collect()
                };
                if !blank_ids.is_empty() {
                    let mut updates: Vec<SavedTrack> = Vec::new();
                    for id in blank_ids {
                        if let Ok(t) = self.lookup_metadata(&id).await {
                            updates.push(t);
                        }
                    }
                    if !updates.is_empty() {
                        let mut g = self.saved.write().await;
                        for t in updates {
                            if let Some(slot) = g.iter_mut().find(|s| s.id == t.id) {
                                *slot = t;
                            }
                        }
                        drop(g);
                        let _ = self.write_saved().await;
                    }
                }
                let g = self.saved.read().await;
                Ok(g.iter().map(saved_to_entry).collect())
            }
            _ => Ok(Vec::new()),
        }
    }

    async fn resolve(&self, uri: &str) -> Result<Playable> {
        let video_id = uri
            .strip_prefix("youtube:")
            .ok_or_else(|| anyhow!("not a youtube URI: {uri}"))?;
        if video_id.is_empty() || video_id.contains(':') {
            return Err(anyhow!("youtube resolve: bad video id `{video_id}`"));
        }
        let stream = self.resolve_stream_url(video_id).await?;
        Ok(Playable::Url(stream))
    }

    async fn play(&self, playable: &Playable) -> Result<()> {
        let url = match playable {
            Playable::Url(u) | Playable::LibraryUri(u) => u.as_str(),
        };
        self.mpd
            .command(commands::ClearQueue)
            .await
            .context("MPD clear")?;
        let _ = self
            .mpd
            .command(commands::Add::uri(url))
            .await
            .context("MPD add")?;
        self.mpd
            .command(commands::Play::current())
            .await
            .context("MPD play")?;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.mpd.command(commands::Stop).await.context("MPD stop")?;
        Ok(())
    }

    async fn pause(&self) -> Result<()> {
        self.mpd
            .command(commands::SetPause(true))
            .await
            .context("MPD pause")?;
        Ok(())
    }

    async fn resume(&self) -> Result<()> {
        self.mpd
            .command(commands::SetPause(false))
            .await
            .context("MPD resume")?;
        Ok(())
    }

    async fn playback_status(&self) -> Result<Option<PlaybackStatus>> {
        Ok(Some(mpd_status(&self.mpd).await?))
    }

    async fn set_volume(&self, vol: u8) -> Result<()> {
        mpd_set_volume(&self.mpd, vol).await
    }

    async fn seek(&self, position: Duration) -> Result<()> {
        // MPD seeks the active song. googlevideo URLs support HTTP Range,
        // so MPD's curl input can re-position into the stream. Expect a
        // brief stall (1-3 s) while MPD re-buffers.
        self.mpd
            .command(commands::Seek(commands::SeekMode::Absolute(position)))
            .await
            .context("MPD seek")?;
        Ok(())
    }

    async fn art(&self, uri: &str, _size: ArtSize) -> Result<Vec<u8>> {
        // Search results store the raw i.ytimg.com thumbnail URL directly
        // in `art_uri`. The thumb_list widget routes the fetch back here
        // via source_scheme; we fetch the URL verbatim — i.ytimg.com is
        // public, no auth, no signing.
        if uri.starts_with("http://") || uri.starts_with("https://") {
            let bytes = self
                .http
                .get(uri)
                .send()
                .await
                .with_context(|| format!("GET {uri}"))?
                .error_for_status()?
                .bytes()
                .await
                .context("read thumb body")?;
            return Ok(bytes.to_vec());
        }
        Err(anyhow!("no cached art URL for {uri}"))
    }

    async fn is_saved(&self, uri: &str) -> Result<bool> {
        let id = uri.strip_prefix("youtube:").unwrap_or(uri);
        let g = self.saved.read().await;
        Ok(g.iter().any(|s| s.id == id))
    }

    async fn save(&self, uri: &str) -> Result<()> {
        let id = uri
            .strip_prefix("youtube:")
            .ok_or_else(|| anyhow!("not a youtube URI: {uri}"))?
            .to_string();
        // Enrich from the search-result memo if we have it; fall back to a
        // bare stub so saving still works for URIs we haven't seen.
        let mut stub = self
            .memo
            .read()
            .await
            .get(&id)
            .cloned()
            .unwrap_or_else(|| SavedTrack {
                id: id.clone(),
                ..Default::default()
            });
        if stub.id.is_empty() {
            stub.id = id.clone();
        }
        {
            let mut g = self.saved.write().await;
            if g.iter().any(|s| s.id == id) {
                return Ok(());
            }
            g.push(stub);
        }
        self.write_saved().await
    }

    async fn unsave(&self, uri: &str) -> Result<()> {
        let id = uri.strip_prefix("youtube:").unwrap_or(uri).to_string();
        {
            let mut g = self.saved.write().await;
            let before = g.len();
            g.retain(|s| s.id != id);
            if g.len() == before {
                return Ok(());
            }
        }
        self.write_saved().await
    }

    async fn download(&self, uri: &str, progress: Option<Arc<AtomicU8>>) -> Result<PathBuf> {
        self.do_download(uri, progress).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_known_errors() {
        assert!(
            classify_ytdlp_error("HTTP Error 429: Too Many Requests")
                .to_string()
                .contains("rate-limited")
        );
        assert!(
            classify_ytdlp_error("ERROR: Private video")
                .to_string()
                .contains("private")
        );
        assert!(
            classify_ytdlp_error("ERROR: nsig extraction failed")
                .to_string()
                .contains("yt-dlp -U")
        );
    }

    #[test]
    fn picks_closest_thumb_width() {
        let thumbs = vec![
            YtThumbnail {
                url: "a".into(),
                width: Some(48),
            },
            YtThumbnail {
                url: "b".into(),
                width: Some(120),
            },
            YtThumbnail {
                url: "c".into(),
                width: Some(1280),
            },
        ];
        assert_eq!(pick_thumb(&thumbs, 120).map(|t| t.url.as_str()), Some("b"));
        assert_eq!(pick_thumb(&thumbs, 1080).map(|t| t.url.as_str()), Some("c"));
    }
}
