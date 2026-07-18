//! Thin platform entry point. All logic lives in the `fuga` library crate
//! (`src/lib.rs`); this binary only owns the tokio runtime setup and, on
//! macOS, the Cocoa main-thread run loop. See [`fuga::run_app`].

use anyhow::{Context, Result};

#[cfg(not(target_os = "macos"))]
fn main() -> Result<()> {
    // Must run before the runtime spawns threads (sound env mutation): if the
    // outer tmux renders kitty natively, drop TMUX so fuga emits raw kitty.
    fuga::neutralize_native_kitty_tmux();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?;
    rt.block_on(fuga::run_app(None))
}

/// macOS entry point. The real OS main thread is reserved for the Cocoa run
/// loop (see [`fuga::macos`]); the tokio runtime + the entire async app live
/// on a dedicated worker thread. Pre-built MPRIS channels link them: the Cocoa
/// side fills `event_tx` from MPRemoteCommandCenter callbacks, the tokio side
/// consumes `event_rx` in the usual app loop.
#[cfg(target_os = "macos")]
fn main() -> Result<()> {
    use fuga::mpris;

    // Must run before any thread spawns (sound env mutation): if the outer
    // tmux renders kitty natively, drop TMUX so fuga emits raw kitty.
    fuga::neutralize_native_kitty_tmux();

    // A panic on the async worker thread would otherwise leave NSApp.run
    // looping on the main thread forever — process stays alive but the app is
    // dead and only `kill -9` clears it. Forward to the default hook (so the
    // panic message still prints) then exit the whole process.
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        prev(info);
        std::process::exit(101);
    }));

    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel::<mpris::MprisEvent>();
    let (command_tx, _command_rx) = tokio::sync::mpsc::unbounded_channel::<mpris::MprisCommand>();
    let handles = mpris::MprisHandles {
        event_rx,
        command_tx,
    };

    std::thread::Builder::new()
        .name("fuga-async".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("tokio runtime: {e}");
                    std::process::exit(1);
                }
            };
            let code = match rt.block_on(fuga::run_app(Some(handles))) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("err: {e}");
                    1
                }
            };
            // NSApp.run on the main thread won't return on its own; tear the
            // whole process down so it exits with us.
            std::process::exit(code);
        })
        .context("spawn async worker")?;

    fuga::macos::run_main_loop(event_tx);
    Ok(())
}
