use anyhow::Result;
use ratatui_image::picker::{Picker, ProtocolType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThumbMode {
    Kitty,
    /// Sixel for now-playing big art only. Per-row inline thumbs are disabled
    /// because terminals that support Sixel (xterm, foot, mlterm) typically
    /// don't anchor sixel cells to text the way Kitty's Unicode placeholders
    /// do — row thumbs would smear during scroll.
    Sixel,
    Halfblocks,
    Off,
}

impl ThumbMode {
    pub fn cycle(self) -> Self {
        match self {
            ThumbMode::Kitty => ThumbMode::Sixel,
            ThumbMode::Sixel => ThumbMode::Halfblocks,
            ThumbMode::Halfblocks => ThumbMode::Off,
            ThumbMode::Off => ThumbMode::Kitty,
        }
    }

    pub fn from_config(s: &str) -> Self {
        match s.to_ascii_lowercase().as_str() {
            "kitty" => ThumbMode::Kitty,
            "sixel" => ThumbMode::Sixel,
            "octant" | "halfblocks" => ThumbMode::Halfblocks,
            "off" | "none" | "text" => ThumbMode::Off,
            _ => ThumbMode::Kitty,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ThumbMode::Kitty => "kitty",
            ThumbMode::Sixel => "sixel",
            ThumbMode::Halfblocks => "halfblocks",
            ThumbMode::Off => "off",
        }
    }

    /// True if inline per-row thumbnails should render in this mode. Sixel
    /// can't anchor cells to scrolling rows reliably; only Kitty + Halfblocks
    /// keep row art on.
    pub fn supports_row_thumbs(&self) -> bool {
        matches!(self, ThumbMode::Kitty | ThumbMode::Halfblocks)
    }
}

pub struct Term {
    pub picker: Picker,
    pub mode: ThumbMode,
    pub kitty_capable: bool,
    /// Startup skipped the escape probes (unfocused tmux pane, see `probe`)
    /// and env markers didn't settle kitty: the run loop should probe on the
    /// first FocusGained, when tmux routes replies back to this pane.
    pub deferred_probe: bool,
}

impl Term {
    /// Probe the terminal. Must be called after entering the alt screen but before the event
    /// reader starts.
    pub fn probe(config_mode: ThumbMode) -> Result<Self> {
        // A pane nobody is looking at can't hear escape-query replies: tmux
        // routes the outer terminal's answers to each attached client's
        // *active* pane, which strips the escape framing and takes the
        // printable remainder as keystrokes — probing from an unfocused pane
        // types junk into whatever pane the user has focused, in any window.
        // Detached sessions starve the same way. So: no escape queries at all
        // unless we're the focused pane of an attached client.
        if std::env::var_os("TMUX").is_some() && !pane_focused() {
            return Ok(Self::probe_unfocused(config_mode));
        }
        let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| {
            // Query timed out: ratatui-image abandons its stdin reader thread,
            // which stays parked in a blocking read holding the tty's reader
            // lock — starving every later read on the fd (our selfprobe,
            // crossterm's event loop) and clobbering termios whenever it
            // finally wakes. Feed it the DSR terminator it wants: a bare
            // (un-wrapped) status query that tmux's own vt answers directly
            // into the pane, no outer terminal involved. The thread eats the
            // reply, exits its loop, and restores termios before we touch the
            // tty again. Happens only in reply-starved panes (detached
            // session, non-active tmux pane); an answered query never hits
            // this path.
            #[cfg(unix)]
            {
                let q: &[u8] = b"\x1b[5n";
                let _ = unsafe { libc::write(libc::STDOUT_FILENO, q.as_ptr().cast(), q.len()) };
                std::thread::sleep(std::time::Duration::from_millis(150));
            }
            Picker::halfblocks()
        });
        // ratatui-image's probe sets this pane's allow-passthrough to "on"
        // (visible-only), so kitty re-transmits are dropped while our tmux
        // window is hidden and art comes back broken after a window switch.
        // Upgrade the pane option to "all".
        //
        // allow-passthrough=all is necessary but not sufficient: on a
        // window-switch return tmux repaints the pane's text cells but never
        // replays the once-transmitted kitty bitmap, so the art shows as bare
        // (reddish) placeholder glyphs until something re-transmits. We force a
        // re-transmit on crossterm's FocusGained (run_loop), but tmux only
        // delivers FocusGained to the pane when focus-events is on — and it
        // defaults to off. So enable it here too. See decisions.md 2026-06-26.
        if std::env::var_os("TMUX").is_some() {
            tmux_enable_passthrough_and_focus();
        }
        let mut kitty_capable = matches!(picker.protocol_type(), ProtocolType::Kitty);

        // ratatui-image's capability query bundles every probe into a single
        // tmux passthrough and stops reading at the first DSR. tmux broadcasts
        // the query to *every* attached client, so a non-kitty client's DSR can
        // beat our real terminal's kitty reply — the kitty OK is never read and
        // Kitty is mis-detected as unsupported (art falls back to halfblocks).
        // When ratatui reports non-kitty inside tmux, re-probe ourselves two
        // ways: (1) the kitty query in its own passthrough wrapper, reading the
        // whole response window instead of bailing at the first DSR (fixes st,
        // see decisions.md 2026-06-19); (2) an env-var signal for terminals
        // whose kitty reply tmux never forwards back to the pane at all — e.g.
        // Ghostty under tmux 3.6, where the selfprobe times out but
        // GHOSTTY_RESOURCES_DIR survives into the pane env (see 2026-07-03).
        if !kitty_capable
            && std::env::var_os("TMUX").is_some()
            && (kitty_selfprobe() || kitty_terminal_env())
        {
            kitty_capable = true;
            picker.set_protocol_type(ProtocolType::Kitty);
        }

        let mode = match config_mode {
            ThumbMode::Kitty if !kitty_capable => ThumbMode::Halfblocks,
            other => other,
        };

        Ok(Self {
            picker,
            mode,
            kitty_capable,
            deferred_probe: false,
        })
    }

    /// Escape-free probe for panes that can't hear a reply (unfocused pane,
    /// hidden window, detached session). Capability comes from env markers;
    /// markerless kitty terminals (st) start halfblocks with `deferred_probe`
    /// set so the run loop re-probes on the first FocusGained.
    fn probe_unfocused(config_mode: ThumbMode) -> Self {
        // halfblocks() does no terminal I/O — env checks plus a tmux
        // `allow-passthrough on` subprocess — and sets its internal is_tmux
        // flag so kitty transmits stay passthrough-wrapped. Its (10,20) font
        // size matches the query default and is exact for st and ghostty.
        let picker = Picker::halfblocks();
        tmux_enable_passthrough_and_focus();
        let kitty_capable = kitty_terminal_env();
        let mut term = Self {
            picker,
            mode: config_mode,
            kitty_capable,
            deferred_probe: !kitty_capable && config_mode == ThumbMode::Kitty,
        };
        // Reuse the mode→protocol mapping (kitty falls back to halfblocks
        // when not capable); halfblocks()'s default protocol is not
        // query-grounded, so overwrite it.
        term.apply_mode(config_mode);
        tracing::debug!(
            kitty = kitty_capable,
            deferred = term.deferred_probe,
            "escape probes skipped (unfocused tmux pane); capability from env markers"
        );
        term
    }

    pub fn apply_mode(&mut self, mode: ThumbMode) {
        self.mode = match mode {
            ThumbMode::Kitty if !self.kitty_capable => ThumbMode::Halfblocks,
            other => other,
        };
        let proto = match self.mode {
            ThumbMode::Kitty => ProtocolType::Kitty,
            ThumbMode::Sixel => ProtocolType::Sixel,
            ThumbMode::Halfblocks => ProtocolType::Halfblocks,
            ThumbMode::Off => ProtocolType::Halfblocks,
        };
        self.picker.set_protocol_type(proto);
    }
}

