#![allow(dead_code)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use librespot::connect::{ConnectConfig, Spirc};
use librespot::core::authentication::Credentials;
use librespot::core::config::{DeviceType, SessionConfig};
use librespot::core::session::Session;
use librespot::core::spotify_id::SpotifyId;
use librespot::core::SpotifyUri;
use librespot::playback::audio_backend;
use librespot::playback::config::{AudioFormat, Bitrate, PlayerConfig};
use librespot::playback::mixer::softmixer::SoftMixer;
use librespot::playback::mixer::{Mixer, MixerConfig};
use librespot::playback::player::{Player, PlayerEvent};
use tokio::sync::mpsc::UnboundedSender;
use tokio::sync::{Mutex, Notify};

use crate::config::SpotifyConfig;
use crate::types::{PlayState, PlaybackStatus};

/// Forwarded from the librespot player into the App so the queue can advance.
#[derive(Debug, Clone)]
pub enum SpotifyEvent {
    EndOfTrack,
    Stopped,
    Loading,
    Playing,
    Paused,
    Error(String),
}

/// Snapshot of librespot playback position. Updated whenever librespot fires
/// a Playing/Paused/Seeked/PositionCorrection event; read by `playback_status`
/// to compute current elapsed-ms from the wall clock.
#[derive(Default, Clone, Copy)]
pub struct PositionAnchor {
    /// Set when state == Playing; the wall-clock instant we anchored at.
    pub anchored_at: Option<Instant>,
    /// Position in ms at the moment of anchor (or last snapshot when paused).
    pub offset_ms: u32,
}

pub struct SpotifyPlayer {
    pub player: Arc<Player>,
    pub mixer: Arc<dyn Mixer>,
    /// Configured stream bitrate (kbps). Reported in `playback_status`.
    pub bitrate_kbps: u32,
    /// Notified by the events_task when librespot reaches a terminal state
    /// (Stopped, EndOfTrack, Unavailable). `stop_and_wait` listens for this
    /// to confirm audio output has actually halted before returning, so the
    /// dispatcher can switch sources without overlapping audio.
    pub stopped_signal: Arc<Notify>,
    pub position: Arc<Mutex<PositionAnchor>>,
    pub duration_ms: Arc<Mutex<Option<u32>>>,
    pub state: Arc<Mutex<PlayState>>,
    /// The most recently loaded playable (kind + base62 id). Lets the source
    /// reload the same track after rebuilding a dead session (idle keepalive
    /// timeout) so a paused track resumes where it left off.
    pub current: std::sync::Mutex<Option<(PlayableKind, String)>>,
    pub session: Session,
    /// Spotify Connect handle. Holding this advertises fuga as a Connect
    /// device on the user's account; phones/desktops see it in their device
    /// list and can transfer playback to/from it. When another device takes
    /// over, librespot yields cleanly instead of fighting for output.
    pub spirc: Option<Spirc>,
    _events_task: tokio::task::JoinHandle<()>,
    _spirc_task: tokio::task::JoinHandle<()>,
}

