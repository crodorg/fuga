//! PTY soak + performance harness (Waves 4 & 5).
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
//! back to halfblocks), forces the pty raw so keystrokes are delivered, then
//! either drives navigation + resize (soak) or sits idle and measures CPU (perf).
//!
//! Two ignored tests (need the release binary + a few seconds):
//!   cargo build --release
//!   cargo test --release --test soak -- --ignored --nocapture
//! Tunables (env): FUGA_SOAK_BIN (default target/release/fuga),
//!   FUGA_SOAK_ITERS (default 300), FUGA_SOAK_STEP_MS (default 25).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

/// One probe query and the canned reply the harness sends back. The Kitty
/// graphics query (`\x1b_Gi=...`) is deliberately absent: not answering it makes
/// ratatui-image fall back to halfblocks, which is what we want under test.
struct Probe {
    query: &'static [u8],
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

fn resolve_bin() -> PathBuf {
    let bin = bin_path();
    assert!(
        bin.exists(),
        "release binary not found at {} — build it first: cargo build --release \
         (or set FUGA_SOAK_BIN)",
        bin.display()
    );
    // Absolute: portable-pty does PATH resolution otherwise, and our cwd is the
    // temp HOME, so a relative path would not resolve.
    std::fs::canonicalize(&bin).expect("canonicalize fuga binary path")
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

/// Resident set size in kB from /proc/<pid>/status (Linux).
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

/// Total CPU time (utime+stime, in clock ticks, summed across all threads) from
/// /proc/<pid>/stat (Linux). Fields 14/15, indexed after the comm `)` so a comm
/// containing spaces/parens can't shift the offsets.
fn read_cpu_jiffies(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rparen = stat.rfind(')')?;
    let fields: Vec<&str> = stat[rparen + 1..].split_whitespace().collect();
    // fields[0] = state (field 3); utime = field 14 -> idx 11; stime = field 15 -> idx 12.
    let utime: u64 = fields.get(11)?.parse().ok()?;
    let stime: u64 = fields.get(12)?.parse().ok()?;
    Some(utime + stime)
}

#[cfg(unix)]
fn clk_tck() -> u64 {
    let v = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if v > 0 { v as u64 } else { 100 }
}

/// A running fuga child in a pty, with a background reader that drains output,
/// answers capability probes, and watches for panics.
struct Harness {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    bytes_seen: Arc<AtomicUsize>,
    panicked: Arc<AtomicBool>,
    tail: Arc<Mutex<Vec<u8>>>,
    pid: Option<u32>,
    home: PathBuf,
    _reader: JoinHandle<()>,
}

impl Harness {
    fn spawn(bin: &Path) -> Harness {
        // Isolated HOME so the run uses defaults and never touches real state.
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

        // Raw from the start so input is delivered byte-by-byte, not echoed.
        #[cfg(unix)]
        if let Some(fd) = pair.master.as_raw_fd() {
            set_pty_raw(fd);
        }

        // env_clear() so the child never inherits TMUX (which would send fuga
        // down the tmux probe path, shelling out to the real tmux server and
        // toggling termios mid-startup) or any other host surprise.
        let mut cmd = CommandBuilder::new(bin);
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

        let child = pair.slave.spawn_command(cmd).expect("spawn fuga in pty");
        drop(pair.slave); // EOF propagates to the reader when fuga exits.
        let pid = child.process_id();

        let writer = Arc::new(Mutex::new(pair.master.take_writer().expect("pty writer")));
        let mut reader = pair.master.try_clone_reader().expect("pty reader");

        let bytes_seen = Arc::new(AtomicUsize::new(0));
        let panicked = Arc::new(AtomicBool::new(false));
        let tail = Arc::new(Mutex::new(Vec::<u8>::new()));

        let r_writer = Arc::clone(&writer);
        let r_bytes = Arc::clone(&bytes_seen);
        let r_panicked = Arc::clone(&panicked);
        let r_tail = Arc::clone(&tail);
        let reader = std::thread::spawn(move || {
            let mut probes = probes();
            let mut acc: Vec<u8> = Vec::with_capacity(8192);
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break, // EOF / EIO => child gone
                    Ok(n) => {
                        r_bytes.fetch_add(n, Ordering::Relaxed);
                        acc.extend_from_slice(&buf[..n]);
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

        Harness {
            child,
            master: pair.master,
            writer,
            bytes_seen,
            panicked,
            tail,
            pid,
            home,
            _reader: reader,
        }
    }

    fn send(&self, bytes: &[u8]) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(bytes);
            let _ = w.flush();
        }
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
    }

    fn panicked(&self) -> bool {
        self.panicked.load(Ordering::Relaxed)
    }

    fn tail_str(&self) -> String {
        self.tail
            .lock()
            .map(|t| String::from_utf8_lossy(&t).into_owned())
            .unwrap_or_default()
    }

    fn rss_kb(&self) -> Option<u64> {
        self.pid.and_then(read_vmrss_kb)
    }

    #[cfg(unix)]
    fn cpu_jiffies(&self) -> Option<u64> {
        self.pid.and_then(read_cpu_jiffies)
    }

    /// Block until fuga has painted (probe answered + a frame's worth of bytes).
    fn wait_first_frame(&mut self, timeout: Duration) -> Result<Duration, String> {
        let start = Instant::now();
        let deadline = start + timeout;
        while self.bytes_seen.load(Ordering::Relaxed) < 2000 {
            if Instant::now() >= deadline {
                let _ = self.child.kill();
                return Err(format!(
                    "fuga did not render within {timeout:?} ({} bytes) — probe unanswered or \
                     startup hang.\nlast output:\n{}",
                    self.bytes_seen.load(Ordering::Relaxed),
                    self.tail_str()
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(start.elapsed())
    }

    fn exited(&mut self) -> Option<portable_pty::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Send Esc (clear any pending leader/modal/filter state) then 'q', and wait
    /// for the process to tear down. Returns true if it exited in time.
    fn quit(&mut self, timeout: Duration) -> bool {
        self.send(b"\x1b");
        std::thread::sleep(Duration::from_millis(150));
        self.send(b"q");
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.exited().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        false
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

#[test]
#[ignore = "PTY soak: needs the release binary; run with --ignored"]
fn pty_soak_drives_render_resize_quit_without_panic_or_leak() {
    let bin = resolve_bin();
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

    let mut h = Harness::spawn(&bin);
    if let Err(e) = h.wait_first_frame(Duration::from_secs(15)) {
        panic!("{e}");
    }

    // Settle, then record a cold baseline (pre-warmup, for context only).
    std::thread::sleep(Duration::from_millis(500));
    let rss_baseline = h.rss_kb();
    let mut rss_max = rss_baseline.unwrap_or(0);
    // For the leak check we ignore warmup: rss_mid is the first post-warmup
    // (>= halfway) sample, rss_late the last. A real leak keeps the second half
    // climbing; normal art/render caching has plateaued by the midpoint.
    let mut rss_mid = 0u64;
    let mut rss_late = 0u64;

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
        assert!(!h.panicked(), "fuga panicked during soak (iter {i})");
        if let Some(status) = h.exited() {
            panic!("fuga exited early at iter {i} with status {status:?}");
        }
        h.send(keys[i % keys.len()]);
        if i % 7 == 0 {
            let (cols, rows) = sizes[(i / 7) % sizes.len()];
            h.resize(cols, rows);
        }
        if i % 25 == 0 {
            if let Some(kb) = h.rss_kb() {
                rss_max = rss_max.max(kb);
                if i >= iters / 2 {
                    if rss_mid == 0 {
                        rss_mid = kb; // first post-warmup sample
                    }
                    rss_late = kb;
                }
            }
        }
        std::thread::sleep(step);
    }

    let exited = h.quit(Duration::from_secs(8));
    assert!(
        exited,
        "fuga did not exit within 8s of 'q' (possible shutdown hang)"
    );
    assert!(!h.panicked(), "fuga panicked during soak");

    // Leak guard: the soak's SECOND HALF should be roughly flat. Cold-start
    // warmup (art/render caches filling) legitimately grows RSS to a plateau, so
    // comparing a cold baseline to the end measures warmup, not a leak. Comparing
    // the post-warmup midpoint to the end isolates runaway per-iteration growth.
    eprintln!(
        "soak: iters={iters} rss_baseline={rss_baseline:?}kB rss_mid={rss_mid}kB \
         rss_max={rss_max}kB rss_late={rss_late}kB"
    );
    if rss_mid > 0 && rss_late > 0 {
        let ratio = rss_late as f64 / rss_mid as f64;
        assert!(
            ratio < 1.25,
            "RSS grew {ratio:.2}x in the soak's second half (mid {rss_mid}kB -> late {rss_late}kB) \
             — possible leak"
        );
    }
}

/// Wave 5 performance gate. fuga's recurring perf bug is an idle-CPU busy-spin
/// (decisions.md 2026-06-24): a regression pegs a core at ~100% while the app
/// sits doing nothing. This measures CPU over an idle window (no input) and the
/// time-to-first-frame, as committed regression guards.
#[cfg(unix)]
#[test]
#[ignore = "perf gate: needs the release binary; run with --ignored"]
fn idle_cpu_stays_low_and_first_frame_is_fast() {
    let bin = resolve_bin();
    let mut h = Harness::spawn(&bin);
    let first_frame = match h.wait_first_frame(Duration::from_secs(15)) {
        Ok(d) => d,
        Err(e) => panic!("{e}"),
    };

    // Settle past warmup, then measure CPU over a quiet window with NO input.
    std::thread::sleep(Duration::from_millis(750));
    let tck = clk_tck();
    let c0 = h.cpu_jiffies();
    let w0 = Instant::now();
    std::thread::sleep(Duration::from_secs(4));
    assert!(!h.panicked(), "fuga panicked while idle");
    let c1 = h.cpu_jiffies();
    let elapsed = w0.elapsed().as_secs_f64();

    let cpu_pct = match (c0, c1) {
        (Some(a), Some(b)) => (b.saturating_sub(a) as f64) / (tck as f64 * elapsed) * 100.0,
        _ => -1.0,
    };
    eprintln!(
        "perf: first_frame={:.0}ms idle_cpu={:.1}% (window {:.1}s, CLK_TCK={tck})",
        first_frame.as_secs_f64() * 1000.0,
        cpu_pct,
        elapsed
    );

    let exited = h.quit(Duration::from_secs(8));
    assert!(exited, "fuga did not exit within 8s of 'q'");

    // Busy-spin regression guard. Idle should be ~0%; a regression pegs a core
    // (~100% of one). Ceiling is generous for CI scheduler jitter but far below
    // a spin. (-1.0 => /proc read failed; skip rather than false-fail.)
    if cpu_pct >= 0.0 {
        assert!(
            cpu_pct < 15.0,
            "idle CPU {cpu_pct:.1}% — busy-spin regression? (expected ~0%, see decisions 2026-06-24)"
        );
    }
    assert!(
        first_frame < Duration::from_secs(10),
        "first frame took {first_frame:?} — startup/probe regression?"
    );
}
