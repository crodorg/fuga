//! PTY soak / stability harness (Wave 4).
//!
//! Runs the *real* fuga binary inside a pseudo-terminal so it exercises the
//! parts the unit suite can't reach: ratatui-image's `Picker::from_query_stdio`
//! capability probe, crossterm raw-mode setup + the blocking input thread, the
//! tokio event loop, SIGWINCH/resize handling, and the full render path. (The
//! tmux-only `term_probe::kitty_selfprobe` unsafe FFI path is NOT covered — the
//! harness runs with a clean, tmux-free env; cover that one under real tmux.)
//!
//! Why a PTY: fuga's startup probe blocks in a terminal read until the terminal
//! answers a cursor-position query. Without a real terminal it hangs in
//! `n_tty_read` (see decisions.md 2026-06-27). Here the harness *is* the
//! terminal — it answers the probe (and declines the Kitty query so fuga falls
//! back to halfblocks), then drives navigation + resize for many iterations
//! while watching for panics and RSS growth.
//!
//! Ignored by default (needs the release binary + a few seconds). Run with:
//!   cargo build --release
//!   cargo test --release --test soak -- --ignored --nocapture
//! Tunables (env): FUGA_SOAK_BIN (default target/release/fuga),
//!   FUGA_SOAK_ITERS (default 300), FUGA_SOAK_STEP_MS (default 25).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// One probe query and the canned reply the harness sends back. The Kitty
/// graphics query (`\x1b_Gi=...`) is deliberately absent: not answering it makes
/// ratatui-image fall back to halfblocks, which is what we want under test.
struct Probe {
    /// Marker substring fuga writes when it wants this capability.
    query: &'static [u8],
    /// What a real terminal would answer.
    reply: &'static [u8],
    answered: bool,
}

fn probes() -> Vec<Probe> {
    vec![
        // Cursor position report (DSR) — the terminator ratatui-image waits for.
        Probe {
            query: b"\x1b[6n",
            reply: b"\x1b[10;10R",
            answered: false,
        },
        // Secondary device attributes (checked before primary so "\x1b[c" scan
        // doesn't swallow it).
        Probe {
            query: b"\x1b[>c",
            reply: b"\x1b[>0;276;0c",
            answered: false,
        },
        // Primary device attributes — VT220, no ";4" so no sixel claimed.
        Probe {
            query: b"\x1b[c",
            reply: b"\x1b[?62;1;6c",
            answered: false,
        },
        // Cell size in pixels (CSI 16 t) -> CSI 6 ; height ; width t.
        Probe {
            query: b"\x1b[16t",
            reply: b"\x1b[6;14;7t",
            answered: false,
        },
        // Text-area size in pixels (CSI 14 t).
        Probe {
            query: b"\x1b[14t",
            reply: b"\x1b[4;480;640t",
            answered: false,
        },
        // Background color (OSC 11) -> opaque black.
        Probe {
            query: b"\x1b]11;?",
            reply: b"\x1b]11;rgb:0000/0000/0000\x07",
            answered: false,
        },
    ]
}

fn bin_path() -> PathBuf {
    if let Some(p) = std::env::var_os("FUGA_SOAK_BIN") {
        return PathBuf::from(p);
    }
    // tests run with CWD = package root.
    PathBuf::from("target/release/fuga")
}

/// Put the pty into raw mode from the harness side via the master fd (on Linux
/// `tcsetattr` on the master sets the slave's line discipline). Without this the
/// slave defaults to canonical mode with echo, so keystrokes are line-buffered
/// and echoed instead of delivered byte-by-byte — and a TUI never sees them.
/// fuga's own `enable_raw_mode` then just re-asserts what we set.
#[cfg(unix)]
fn set_pty_raw(master_fd: std::os::unix::io::RawFd) {
    unsafe {
        let mut t = std::mem::MaybeUninit::<libc::termios>::zeroed().assume_init();
        if libc::tcgetattr(master_fd, &mut t) == 0 {
            libc::cfmakeraw(&mut t);
            let _ = libc::tcsetattr(master_fd, libc::TCSANOW, &t);
        }
    }
}

fn read_vmrss_kb(pid: u32) -> Option<u64> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb);
        }
    }
    None
}

