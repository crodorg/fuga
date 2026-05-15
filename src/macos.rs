//! macOS-only: own the real OS main thread for Cocoa.
//!
//! `MPRemoteCommandCenter` (the system facility that delivers play / pause /
//! next / previous from media keys, AirPods, the Touch Bar, the Lock Screen,
//! etc.) only works inside an `NSApplication`. NSApp insists on running on
//! the real main thread, and `NSApp.run` blocks forever — so we can't share
//! that thread with the tokio runtime.
//!
//! The split (see `main.rs`):
//!
//!   thread 0 (real main): NSApp with `Accessory` activation policy →
//!     no dock icon and no foreground activation (so the terminal keeps
//!     keyboard focus — the bug that bit the previous souvlaki attempt),
//!     but still a real app to the system, which is the level needed for
//!     `MPRemoteCommandCenter` to route remote-command events to us.
//!     Purely a sink for those callbacks.
//!
//!   thread "fuga-async": tokio multi-thread runtime → `async_main`.
//!     Receives `MprisEvent`s through the same `UnboundedReceiver` Linux
//!     uses for the D-Bus MPRIS bridge, so the app code path is identical.

use crate::mpris::MprisEvent;
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_foundation::{NSDictionary, NSString};
use objc2_media_player::{
    MPMediaItemPropertyTitle, MPNowPlayingInfoCenter, MPNowPlayingPlaybackState, MPRemoteCommand,
    MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use tokio::sync::mpsc::UnboundedSender;

pub fn run_main_loop(tx: UnboundedSender<MprisEvent>) {
    // SAFETY: callers in main.rs invoke this only from the OS main thread.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    // Accessory (not Prohibited): no dock icon and no foreground activation,
    // but the process is still a real app to macOS — which is the level
    // MPRemoteCommandCenter needs to route remote-command events to us.
    // Prohibited is too restrictive: the system treats us as a daemon and
    // skips event delivery.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
    unsafe {
        install(&center.playCommand(), tx.clone(), MprisEvent::Play);
        install(&center.pauseCommand(), tx.clone(), MprisEvent::Pause);
        install(
            &center.togglePlayPauseCommand(),
            tx.clone(),
            MprisEvent::PlayPause,
        );
        install(&center.nextTrackCommand(), tx.clone(), MprisEvent::Next);
        install(
            &center.previousTrackCommand(),
            tx.clone(),
            MprisEvent::Previous,
        );
        install(&center.stopCommand(), tx, MprisEvent::Stop);
    }

    // Announce ourselves as an active player. Without this macOS doesn't
    // pick a "now playing" app and the remote commands above never fire,
    // even with handlers attached. Real metadata will overwrite this once
    // a track starts (TODO: plumb MprisCommand::Metadata back here); for
    // now a placeholder title is enough for the system to route events.
    unsafe { announce_player() };

    // Blocks until the process exits. The async worker calls
    // `std::process::exit` when `async_main` returns, tearing this down.
    app.run();
}

unsafe fn announce_player() {
    let center = MPNowPlayingInfoCenter::defaultCenter();
    let title_val: Retained<NSString> = NSString::from_str("fuga");
    let title_obj: &AnyObject = &*(Retained::as_ptr(&title_val) as *const AnyObject);
    let info: Retained<NSDictionary<NSString, AnyObject>> =
        NSDictionary::from_slices(&[MPMediaItemPropertyTitle], &[title_obj]);
    center.setNowPlayingInfo(Some(&info));
    center.setPlaybackState(MPNowPlayingPlaybackState::Playing);
}

unsafe fn install(cmd: &MPRemoteCommand, tx: UnboundedSender<MprisEvent>, ev: MprisEvent) {
    cmd.setEnabled(true);
    let handler = RcBlock::new(move |_event: std::ptr::NonNull<MPRemoteCommandEvent>| {
        // Debug-level so it's silent in default `info` logs but available
        // when diagnosing missing media-key behavior with `--debug`.
        tracing::debug!("macos mediakey: {:?}", ev);
        // Unbounded; only fails after the receiver is dropped, i.e. shutdown.
        let _ = tx.send(ev.clone());
        MPRemoteCommandHandlerStatus::Success
    });
    cmd.addTargetWithHandler(&handler);
}