/// The exact kitty-graphics query reply ratatui-image looks for (`i=31;OK`).
#[cfg(unix)]
const KITTY_OK: &[u8] = b"\x1b_Gi=31;OK\x1b\\";

/// The kitty graphics `a=q` query alone in one tmux passthrough wrapper. No
/// DSR terminator: readers scan the whole response window so a racing
/// client's DSR can't cut them off early (see decisions.md 2026-06-19).
/// Shared by `kitty_selfprobe` (startup, reads the tty) and the deferred
/// probe (post-startup, reply arrives as key events — `KittyReplyScanner`).
pub const KITTY_QUERY_TMUX: &[u8] =
    b"\x1bPtmux;\x1b\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\x1b\\\x1b\\";

/// True when this tmux pane is the one the attached client is looking at:
/// `display-message` output `pane_active:window_active:session_attached` is
/// exactly `1:1:N` with N >= 1. Pure so it's unit-testable without tmux.
fn parse_focus_output(s: &str) -> bool {
    let mut it = s.trim().split(':');
    let pane = it.next() == Some("1");
    let window = it.next() == Some("1");
    let attached = it
        .next()
        .and_then(|n| n.parse::<u32>().ok())
        .is_some_and(|n| n >= 1);
    pane && window && attached && it.next().is_none()
}