impl SpotifyPlayer {
    pub async fn connect(
        access_token: &str,
        config: &SpotifyConfig,
        events: UnboundedSender<SpotifyEvent>,
    ) -> Result<Self> {
        let session_config = SessionConfig {
            device_id: device_id(&config.device_name),
            ..SessionConfig::default()
        };
        let session = Session::new(session_config, None);
        let credentials = Credentials::with_access_token(access_token);

        let bitrate = parse_bitrate(config.resolved_bitrate());
        let player_config = PlayerConfig {
            bitrate,
            normalisation: config.volume_normalisation,
            ..PlayerConfig::default()
        };

        let audio_format = AudioFormat::default();
        let backend = audio_backend::find(None).ok_or_else(|| {
            anyhow!("no audio backend (compile with rodio-backend or pulseaudio-backend)")
        })?;

        // SoftMixer means volume changes attenuate the audio stream in software,
        // so the master volume slider works on Spotify the same way it does on
        // MPD. NoOpVolume (the prior wiring) ignored the volume setting.
        let mixer: Arc<dyn Mixer> = Arc::new(
            SoftMixer::open(MixerConfig::default())
                .map_err(|e| anyhow!("librespot SoftMixer open: {e}"))?,
        );

        let player = Player::new(
            player_config,
            session.clone(),
            mixer.get_soft_volume(),
            move || backend(None, audio_format),
        );

        // Spirc::new authenticates the session AND registers fuga as a
        // Spotify Connect device. Must come after Player construction
        // because Spirc needs the player handle.
        let connect_config = ConnectConfig {
            name: config.device_name.clone(),
            device_type: DeviceType::Computer,
            ..ConnectConfig::default()
        };
        let (spirc, spirc_future) = Spirc::new(
            connect_config,
            session.clone(),
            credentials,
            player.clone(),
            mixer.clone(),
        )
        .await
        .map_err(|e| anyhow!("librespot Spirc init: {e}"))?;
        let spirc_task = tokio::spawn(async move {
            spirc_future.await;
        });

        let stopped_signal = Arc::new(Notify::new());
        let position = Arc::new(Mutex::new(PositionAnchor::default()));
        let duration_ms: Arc<Mutex<Option<u32>>> = Arc::new(Mutex::new(None));
        let state = Arc::new(Mutex::new(PlayState::Stopped));

        let stopped_for_task = stopped_signal.clone();
        let position_for_task = position.clone();
        let duration_for_task = duration_ms.clone();
        let state_for_task = state.clone();

        let mut event_rx = player.get_player_event_channel();
        let events_task = tokio::spawn(async move {
            while let Some(ev) = event_rx.recv().await {
                // Wake any stop_and_wait waiter when audio is actually halted.
                if matches!(
                    ev,
                    PlayerEvent::Stopped { .. }
                        | PlayerEvent::EndOfTrack { .. }
                        | PlayerEvent::Unavailable { .. }
                ) {
                    stopped_for_task.notify_waiters();
                }
                // Maintain position anchor + state so playback_status can
                // compute elapsed-ms without polling librespot every tick.
                match &ev {
                    PlayerEvent::TrackChanged { audio_item } => {
                        *duration_for_task.lock().await = Some(audio_item.duration_ms);
                    }
                    PlayerEvent::Playing { position_ms, .. }
                    | PlayerEvent::PositionCorrection { position_ms, .. }
                    | PlayerEvent::PositionChanged { position_ms, .. }
                    | PlayerEvent::Seeked { position_ms, .. } => {
                        *position_for_task.lock().await = PositionAnchor {
                            anchored_at: Some(Instant::now()),
                            offset_ms: *position_ms,
                        };
                        *state_for_task.lock().await = PlayState::Playing;
                    }
                    PlayerEvent::Paused { position_ms, .. } => {
                        *position_for_task.lock().await = PositionAnchor {
                            anchored_at: None,
                            offset_ms: *position_ms,
                        };
                        *state_for_task.lock().await = PlayState::Paused;
                    }
                    PlayerEvent::Stopped { .. }
                    | PlayerEvent::EndOfTrack { .. }
                    | PlayerEvent::Unavailable { .. } => {
                        *position_for_task.lock().await = PositionAnchor::default();
                        *state_for_task.lock().await = PlayState::Stopped;
                    }
                    _ => {}
                }
                let mapped = match ev {
                    PlayerEvent::EndOfTrack { .. } => Some(SpotifyEvent::EndOfTrack),
                    PlayerEvent::Stopped { .. } => Some(SpotifyEvent::Stopped),
                    PlayerEvent::Loading { .. } => Some(SpotifyEvent::Loading),
                    PlayerEvent::Playing { .. } => Some(SpotifyEvent::Playing),
                    PlayerEvent::Paused { .. } => Some(SpotifyEvent::Paused),
                    PlayerEvent::Unavailable { .. } => {
                        Some(SpotifyEvent::Error("track unavailable".into()))
                    }
                    _ => None,
                };
                if let Some(e) = mapped {
                    if events.send(e).is_err() {
                        break;
                    }
                }
            }
        });

        Ok(Self {
            player,
            mixer,
            bitrate_kbps: bitrate_kbps_from(bitrate),
            stopped_signal,
            position,
            duration_ms,
            state,
            current: std::sync::Mutex::new(None),
            session,
            spirc: Some(spirc),
            _events_task: events_task,
            _spirc_task: spirc_task,
        })
    }

    pub fn load_track(&self, base62_id: &str) -> Result<()> {
        self.load_at(PlayableKind::Track, base62_id, 0)
    }

    pub fn load_episode(&self, base62_id: &str) -> Result<()> {
        self.load_at(PlayableKind::Episode, base62_id, 0)
    }

    /// Load a playable at a specific position and start playing. Records it as
    /// the current playable so a later session rebuild can resume the same
    /// track. `position_ms == 0` is the normal start-from-top case.
    pub fn load_at(&self, kind: PlayableKind, base62_id: &str, position_ms: u32) -> Result<()> {
        let id = SpotifyId::from_base62(base62_id)
            .map_err(|e| anyhow!("parse spotify id `{base62_id}`: {e}"))?;
        let uri = match kind {
            PlayableKind::Track => SpotifyUri::Track { id },
            PlayableKind::Episode => SpotifyUri::Episode { id },
        };
        self.player.load(uri, true, position_ms);
        if let Ok(mut g) = self.current.lock() {
            *g = Some((kind, base62_id.to_string()));
        }
        Ok(())
    }

