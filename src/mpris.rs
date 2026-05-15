//! Media-key bridge. Exposes a uniform channel-based interface to the app;
//! the platform-specific backend lives behind `cfg(target_os = …)`:
//!
//! * Linux  — `mpris-server` exposes `org.mpris.MediaPlayer2.fuga` so media
//!   keys, GNOME panel, KDE plasmoids and `playerctl` all drive fuga.
//! * macOS  — `souvlaki` registers an `MPRemoteCommandCenter` handler so the
//!   F7/F8/F9 media keys, AirPods, and Control Center drive fuga natively.
//! * Other  — dead channels (the app code path is identical regardless).
//!
//! Two channels link the backend to the app, in both directions:
//!
//!   app <— MprisEvent  —— external client / hardware action (Play, Next, …)
//!   app —— MprisCommand —> outbound state pushes (metadata, status, vol)
//!
//! Each backend lives on a dedicated OS thread because the underlying
//! handles (`mpris-server::Player` on Linux, `souvlaki::MediaControls` on
//! macOS) are `!Send`.

use anyhow::Result;
use tokio::sync::mpsc;

#[cfg(target_os = "linux")]
use mpris_server::{Metadata, PlaybackStatus, Player, Time};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::thread;

/// Inbound: an external client (D-Bus, MPRemoteCommandCenter, …) requested
/// an action. Translated to `Action` in the app event loop.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
pub enum MprisEvent {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    Stop,
    /// Absolute volume 0..=100 (D-Bus reports float 0.0..=1.0; we scale).
    /// Only the Linux MPRIS backend emits this; macOS's
    /// `MPRemoteCommandCenter` has no system-level volume slider.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    SetVolume(u8),
}

/// Outbound: app pushes new state to the platform media bridge.
#[derive(Debug, Clone)]
#[cfg_attr(
    not(any(target_os = "linux", target_os = "macos")),
    allow(dead_code)
)]
pub enum MprisCommand {
    Metadata {
        title: String,
        artists: Vec<String>,
        album: Option<String>,
        duration_ms: u32,
        art_url: Option<String>,
    },
    PlaybackStatus(MprisStatus),
    /// macOS drops the inner value (no system volume API), but the variant
    /// still needs to round-trip through the channel so the app code path
    /// stays platform-agnostic.
    Volume(#[cfg_attr(not(target_os = "linux"), allow(dead_code))] u8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MprisStatus {
    Playing,
    Paused,
    Stopped,
}

pub struct MprisHandles {
    pub event_rx: mpsc::UnboundedReceiver<MprisEvent>,
    pub command_tx: mpsc::UnboundedSender<MprisCommand>,
}

/// Spawn the platform media bridge on a dedicated thread. Returns the
/// channels the app uses to receive media-key events and push state updates.
/// Platforms without a backend return Ok with dead channels so the app code
/// path stays the same.
#[cfg(all(not(target_os = "linux"), not(target_os = "macos")))]
pub fn spawn() -> Result<MprisHandles> {
    let (_event_tx, event_rx) = mpsc::unbounded_channel::<MprisEvent>();
    let (command_tx, _command_rx) = mpsc::unbounded_channel::<MprisCommand>();
    Ok(MprisHandles {
        event_rx,
        command_tx,
    })
}

/// macOS: souvlaki bridges to `MPRemoteCommandCenter`. The `MediaControls`
/// handle is `!Send` (it holds Objective-C objects), so it lives on a
/// dedicated OS thread that creates it locally and drains the outbound
/// command channel with `blocking_recv`. Volume commands are dropped because
/// `MPRemoteCommandCenter` has no system-level volume slider — the channel
/// is kept for parity with the Linux path.
#[cfg(target_os = "macos")]
pub fn spawn() -> Result<MprisHandles> {
    use souvlaki::{
        MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, PlatformConfig,
    };
    use std::time::Duration as StdDuration;

    let (event_tx, event_rx) = mpsc::unbounded_channel::<MprisEvent>();
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<MprisCommand>();

    thread::spawn(move || {
        let config = PlatformConfig {
            dbus_name: "fuga",
            display_name: "fuga",
            hwnd: None,
        };
        let mut controls = match MediaControls::new(config) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("media controls init failed: {e:?}");
                return;
            }
        };
        let tx = event_tx.clone();
        if let Err(e) = controls.attach(move |event: MediaControlEvent| {
            let mapped = match event {
                MediaControlEvent::Play => Some(MprisEvent::Play),
                MediaControlEvent::Pause => Some(MprisEvent::Pause),
                MediaControlEvent::Toggle => Some(MprisEvent::PlayPause),
                MediaControlEvent::Next => Some(MprisEvent::Next),
                MediaControlEvent::Previous => Some(MprisEvent::Previous),
                MediaControlEvent::Stop => Some(MprisEvent::Stop),
                _ => None,
            };
            if let Some(ev) = mapped {
                let _ = tx.send(ev);
            }
        }) {
            tracing::warn!("media controls attach failed: {e:?}");
            return;
        }

        while let Some(cmd) = command_rx.blocking_recv() {
            match cmd {
                MprisCommand::Metadata {
                    title,
                    artists,
                    album,
                    duration_ms,
                    art_url,
                } => {
                    let artist_joined = artists.join(", ");
                    let dur = StdDuration::from_millis(duration_ms as u64);
                    let meta = MediaMetadata {
                        title: Some(&title),
                        artist: if artist_joined.is_empty() {
                            None
                        } else {
                            Some(&artist_joined)
                        },
                        album: album.as_deref(),
                        cover_url: art_url.as_deref(),
                        duration: Some(dur),
                    };
                    if let Err(e) = controls.set_metadata(meta) {
                        tracing::debug!("mac set_metadata: {e:?}");
                    }
                }
                MprisCommand::PlaybackStatus(s) => {
                    let pb = match s {
                        MprisStatus::Playing => MediaPlayback::Playing { progress: None },
                        MprisStatus::Paused => MediaPlayback::Paused { progress: None },
                        MprisStatus::Stopped => MediaPlayback::Stopped,
                    };
                    if let Err(e) = controls.set_playback(pb) {
                        tracing::debug!("mac set_playback: {e:?}");
                    }
                }
                MprisCommand::Volume(_) => {
                    // No-op: MPRemoteCommandCenter has no system volume API.
                }
            }
        }
    });

    Ok(MprisHandles {
        event_rx,
        command_tx,
    })
}