/// Whether this pane is the focused pane of an attached tmux client — the
/// only launch context where an escape-query reply can reach us. Any failure
/// to determine (no TMUX_PANE, tmux error, garbage output) reads as
/// unfocused: skipping the probe costs halfblock art at worst, while probing
/// blind types junk into the user's focused pane.
fn pane_focused() -> bool {
    let Some(pane) = std::env::var_os("TMUX_PANE") else {
        return false;
    };
    std::process::Command::new("tmux")
        .arg("display-message")
        .arg("-p")
        .arg("-t")
        .arg(&pane)
        .arg("#{pane_active}:#{window_active}:#{session_attached}")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .is_some_and(|o| parse_focus_output(&String::from_utf8_lossy(&o.stdout)))
}

/// The two tmux options every fuga-in-tmux startup needs: allow-passthrough
/// upgraded to "all" (ratatui-image's own detection sets visible-only "on",
/// under which kitty re-transmits from a hidden window are dropped) and
/// focus-events on (off by default; the art re-transmit on focus-return and
/// the deferred probe both need FocusGained delivered). See decisions.md
/// 2026-06-26.
fn tmux_enable_passthrough_and_focus() {
    let _ = std::process::Command::new("tmux")
        .args(["set", "-p", "allow-passthrough", "all"])
        .output();
    let _ = std::process::Command::new("tmux")
        .args(["set", "-g", "focus-events", "on"])
        .output();
}

/// Scans key-event characters for the kitty `i=31;OK` reply during the
/// deferred probe's capture window: once the event loop owns stdin, a
/// terminal reply can't be read off the tty (the line discipline serializes
/// readers — see `kitty_selfprobe_fd`) and instead surfaces through
/// crossterm as ordinary key events. Only characters that can appear in the
/// reply frame are claimed; everything else stays user input.
#[derive(Default)]
pub struct KittyReplyScanner {
    seen: String,
}

impl KittyReplyScanner {
    /// Text form of the `KITTY_OK` reply; the `31` is the image id from
    /// `KITTY_QUERY_TMUX` — the three constants must move in lockstep.
    const NEEDLE: &'static str = "i=31;OK";
    /// Every char of the passthrough-stripped reply frame.
    const FRAME_CHARS: &'static str = "Gi=31;OK_\\";

    /// Feed one key char. True = reply-frame char, swallow it (then check
    /// [`Self::matched`]); false = unrelated user input, pass it through.
    pub fn feed(&mut self, c: char) -> bool {
        if Self::FRAME_CHARS.contains(c) {
            self.seen.push(c);
            true
        } else {
            false
        }
    }

