use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use mpd_client::{client::Client, commands};
use serde::Deserialize;
use tokio::sync::RwLock;

use crate::source::MusicSource;
use crate::source::mpd_shared::{mpd_set_volume, mpd_status};
use crate::source::radio::resolve_playlist;
use crate::types::{ArtSize, Entry, EntryKind, Item, ItemDisplay, Playable, PlaybackStatus};

const CHANNELS_URL: &str = "https://api.somafm.com/channels.json";
const CACHE_FILENAME: &str = "somafm_channels.json";

#[derive(Debug, Clone, Deserialize)]
pub struct ChannelsRoot {
    pub channels: Vec<Channel>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub dj: String,
    #[serde(default)]
    pub genre: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub largeimage: Option<String>,
    #[serde(default)]
    pub xlimage: Option<String>,
    #[serde(default)]
    pub playlists: Vec<Playlist>,
    #[serde(default)]
    pub listeners: Option<String>,
    #[serde(default, rename = "lastPlaying")]
    pub last_playing: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Playlist {
    pub url: String,
    #[serde(default)]
    pub format: String,
    #[serde(default)]
    pub quality: String,
}

/// Parse the SomaFM `channels.json` body into the channel list. Pure (no I/O)
/// so the fuzz target and tests can drive it on arbitrary input.
pub fn parse_channels(raw: &str) -> Result<Vec<Channel>> {
    let parsed: ChannelsRoot = serde_json::from_str(raw).context("parse channels.json")?;
    Ok(parsed.channels)
}

pub struct SomaFmSource {
    cache_path: PathBuf,
    cache_ttl: Duration,
    channels: RwLock<Vec<Channel>>,
    mpd: Client,
    http: reqwest::Client,
}

impl SomaFmSource {
    pub fn new(
        cache_dir: PathBuf,
        cache_ttl_hours: u64,
        mpd: Client,
        http: reqwest::Client,
    ) -> Self {
        Self {
            cache_path: cache_dir.join(CACHE_FILENAME),
            cache_ttl: Duration::from_secs(cache_ttl_hours * 3600),
            channels: RwLock::new(Vec::new()),
            mpd,
            http,
        }
    }

    /// Loads channels from disk if fresh, otherwise fetches and caches.
    pub async fn ensure_channels(&self) -> Result<()> {
        if !self.channels.read().await.is_empty() {
            return Ok(());
        }
        let fresh_disk = self.read_disk_if_fresh().await?;
        let raw = match fresh_disk {
            Some(s) => s,
            None => self.fetch_and_persist().await?,
        };
        *self.channels.write().await = parse_channels(&raw)?;
        Ok(())
    }

    async fn read_disk_if_fresh(&self) -> Result<Option<String>> {
        let path = &self.cache_path;
        if !path.exists() {
            return Ok(None);
        }
        let meta = tokio::fs::metadata(path).await.ok();
        if let Some(meta) = meta {
            if let Ok(modified) = meta.modified() {
                if let Ok(age) = SystemTime::now().duration_since(modified) {
                    if age <= self.cache_ttl {
                        return Ok(tokio::fs::read_to_string(path).await.ok());
                    }
                }
            }
        }
        Ok(None)
    }

    async fn fetch_and_persist(&self) -> Result<String> {
        let body = self
            .http
            .get(CHANNELS_URL)
            .send()
            .await
            .context("GET channels.json")?
            .error_for_status()?
            .text()
            .await
            .context("read channels.json body")?;
        if let Some(parent) = self.cache_path.parent() {
            tokio::fs::create_dir_all(parent).await.ok();
        }
        if let Err(e) = tokio::fs::write(&self.cache_path, &body).await {
            tracing::warn!(?self.cache_path, "somafm cache write failed: {e}");
        }
        Ok(body)
    }

    fn channel_for_uri<'a>(&self, channels: &'a [Channel], uri: &str) -> Option<&'a Channel> {
        // Strip the scheme prefix, then any size discriminator. URI shapes:
        //   somafm:<id>             — play / resolve / generic art lookup
        //   somafm:thumb:<id>       — small icon (row thumb)
        //   somafm:full:<id>        — full-size image (now-playing pane)
        let after_scheme = uri.strip_prefix("somafm:").unwrap_or(uri);
        let id = after_scheme
            .strip_prefix("thumb:")
            .or_else(|| after_scheme.strip_prefix("full:"))
            .unwrap_or(after_scheme);
        channels.iter().find(|c| c.id == id)
    }
}