#[cfg(target_os = "linux")]
pub fn spawn() -> Result<MprisHandles> {
    let (event_tx, event_rx) = mpsc::unbounded_channel::<MprisEvent>();
    let (command_tx, mut command_rx) = mpsc::unbounded_channel::<MprisCommand>();

    thread::spawn(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("mpris runtime build: {e}");
                return;
            }
        };
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let player = match Player::builder("fuga")
                .identity("fuga")
                .desktop_entry("fuga")
                .can_play(true)
                .can_pause(true)
                .can_go_next(true)
                .can_go_previous(true)
                .can_control(true)
                .can_quit(false)
                .can_raise(false)
                .can_seek(false)
                .build()
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!("mpris player build: {e}");
                    return;
                }
            };

            // Wire incoming D-Bus invocations to the inbound channel.
            let tx = event_tx.clone();
            player.connect_play_pause(move |_p| {
                let _ = tx.send(MprisEvent::PlayPause);
            });
            let tx = event_tx.clone();
            player.connect_play(move |_p| {
                let _ = tx.send(MprisEvent::Play);
            });
            let tx = event_tx.clone();
            player.connect_pause(move |_p| {
                let _ = tx.send(MprisEvent::Pause);
            });
            let tx = event_tx.clone();
            player.connect_next(move |_p| {
                let _ = tx.send(MprisEvent::Next);
            });
            let tx = event_tx.clone();
            player.connect_previous(move |_p| {
                let _ = tx.send(MprisEvent::Previous);
            });
            let tx = event_tx.clone();
            player.connect_stop(move |_p| {
                let _ = tx.send(MprisEvent::Stop);
            });
            let tx = event_tx.clone();
            player.connect_set_volume(move |_p, vol| {
                // Volume is f64 0.0..=1.0 in MPRIS; clamp + scale to u8 0..=100.
                let scaled = (vol.clamp(0.0, 1.0) * 100.0).round() as u8;
                let _ = tx.send(MprisEvent::SetVolume(scaled));
            });

            // Run the D-Bus event loop on this LocalSet.
            tokio::task::spawn_local(player.run());

            while let Some(cmd) = command_rx.recv().await {
                match cmd {
                    MprisCommand::Metadata {
                        title,
                        artists,
                        album,
                        duration_ms,
                        art_url,
                    } => {
                        let mut b = Metadata::builder()
                            .title(&title)
                            .artist(artists.iter().map(|s| s.as_str()).collect::<Vec<_>>())
                            .length(Time::from_millis(duration_ms as i64));
                        if let Some(a) = &album {
                            b = b.album(a);
                        }
                        if let Some(url) = &art_url {
                            b = b.art_url(url);
                        }
                        if let Err(e) = player.set_metadata(b.build()).await {
                            tracing::warn!("mpris set_metadata: {e}");
                        }
                    }
                    MprisCommand::PlaybackStatus(s) => {
                        let st = match s {
                            MprisStatus::Playing => PlaybackStatus::Playing,
                            MprisStatus::Paused => PlaybackStatus::Paused,
                            MprisStatus::Stopped => PlaybackStatus::Stopped,
                        };
                        if let Err(e) = player.set_playback_status(st).await {
                            tracing::warn!("mpris set_playback_status: {e}");
                        }
                    }
                    MprisCommand::Volume(v) => {
                        let vol = (v.min(100) as f64) / 100.0;
                        if let Err(e) = player.set_volume(vol).await {
                            tracing::warn!("mpris set_volume: {e}");
                        }
                    }
                }
            }
        });
    });

    Ok(MprisHandles {
        event_rx,
        command_tx,
    })
}