    /// The playable currently loaded, if any.
    pub fn current(&self) -> Option<(PlayableKind, String)> {
        self.current.lock().ok().and_then(|g| g.clone())
    }

    pub fn pause(&self) {
        self.player.pause();
    }

    pub fn resume(&self) {
        self.player.play();
    }

    pub fn stop(&self) {
        self.player.stop();
    }

    pub fn seek(&self, position_ms: u32) {
        self.player.seek(position_ms);
    }

    /// Stop the player and wait for librespot to confirm audio output has
    /// halted (or the timeout fires). Pin the `notified()` future *before*
    /// firing the stop command so a fast Stopped event can't race past.
    pub async fn stop_and_wait(&self, timeout: Duration) {
        let notified = self.stopped_signal.notified();
        tokio::pin!(notified);
        self.player.stop();
        tokio::select! {
            _ = &mut notified => {}
            _ = tokio::time::sleep(timeout) => {
                tracing::warn!("librespot stop timed out after {:?}", timeout);
            }
        }
    }

    pub fn set_volume(&self, vol: u8) {
        // Map 0..=100 → 0..=u16::MAX. SoftMixer takes u16 across full range.
        let scaled = (vol.min(100) as u32 * u16::MAX as u32 / 100) as u16;
        self.mixer.set_volume(scaled);
    }

    pub async fn playback_status(&self) -> PlaybackStatus {
        let state = *self.state.lock().await;
        let pos = *self.position.lock().await;
        let dur = *self.duration_ms.lock().await;
        let elapsed_ms = match pos.anchored_at {
            Some(t) => pos
                .offset_ms
                .saturating_add(t.elapsed().as_millis().min(u32::MAX as u128) as u32),
            None => pos.offset_ms,
        };
        let elapsed_ms = match dur {
            Some(d) => elapsed_ms.min(d),
            None => elapsed_ms,
        };
        // Read the configured volume back out so the bottom bar stays in sync
        // even if Spotify Connect changes it externally.
        let raw = self.mixer.volume();
        let vol = (raw as u32 * 100 / u16::MAX as u32) as u8;
        PlaybackStatus {
            elapsed: Duration::from_millis(elapsed_ms as u64),
            duration: dur.map(|d| Duration::from_millis(d as u64)),
            volume: vol,
            state,
            codec: Some("OGG".into()),
            bitrate_kbps: Some(self.bitrate_kbps),
            stream_title: None,
        }
    }
}

fn parse_bitrate(s: &str) -> Bitrate {
    match s {
        "96" => Bitrate::Bitrate96,
        "160" => Bitrate::Bitrate160,
        _ => Bitrate::Bitrate320,
    }
}

fn bitrate_kbps_from(b: Bitrate) -> u32 {
    match b {
        Bitrate::Bitrate96 => 96,
        Bitrate::Bitrate160 => 160,
        Bitrate::Bitrate320 => 320,
    }
}

/// Stable device id from the configured device name (sha1 hex like spotifyd).
fn device_id(name: &str) -> String {
    use sha2::{Digest, Sha256};
    let h = Sha256::digest(name.as_bytes());
    h.iter().map(|b| format!("{b:02x}")).collect()
}

/// Kind of Spotify thing playable via librespot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayableKind {
    Track,
    Episode,
}

/// Parse `spotify:track:<id>` / `spotify:episode:<id>` (or just `<id>`,
/// assumed Track) to (kind, base62).
pub fn parse_playable_uri(uri: &str) -> Result<(PlayableKind, &str)> {
    if let Some(rest) = uri.strip_prefix("spotify:track:") {
        Ok((PlayableKind::Track, rest))
    } else if let Some(rest) = uri.strip_prefix("spotify:episode:") {
        Ok((PlayableKind::Episode, rest))
    } else if uri.len() == 22 && uri.chars().all(|c| c.is_ascii_alphanumeric()) {
        Ok((PlayableKind::Track, uri))
    } else {
        Err(anyhow!("not a spotify track/episode URI: {uri}"))
    }
}

/// Back-compat: tracks only.
pub fn base62_from_uri(uri: &str) -> Result<&str> {
    let (k, id) = parse_playable_uri(uri)?;
    if k != PlayableKind::Track {
        return Err(anyhow!("not a spotify track URI: {uri}"));
    }
    Ok(id)
}
