#![allow(dead_code)]

pub mod local;
pub mod mpd_shared;
pub mod radio;
pub mod somafm;
pub mod spotify;
pub mod youtube;

use anyhow::Result;
use async_trait::async_trait;

use crate::types::{ArtSize, DeviceEntry, Entry, Item, Playable, PlaybackStatus};

#[async_trait]
pub trait MusicSource: Send + Sync {
    fn scheme(&self) -> &'static str;
    fn display_name(&self) -> &'static str;

    async fn search(&self, query: &str) -> Result<Vec<Item>>;
    async fn browse(&self, path: &str) -> Result<Vec<Entry>>;
    /// Stream rows in batches via `tx` as they become available. Default
    /// impl awaits `browse()` and sends one batch — sources that paginate
    /// over multiple network round-trips (currently Spotify saved_albums)
    /// override this to flush per page so the first page can render while
    /// later pages are still in flight. Mid-stream errors land in the
    /// channel as `Err(_)` and stop the stream. The implementation drops
    /// `tx` on return so the consumer's `recv()` returns `None`.
    async fn browse_streaming(
        &self,
        path: &str,
        tx: tokio::sync::mpsc::Sender<Result<Vec<Entry>>>,
    ) {
        let _ = tx.send(self.browse(path).await).await;
    }
    async fn resolve(&self, uri: &str) -> Result<Playable>;

    async fn play(&self, playable: &Playable) -> Result<()>;
    async fn stop(&self) -> Result<()>;

    async fn pause(&self) -> Result<()> {
        Err(anyhow::anyhow!("pause not supported"))
    }
    async fn resume(&self) -> Result<()> {
        Err(anyhow::anyhow!("resume not supported"))
    }

    /// Current playback state. `None` means the source has nothing to report
    /// (e.g. it isn't the active source). Polled every UI tick — return Ok(None)
    /// rather than Err for sources without playback semantics.
    async fn playback_status(&self) -> Result<Option<PlaybackStatus>> {
        Ok(None)
    }

    /// Set the source's playback volume (0..=100). Default no-op so unsupported
    /// sources don't error.
    async fn set_volume(&self, _vol: u8) -> Result<()> {
        Ok(())
    }

    /// Art bytes (decoded later by ArtCache).
    async fn art(&self, _uri: &str, _size: ArtSize) -> Result<Vec<u8>> {
        Err(anyhow::anyhow!("art not supported"))
    }

    /// Is this URI in the user's library / liked? Default: false.
    async fn is_saved(&self, _uri: &str) -> Result<bool> {
        Ok(false)
    }
    /// Add to saved. Default: no-op.
    async fn save(&self, _uri: &str) -> Result<()> {
        Ok(())
    }
    /// Remove from saved. Default: no-op.
    async fn unsave(&self, _uri: &str) -> Result<()> {
        Ok(())
    }

    /// Seek the current track to an absolute position. Default: error.
    async fn seek(&self, _position: std::time::Duration) -> Result<()> {
        Err(anyhow::anyhow!("seek not supported"))
    }

    /// List Spotify-Connect-style playback targets. Default: empty (most
    /// sources have a single hardware output and nothing to pick from).
    async fn list_devices(&self) -> Result<Vec<DeviceEntry>> {
        Ok(Vec::new())
    }

    /// Transfer playback to the named device. Default: no-op.
    async fn transfer_to_device(&self, _device_id: &str) -> Result<()> {
        Ok(())
    }

    /// Add a track URI to a user-owned playlist URI. Default: not supported.
    /// Currently only Spotify implements this; local / radio sources error.
    async fn add_to_playlist(&self, _playlist_uri: &str, _track_uri: &str) -> Result<()> {
        Err(anyhow::anyhow!("add_to_playlist not supported"))
    }

    /// Remove a track URI from a user-owned playlist URI. Default: not
    /// supported.
    async fn remove_from_playlist(&self, _playlist_uri: &str, _track_uri: &str) -> Result<()> {
        Err(anyhow::anyhow!("remove_from_playlist not supported"))
    }

    /// Resolve a track URI to a related entity URI (`"album"` or
    /// `"artist"`). Used by "Go to album/artist" in the action menu.
    /// Default: not supported.
    async fn relation_uri(&self, _track_uri: &str, _kind: &str) -> Result<String> {
        Err(anyhow::anyhow!("relation_uri not supported"))
    }

    /// Download the track to local disk with embedded metadata. Returns
    /// the path written. Only YouTube implements this today; other sources
    /// either have nothing to download (radio / somafm) or already live on
    /// disk (local) / require a different transport (spotify).
    ///
    /// `progress` is an optional shared 0..=100 percentage slot; the
    /// implementation may update it as the download runs. `255` is the
    /// sentinel for "no download active" — callers store that on entry
    /// and after completion to signal done.
    async fn download(
        &self,
        _uri: &str,
        _progress: Option<std::sync::Arc<std::sync::atomic::AtomicU8>>,
    ) -> Result<std::path::PathBuf> {
        Err(anyhow::anyhow!("download not supported"))
    }

    /// Embedded lyrics carried in the track's own metadata, if any (raw blob,
    /// LRC-timestamped or plain). Only local files realistically have these;
    /// the lyrics layer prefers them over the lrclib network lookup. Default:
    /// none.
    async fn embedded_lyrics(&self, _uri: &str) -> Result<Option<String>> {
        Ok(None)
    }
}

/// Routes art fetches to the source that owns the URI's scheme.
pub async fn fetch_art_via_dispatcher(
    dispatcher: &crate::dispatch::Dispatcher,
    uri: &str,
    size: ArtSize,
) -> Result<Vec<u8>> {
    let scheme = uri.split(':').next().unwrap_or("");
    let src = dispatcher
        .get(scheme)
        .ok_or_else(|| anyhow::anyhow!("no source for scheme {scheme}"))?;
    src.art(uri, size).await
}
