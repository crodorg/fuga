use anyhow::{Context, Result};
use mpd_client::{client::Client, commands, responses};

use crate::types::{PlayState, PlaybackStatus};

/// Query MPD for current playback status. Used by every MPD-backed source
/// (LocalSource, RadioSource, SomaFmSource) so we don't duplicate the
/// command + field-mapping boilerplate.
pub async fn mpd_status(client: &Client) -> Result<PlaybackStatus> {
    // Batch Status + CurrentSong into a single command-list round-trip rather
    // than two sequential commands — halves the per-poll socket traffic (and
    // the mpd_client idle/noidle re-arm cycle) with identical results. Both are
    // still fetched every poll, so live ICY stream titles (radio/somafm) keep
    // updating exactly as before.
    let (s, current) = client
        .command_list((commands::Status, commands::CurrentSong))
        .await
        .context("MPD status")?;
    // Capture both codec (from URL extension) and live stream title (MPD
    // surfaces ICY StreamTitle via the Title tag for HTTP streams).
    let (codec, stream_title) = match current {
        Some(cs) => (
            codec_from_url(&cs.song.url),
            cs.song.title().map(str::to_owned),
        ),
        None => (None, None),
    };
    Ok(PlaybackStatus {
        elapsed: s.elapsed.unwrap_or_default(),
        duration: s.duration,
        volume: s.volume,
        state: match s.state {
            responses::PlayState::Playing => PlayState::Playing,
            responses::PlayState::Paused => PlayState::Paused,
            responses::PlayState::Stopped => PlayState::Stopped,
        },
        codec,
        bitrate_kbps: s.bitrate.map(|b| b as u32),
        stream_title,
    })
}

/// Heuristic codec label from the song's library URL (extension) or stream
/// URL. MPD's `audio` field on Status is sample rate / depth / channels — not
/// the container — so we lean on the URL instead.
fn codec_from_url(url: &str) -> Option<String> {
    let lower = url.to_ascii_lowercase();
    let trimmed = lower.split('?').next().unwrap_or(&lower);
    let ext = std::path::Path::new(trimmed)
        .extension()
        .and_then(|s| s.to_str())?;
    Some(match ext {
        "flac" => "FLAC".into(),
        "mp3" => "MP3".into(),
        "ogg" | "oga" => "OGG".into(),
        "opus" => "OPUS".into(),
        "m4a" | "aac" | "mp4" => "AAC".into(),
        "wav" => "WAV".into(),
        "wv" => "WV".into(),
        "ape" => "APE".into(),
        "alac" => "ALAC".into(),
        "wma" => "WMA".into(),
        "pls" | "m3u" | "m3u8" => return None,
        other => other.to_uppercase(),
    })
}

pub async fn mpd_set_volume(client: &Client, vol: u8) -> Result<()> {
    client
        .command(commands::SetVolume(vol.min(100)))
        .await
        .context("MPD setvol")?;
    Ok(())
}