#[test]
#[ignore = "PTY soak: needs the release binary; run with --ignored"]
fn pty_soak_drives_render_resize_quit_without_panic_or_leak() {
    let bin = bin_path();
    assert!(
        bin.exists(),
        "release binary not found at {} — build it first: cargo build --release \
         (or set FUGA_SOAK_BIN)",
        bin.display()
    );
    // Absolute path: portable-pty does PATH resolution otherwise, and our cwd is
    // the temp HOME, so a relative path would not resolve.
    let bin = std::fs::canonicalize(&bin).expect("canonicalize fuga binary path");

    let iters: usize = std::env::var("FUGA_SOAK_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let step = Duration::from_millis(
        std::env::var("FUGA_SOAK_STEP_MS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(25),
    );

    // Isolated HOME so the soak uses defaults and never touches real config/state.
    let home = std::env::temp_dir().join(format!("fuga-soak-{}", std::process::id()));
    let _ = std::fs::create_dir_all(home.join(".config"));
    let _ = std::fs::create_dir_all(home.join("run"));

    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("openpty");

    // Raw from the start so input is delivered byte-by-byte, not line-buffered/echoed.
    #[cfg(unix)]
    if let Some(fd) = pair.master.as_raw_fd() {
        set_pty_raw(fd);
    }

    // Clean, minimal env: env_clear() so the child never inherits TMUX (which
    // would send fuga down the tmux probe path, shelling out to the real tmux
    // server and toggling termios mid-startup) or any other host surprise.
    let mut cmd = CommandBuilder::new(&bin);
    cmd.env_clear();
    cmd.cwd(&home);
    cmd.env("HOME", &home);
    cmd.env("XDG_CONFIG_HOME", home.join(".config"));
    cmd.env("XDG_RUNTIME_DIR", home.join("run"));
    cmd.env("TERM", "xterm-256color");
    cmd.env("LANG", "C.UTF-8");
    if let Some(path) = std::env::var_os("PATH") {
        cmd.env("PATH", path);
    }

    let mut child = pair.slave.spawn_command(cmd).expect("spawn fuga in pty");
    // Drop the slave handle so EOF propagates to our reader when fuga exits.
    drop(pair.slave);
    let pid = child.process_id();

    let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("pty writer")));
    let mut reader = pair.master.try_clone_reader().expect("pty reader");

    let bytes_seen = Arc::new(AtomicUsize::new(0));
    let panicked = Arc::new(AtomicBool::new(false));
    let tail = Arc::new(Mutex::new(Vec::<u8>::new()));

    // Reader thread: drain continuously (a TUI that can't write blocks), answer
    // capability probes, watch for panics, keep the last few KB for diagnostics.
    let r_writer = Arc::clone(&writer);
    let r_bytes = Arc::clone(&bytes_seen);
    let r_panicked = Arc::clone(&panicked);
    let r_tail = Arc::clone(&tail);
    let reader_thread = std::thread::spawn(move || {
        let mut probes = probes();
        let mut acc: Vec<u8> = Vec::with_capacity(8192);
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break, // EOF / EIO => child gone
                Ok(n) => {
                    r_bytes.fetch_add(n, Ordering::Relaxed);
                    acc.extend_from_slice(&buf[..n]);
                    // Answer any not-yet-answered probe present in the stream.
                    for p in probes.iter_mut() {
                        if !p.answered && acc.windows(p.query.len()).any(|w| w == p.query) {
                            if let Ok(mut w) = r_writer.lock() {
                                let _ = w.write_all(p.reply);
                                let _ = w.flush();
                            }
                            p.answered = true;
                        }
                    }
                    if acc.windows(8).any(|w| w == b"panicked") {
                        r_panicked.store(true, Ordering::Relaxed);
                    }
                    // Keep the accumulator (and a diagnostic tail) bounded.
                    if acc.len() > 64 * 1024 {
                        let cut = acc.len() - 8192;
                        acc.drain(..cut);
                    }
                    if let Ok(mut t) = r_tail.lock() {
                        t.extend_from_slice(&buf[..n]);
                        let len = t.len();
                        if len > 4096 {
                            t.drain(..len - 4096);
                        }
                    }
                }
            }
        }
    });

    let send = |bytes: &[u8]| {
        if let Ok(mut w) = writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    };

    // Wait for first render (probe answered + buffer painted) before driving.
    let init_deadline = Instant::now() + Duration::from_secs(15);
    while bytes_seen.load(Ordering::Relaxed) < 2000 {
        if Instant::now() >= init_deadline {
            let _ = child.kill();
            let t = tail
                .lock()
                .map(|t| String::from_utf8_lossy(&t).into_owned())
                .unwrap_or_default();
            panic!(
                "fuga did not render within 15s ({} bytes seen) — probe unanswered or startup hang.\n\
                 last output:\n{}",
                bytes_seen.load(Ordering::Relaxed),
                t
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Settle, then baseline RSS.
    std::thread::sleep(Duration::from_millis(500));
    let rss_baseline = pid.and_then(read_vmrss_kb);
    let mut rss_max = rss_baseline.unwrap_or(0);
    let mut rss_late = rss_baseline.unwrap_or(0);

    // Safe navigation + resize only — nothing that triggers network/playback,
    // opens a modal, or starts a leader sequence ('g' is a leader prefix). Tab /
    // BackTab / digits switch tabs; j/k and arrows scroll.
    let keys: &[&[u8]] = &[
        b"\t",     // next_tab
        b"j",      // down
        b"k",      // up
        b"\x1b[B", // Down arrow
        b"\x1b[A", // Up arrow
        b"1",      // tab_1
        b"2",      // tab_2
        b"3",      // tab_3
        b"\x1b[Z", // BackTab (prev_tab)
    ];
    let sizes = [(80u16, 24u16), (100, 30), (60, 20), (120, 40), (90, 26)];

    for i in 0..iters {
        assert!(
            !panicked.load(Ordering::Relaxed),
            "fuga panicked during soak (iter {i})"
        );
        // Liveness: if the child exited early, that's a crash/early-quit.
        if let Ok(Some(status)) = child.try_wait() {
            panic!("fuga exited early at iter {i} with status {status:?}");
        }

        send(keys[i % keys.len()]);
        if i % 7 == 0 {
            let (cols, rows) = sizes[(i / 7) % sizes.len()];
            let _ = pair.master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        if i % 25 == 0 {
            if let Some(kb) = pid.and_then(read_vmrss_kb) {
                rss_max = rss_max.max(kb);
                if i >= iters / 2 {
                    rss_late = kb;
                }
            }
        }
        std::thread::sleep(step);
    }

    // Quit cleanly and confirm the process tears down. Esc first clears any
    // pending leader/filter/modal state so 'q' reaches the top-level quit.
    send(b"\x1b");
    std::thread::sleep(Duration::from_millis(150));
    send(b"q");
    let quit_deadline = Instant::now() + Duration::from_secs(8);
    let mut exited = false;
    while Instant::now() < quit_deadline {
        if let Ok(Some(_)) = child.try_wait() {
            exited = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !exited {
        let _ = child.kill();
        let dump = tail
            .lock()
            .map(|t| {
                t.iter()
                    .map(|&b| {
                        if b == b'\n' || (0x20..0x7f).contains(&b) {
                            b as char
                        } else {
                            '.'
                        }
                    })
                    .collect::<String>()
            })
            .unwrap_or_default();
        panic!("fuga did not exit within 8s of 'q' (possible shutdown hang)\nlast output:\n{dump}");
    }
    let _ = reader_thread.join();

    assert!(
        !panicked.load(Ordering::Relaxed),
        "fuga panicked during soak"
    );

    // Leak guard: late RSS shouldn't balloon past the warmup baseline. Generous
    // factor — image/art caches legitimately grow some; we're catching runaway
    // per-iteration growth, not normal caching.
    eprintln!(
        "soak: iters={iters} rss_baseline={:?}kB rss_max={rss_max}kB rss_late={rss_late}kB",
        rss_baseline
    );
    if let Some(base) = rss_baseline {
        if base > 0 && rss_late > 0 {
            let ratio = rss_late as f64 / base as f64;
            assert!(
                ratio < 1.6,
                "RSS grew {ratio:.2}x over the soak (baseline {base}kB -> late {rss_late}kB) — \
                 possible leak"
            );
        }
    }

    let _ = std::fs::remove_dir_all(&home);
}
