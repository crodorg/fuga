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
}

impl Term {
    /// Probe the terminal. Must be called after entering the alt screen but before the event
    /// reader starts.
    pub fn probe(config_mode: ThumbMode) -> Result<Self> {
        let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
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
            let _ = std::process::Command::new("tmux")
                .args(["set", "-p", "allow-passthrough", "all"])
                .output();
            let _ = std::process::Command::new("tmux")
                .args(["set", "-g", "focus-events", "on"])
                .output();
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
        })
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

/// Send a Kitty graphics `a=q` query in its own tmux passthrough wrapper and
/// read the whole response window (not just up to the first DSR) for the
/// `i=31;OK` reply. Returns true if the real terminal answered. This sidesteps
/// the multi-client DSR race that defeats ratatui-image's bundled query inside
/// tmux (see `Term::probe`). TTY state is saved and restored around the probe;
/// a non-answering terminal costs one 500 ms timeout.
#[cfg(unix)]
fn kitty_selfprobe() -> bool {
    use std::io::Write;
    use std::time::{Duration, Instant};

    let fd = libc::STDIN_FILENO;

    // Save TTY state, switch to raw with a 100 ms read timeout (VMIN=0/VTIME=1)
    // so reads return promptly and never block past the deadline.
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
        return false;
    }
    let mut raw = orig;
    raw.c_lflag &= !(libc::ICANON | libc::ECHO);
    raw.c_cc[libc::VMIN] = 0;
    raw.c_cc[libc::VTIME] = 1;
    if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
        return false;
    }

    // Kitty query alone in one passthrough wrapper. No DSR terminator: we read
    // the whole window so a racing client's DSR can't cut us off early.
    {
        let mut out = std::io::stdout().lock();
        let _ =
            out.write_all(b"\x1bPtmux;\x1b\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\x1b\\\x1b\\");
        let _ = out.flush();
    }

    let mut buf = Vec::with_capacity(256);
    let mut chunk = [0u8; 256];
    let deadline = Instant::now() + Duration::from_millis(500);
    let found = loop {
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), chunk.len()) };
        if n > 0 {
            buf.extend_from_slice(&chunk[..n as usize]);
            if buf.windows(KITTY_OK.len()).any(|w| w == KITTY_OK) {
                break true;
            }
        }
        if Instant::now() >= deadline {
            break false;
        }
    };

    unsafe { libc::tcsetattr(fd, libc::TCSANOW, &orig) };
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
    [
        "GHOSTTY_RESOURCES_DIR",
        "GHOSTTY_BIN_DIR",
        "KITTY_WINDOW_ID",
        "WEZTERM_EXECUTABLE",
        "WEZTERM_PANE",
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
        // Nothing present, or only tmux-clobbered generics, does not.
        assert!(!super::kitty_env_present(|_| false));
        assert!(!super::kitty_env_present(
            |n| n == "TERM_PROGRAM" || n == "TERM"
        ));
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
