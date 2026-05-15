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
//!   thread 0 (real main): NSApp with `Prohibited` activation policy →
//!     no dock icon, no foreground stealing (the bug that bit the previous
//!     souvlaki attempt), purely a sink for remote-command callbacks.
//!
//!   thread "fuga-async": tokio multi-thread runtime → `async_main`.
//!     Receives `MprisEvent`s through the same `UnboundedReceiver` Linux
//!     uses for the D-Bus MPRIS bridge, so the app code path is identical.

use crate::mpris::MprisEvent;
use block2::RcBlock;
use objc2::MainThreadMarker;
use objc2_app_kit::{NSApplication, NSApplicationActivationPolicy};
use objc2_media_player::{
    MPRemoteCommand, MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};
use tokio::sync::mpsc::UnboundedSender;

pub fn run_main_loop(tx: UnboundedSender<MprisEvent>) {
    // SAFETY: callers in main.rs invoke this only from the OS main thread.
    let mtm = unsafe { MainThreadMarker::new_unchecked() };
    let app = NSApplication::sharedApplication(mtm);
    // Critical: keep us out of the dock and out of the foreground app list.
    // Without this, NSApp claims the active-app slot when first touched and
    // pulls keyboard focus out of the terminal that launched us.
    app.setActivationPolicy(NSApplicationActivationPolicy::Prohibited);

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

    // Blocks until the process exits. The async worker calls
    // `std::process::exit` when `async_main` returns, tearing this down.
    app.run();
}

unsafe fn install(cmd: &MPRemoteCommand, tx: UnboundedSender<MprisEvent>, ev: MprisEvent) {
    cmd.setEnabled(true);
    let handler = RcBlock::new(move |_event: std::ptr::NonNull<MPRemoteCommandEvent>| {
        // Unbounded; only fails after the receiver is dropped, i.e. shutdown.
        let _ = tx.send(ev.clone());
        MPRemoteCommandHandlerStatus::Success
    });
    cmd.addTargetWithHandler(&handler);
}