    pub fn matched(&self) -> bool {
        self.seen.contains(Self::NEEDLE)
    }
}

/// Send a Kitty graphics `a=q` query in its own tmux passthrough wrapper and
/// read the whole response window (not just up to the first DSR) for the
/// `i=31;OK` reply. Returns true if the real terminal answered. This sidesteps
/// the multi-client DSR race that defeats ratatui-image's bundled query inside
/// tmux (see `Term::probe`). TTY state is saved and restored around the probe;
/// a non-answering terminal costs one 500 ms timeout.
#[cfg(unix)]
fn kitty_selfprobe() -> bool {
    kitty_selfprobe_fd(libc::STDIN_FILENO, libc::STDOUT_FILENO)
}

/// Core of [`kitty_selfprobe`], parameterized over the tty fds so tests can
/// aim it at a pty. Reads with `O_NONBLOCK` + `poll` rather than VMIN/VTIME:
/// the line discipline serializes tty readers, so if another thread is parked
/// in a blocking read on the same fd (ratatui-image's abandoned query-reader
/// thread, see `Term::probe`), a plain read would queue behind its lock
/// indefinitely — VTIME never even starts. A nonblocking read returns EAGAIN
/// instead of queueing, and poll's timeout bounds the wait, so the 500 ms
/// deadline holds no matter what else has the tty.
#[cfg(unix)]
fn kitty_selfprobe_fd(read_fd: libc::c_int, write_fd: libc::c_int) -> bool {
    use std::time::{Duration, Instant};

    // Save TTY state; drop canonical buffering + echo so the reply arrives
    // raw and unechoed.
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(read_fd, &mut orig) } != 0 {
        return false;
    }
    let mut raw = orig;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    if unsafe { libc::tcsetattr(read_fd, libc::TCSANOW, &raw) } != 0 {
        return false;
    }
    let orig_flags = unsafe { libc::fcntl(read_fd, libc::F_GETFL) };
    if orig_flags != -1 {
        unsafe { libc::fcntl(read_fd, libc::F_SETFL, orig_flags | libc::O_NONBLOCK) };
    }

    // Kitty query alone in one passthrough wrapper. No DSR terminator: we read
    // the whole window so a racing client's DSR can't cut us off early.
    {
        let q = KITTY_QUERY_TMUX;
        let _ = unsafe { libc::write(write_fd, q.as_ptr().cast(), q.len()) };
    }

    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    let deadline = Instant::now() + Duration::from_millis(500);
    let found = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break false;
        }
        let mut pfd = libc::pollfd {
            fd: read_fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, remaining.as_millis() as libc::c_int) };
        if rc < 0 {
            break false;
        }
        if rc == 0 {
            continue; // poll timeout → deadline check exits next iteration
        }
        let n = unsafe { libc::read(read_fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n > 0 {
            buf.extend_from_slice(&chunk[..n as usize]);
            if buf.windows(KITTY_OK.len()).any(|w| w == KITTY_OK) {
                break true;
            }
        } else if n < 0 {
            let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
            // EAGAIN: nothing buffered, or a competing reader consumed it.
            if errno != libc::EAGAIN && errno != libc::EWOULDBLOCK {
                break false;
            }
        } else {
            break false; // EOF
        }
    };

    if orig_flags != -1 {
        unsafe { libc::fcntl(read_fd, libc::F_SETFL, orig_flags) };
    }
    unsafe { libc::tcsetattr(read_fd, libc::TCSANOW, &orig) };
    found
}

#[cfg(not(unix))]
fn kitty_selfprobe() -> bool {
    false
}

/// Some terminals speak the Kitty graphics protocol but can be recognized by an
/// environment variable that survives into tmux — where `TERM`/`TERM_PROGRAM`
/// are rewritten to tmux's own values and so can't be trusted. Consulted as a
/// fallback capability signal inside tmux when the escape-query probe races or
/// fails: e.g. Ghostty, whose Kitty query reply tmux doesn't forward back to the
/// pane, so `kitty_selfprobe` never sees the `i=31;OK`. See decisions.md
/// 2026-07-03.
fn kitty_terminal_env() -> bool {
    kitty_env_present(|name| std::env::var_os(name).is_some())
}