fn channel_to_item(c: &Channel) -> Item {
    // `art_uri` (rows) and `art_uri_full` (now-playing pane) double as cache
    // keys. The `ArtCache` is keyed by URI alone, so identical art URIs
    // collapse into a single cache entry — whichever fetch lands first wins,
    // which made the now-playing pane stuck at the small ~120px icon when
    // the row thumb fetched first. Splitting on `thumb:` / `full:` gives the
    // pane its own cache slot for the xlimage.
    let play_uri = format!("somafm:{}", c.id);
    let thumb_uri = format!("somafm:thumb:{}", c.id);
    let full_uri = format!("somafm:full:{}", c.id);
    Item {
        uri: play_uri,
        display: ItemDisplay {
            title: c.title.clone(),
            artist: (!c.dj.is_empty()).then(|| c.dj.clone()),
            album: (!c.genre.is_empty()).then(|| c.genre.clone()),
            art_uri: Some(thumb_uri),
            art_uri_full: Some(full_uri),
            duration: None,
            sort_hint: None,
            track_no: None,
            year_hint: None,
        },
    }
}

/// Pick the highest-quality MP3 playlist URL.
fn best_mp3_playlist(c: &Channel) -> Option<&str> {
    let mp3: Vec<&Playlist> = c
        .playlists
        .iter()
        .filter(|p| p.format.eq_ignore_ascii_case("mp3"))
        .collect();
    let pick = quality_rank(&mp3, "highest")
        .or_else(|| quality_rank(&mp3, "high"))
        .or_else(|| mp3.first().copied())
        .or_else(|| c.playlists.first());
    pick.map(|p| p.url.as_str())
}

fn quality_rank<'b>(playlists: &[&'b Playlist], quality: &str) -> Option<&'b Playlist> {
    playlists
        .iter()
        .copied()
        .find(|p| p.quality.eq_ignore_ascii_case(quality))
}

#[async_trait]
impl MusicSource for SomaFmSource {
    fn scheme(&self) -> &'static str {
        "somafm"
    }

    fn display_name(&self) -> &'static str {
        "SomaFM"
    }

    async fn search(&self, query: &str) -> Result<Vec<Item>> {
        self.ensure_channels().await?;
        let q = query.to_ascii_lowercase();
        let g = self.channels.read().await;
        Ok(g.iter()
            .filter(|c| {
                c.title.to_ascii_lowercase().contains(&q)
                    || c.genre.to_ascii_lowercase().contains(&q)
                    || c.description.to_ascii_lowercase().contains(&q)
            })
            .map(channel_to_item)
            .collect())
    }

    async fn browse(&self, _path: &str) -> Result<Vec<Entry>> {
        self.ensure_channels().await?;
        let g = self.channels.read().await;
        Ok(g.iter()
            .map(|c| Entry {
                uri: format!("somafm:{}", c.id),
                label: format!("{} — {}", c.title, c.genre),
                kind: EntryKind::Track,
                display: Some(channel_to_item(c).display),
            })
            .collect())
    }

    async fn resolve(&self, uri: &str) -> Result<Playable> {
        self.ensure_channels().await?;
        let g = self.channels.read().await;
        let chan = self
            .channel_for_uri(&g, uri)
            .ok_or_else(|| anyhow!("unknown SomaFM channel: {uri}"))?;
        let pls_url = best_mp3_playlist(chan)
            .ok_or_else(|| anyhow!("no playlists for channel {}", chan.id))?
            .to_string();
        drop(g);
        let stream = resolve_playlist(&self.http, &pls_url).await?;
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
            .context("MPD addid")?;
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

    async fn art(&self, uri: &str, size: ArtSize) -> Result<Vec<u8>> {
        self.ensure_channels().await?;
        let g = self.channels.read().await;
        let chan = self
            .channel_for_uri(&g, uri)
            .ok_or_else(|| anyhow!("unknown SomaFM channel: {uri}"))?;
        // URI prefix overrides the ArtSize hint when present. This lets the
        // cache hold separate entries per size (the cache is URI-keyed and
        // doesn't track ArtSize itself).
        let after_scheme = uri.strip_prefix("somafm:").unwrap_or(uri);
        let effective = if after_scheme.starts_with("thumb:") {
            ArtSize::Thumb
        } else if after_scheme.starts_with("full:") {
            ArtSize::Full
        } else {
            size
        };
        let url = match effective {
            ArtSize::Thumb => chan
                .image
                .clone()
                .or_else(|| chan.largeimage.clone())
                .or_else(|| chan.xlimage.clone()),
            ArtSize::Medium => chan
                .largeimage
                .clone()
                .or_else(|| chan.xlimage.clone())
                .or_else(|| chan.image.clone()),
            ArtSize::Full => chan
                .xlimage
                .clone()
                .or_else(|| chan.largeimage.clone())
                .or_else(|| chan.image.clone()),
        }
        .ok_or_else(|| anyhow!("no image for channel {}", chan.id))?;
        drop(g);
        let bytes = self
            .http
            .get(&url)
            .send()
            .await
            .with_context(|| format!("GET {url}"))?
            .error_for_status()?
            .bytes()
            .await
            .context("read art body")?;
        Ok(bytes.to_vec())
    }
}
