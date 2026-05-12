//! MPRIS2 D-Bus bridge: lets media keys, GNOME panel, KDE plasmoids and
//! `playerctl` drive fuga. Exposes `org.mpris.MediaPlayer2.fuga`.
//!
//! Architecture mirrors spotatui's: the `mpris-server` `Player` uses `Rc`
//! internally (not Send), so it lives on a dedicated OS thread with its own
//! current-thread tokio runtime + LocalSet. Two channels link it to the app:
//!
//!   app <— MprisEvent  —— D-Bus client invocations (Play, Next, …)
//!   app —— MprisCommand —> outbound state pushes (metadata, status, vol)
//!
//! Linux-only. Feature-gated at the call site so non-Linux builds skip this
//! module entirely.

use anyhow::Result;
use tokio::sync::mpsc;

#[cfg(target_os = "linux")]
use mpris_server::{Metadata, PlaybackStatus, Player, Time};
#[cfg(target_os = "linux")]
use std::thread;

/// Inbound: D-Bus client requested an action. Translated to `Action` in app.
#[derive(Debug, Clone)]
pub enum MprisEvent {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    Stop,
    /// Absolute volume 0..=100 (D-Bus reports float 0.0..=1.0; we scale).
    SetVolume(u8),
}

/// Outbound: app pushes new state to the MPRIS server.
#[derive(Debug, Clone)]
pub enum MprisCommand {
    Metadata {
        title: String,
        artists: Vec<String>,
        album: Option<String>,
        duration_ms: u32,
        art_url: Option<String>,
    },
    PlaybackStatus(MprisStatus),
    Volume(u8),
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

/// Spawn the MPRIS server on a dedicated thread. Returns the channels the app
/// uses to receive media-key events and push state updates. Non-Linux builds
/// return Ok with dead channels — the app code path stays the same.
#[cfg(not(target_os = "linux"))]
pub fn spawn() -> Result<MprisHandles> {
    let (_event_tx, event_rx) = mpsc::unbounded_channel::<MprisEvent>();
    let (command_tx, _command_rx) = mpsc::unbounded_channel::<MprisCommand>();
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
