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
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        // ratatui-image's probe sets this pane's allow-passthrough to "on"
        // (visible-only), so kitty re-transmits are dropped while our tmux
        // window is hidden and art comes back broken after a window switch.
        // Upgrade the pane option to "all".
        if std::env::var_os("TMUX").is_some() {
            let _ = std::process::Command::new("tmux")
                .args(["set", "-p", "allow-passthrough", "all"])
                .output();
        }
        let kitty_capable = matches!(picker.protocol_type(), ProtocolType::Kitty);

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