/// Pure core of [`kitty_terminal_env`]: true when `lookup` reports any known
/// Kitty-capable terminal's marker variable present. Split out so the marker
/// list is unit-testable without mutating process env.
fn kitty_env_present(lookup: impl Fn(&str) -> bool) -> bool {
    // Ghostty exports GHOSTTY_RESOURCES_DIR / GHOSTTY_BIN_DIR; kitty exports
    // KITTY_WINDOW_ID; WezTerm exports WEZTERM_EXECUTABLE / WEZTERM_PANE. All are
    // plain env vars inherited by pane processes, so they persist inside tmux.
    // FUGA_ASSUME_KITTY is the explicit user override for launch contexts where
    // no probe can work: tmux routes query replies to the *active* pane, so a
    // launch in a non-focused pane (or a detached session) can never hear the
    // terminal answer, and once the event loop owns stdin a late reply would
    // read as keystrokes — capability must be known up front.
    [
        "GHOSTTY_RESOURCES_DIR",
        "GHOSTTY_BIN_DIR",
        "KITTY_WINDOW_ID",
        "WEZTERM_EXECUTABLE",
        "WEZTERM_PANE",
        "FUGA_ASSUME_KITTY",
    ]
    .iter()
    .any(|name| lookup(name))
}

#[cfg(test)]
mod tests {
    use super::ThumbMode;

    #[test]
    fn from_config_parses_known_modes_and_defaults_to_kitty() {
        assert_eq!(ThumbMode::from_config("kitty"), ThumbMode::Kitty);
        assert_eq!(ThumbMode::from_config("Sixel"), ThumbMode::Sixel);
        assert_eq!(ThumbMode::from_config("halfblocks"), ThumbMode::Halfblocks);
        assert_eq!(ThumbMode::from_config("octant"), ThumbMode::Halfblocks);
        assert_eq!(ThumbMode::from_config("off"), ThumbMode::Off);
        assert_eq!(ThumbMode::from_config("none"), ThumbMode::Off);
        assert_eq!(ThumbMode::from_config("text"), ThumbMode::Off);
        assert_eq!(ThumbMode::from_config("nonsense"), ThumbMode::Kitty);
    }

    #[test]
    fn cycle_is_a_four_state_loop() {
        assert_eq!(ThumbMode::Kitty.cycle(), ThumbMode::Sixel);
        assert_eq!(ThumbMode::Sixel.cycle(), ThumbMode::Halfblocks);
        assert_eq!(ThumbMode::Halfblocks.cycle(), ThumbMode::Off);
        assert_eq!(ThumbMode::Off.cycle(), ThumbMode::Kitty);
    }

    #[test]
    fn kitty_env_present_matches_known_terminals_only() {
        // Ghostty / kitty / WezTerm markers each trip it.
        assert!(super::kitty_env_present(|n| n == "GHOSTTY_RESOURCES_DIR"));
        assert!(super::kitty_env_present(|n| n == "GHOSTTY_BIN_DIR"));
        assert!(super::kitty_env_present(|n| n == "KITTY_WINDOW_ID"));
        assert!(super::kitty_env_present(|n| n == "WEZTERM_PANE"));
        // The explicit override for probe-blind launch contexts.
        assert!(super::kitty_env_present(|n| n == "FUGA_ASSUME_KITTY"));
        // Nothing present, or only tmux-clobbered generics, does not.
        assert!(!super::kitty_env_present(|_| false));
        assert!(!super::kitty_env_present(
            |n| n == "TERM_PROGRAM" || n == "TERM"
        ));
    }

    /// Regression: the selfprobe must honor its deadline even when another
    /// thread is parked in a blocking read on the same tty. ratatui-image's
    /// timed-out query leaves exactly such an orphan reader behind, and the
    /// line discipline serializes tty readers — the old VMIN=0/VTIME=1 loop
    /// queued behind the orphan's lock forever (main thread wedged in
    /// n_tty_read, fuga never drew a frame).
    #[cfg(unix)]
    #[test]
    fn selfprobe_returns_within_deadline_despite_blocked_reader() {
        use std::time::{Duration, Instant};

        let (mut master, mut slave) = (0 as libc::c_int, 0 as libc::c_int);
        let ok = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(ok, 0, "openpty failed");

        // Orphan: a blocking read on the slave that never completes (nothing
        // writes to the master), holding the tty reader lock — the state
        // ratatui-image's abandoned query thread leaves behind.
        let orphan_fd = slave;
        std::thread::spawn(move || {
            let mut b = [0u8; 64];
            unsafe { libc::read(orphan_fd, b.as_mut_ptr().cast(), b.len()) };
        });
        std::thread::sleep(Duration::from_millis(100)); // let it enter the read

        let (tx, rx) = std::sync::mpsc::channel();
        let probe_fd = slave;
        let t0 = Instant::now();
        std::thread::spawn(move || {
            let _ = tx.send(super::kitty_selfprobe_fd(probe_fd, probe_fd));
        });
        match rx.recv_timeout(Duration::from_secs(3)) {
            Ok(found) => {
                assert!(!found, "no terminal answered; probe must report false");
                assert!(
                    t0.elapsed() < Duration::from_millis(1500),
                    "probe took {:?}, deadline is 500ms",
                    t0.elapsed()
                );
            }
            Err(_) => panic!("kitty_selfprobe blocked past 3s behind the orphan reader"),
        }
    }

    #[test]
    fn parse_focus_output_accepts_only_focused_pane_of_attached_session() {
        assert!(super::parse_focus_output("1:1:1"));
        assert!(super::parse_focus_output("1:1:2\n")); // two clients, newline from -p
        assert!(!super::parse_focus_output("0:1:1")); // pane not active
        assert!(!super::parse_focus_output("1:0:1")); // window hidden
        assert!(!super::parse_focus_output("1:1:0")); // session detached
        assert!(!super::parse_focus_output(""));
        assert!(!super::parse_focus_output("junk"));
        assert!(!super::parse_focus_output("1:1"));
        assert!(!super::parse_focus_output("1:1:1:1"));
    }

    #[test]
    fn reply_scanner_matches_kitty_ok_and_passes_user_keys() {
        let mut s = super::KittyReplyScanner::default();
        for c in "Gi=3".chars() {
            assert!(s.feed(c), "frame char {c:?} must be swallowed");
        }
        // A user keystroke mid-window passes through and doesn't break the match.
        assert!(!s.feed('j'));
        assert!(!s.matched());
        for c in "1;OK".chars() {
            assert!(s.feed(c));
        }
        assert!(s.matched());
    }

    #[test]
    fn reply_scanner_swallows_frame_wrappers_without_matching() {
        let mut s = super::KittyReplyScanner::default();
        assert!(s.feed('_')); // Alt+_ from the APC intro
        assert!(s.feed('\\')); // Alt+\ from the ST terminator
        assert!(!s.matched());
    }

    #[test]
    fn as_str_roundtrips_through_from_config() {
        for m in [
            ThumbMode::Kitty,
            ThumbMode::Sixel,
            ThumbMode::Halfblocks,
            ThumbMode::Off,
        ] {
            assert_eq!(ThumbMode::from_config(m.as_str()), m);
        }
    }

    #[test]
    fn only_kitty_and_halfblocks_anchor_row_thumbs() {
        assert!(ThumbMode::Kitty.supports_row_thumbs());
        assert!(ThumbMode::Halfblocks.supports_row_thumbs());
        assert!(!ThumbMode::Sixel.supports_row_thumbs());
        assert!(!ThumbMode::Off.supports_row_thumbs());
    }
}
