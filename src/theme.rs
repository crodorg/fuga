use ratatui::style::{Color, Modifier, Style};
use serde::Deserialize;
use std::collections::HashMap;

use crate::types::SourceMode;

/// Color tokens consumed across every widget. Add a token here, expose a
/// `Style` getter, swap callers from hardcoded styles. Add the token to each
/// preset.
#[derive(Debug, Clone)]
pub struct Theme {
    pub fg: Color,
    pub bg: Color,
    pub border: Color,
    pub accent: Color,
    pub selection_fg: Color,
    pub selection_bg: Color,
    pub dim: Color,
    pub header: Color,
    pub progress: Color,
    pub progress_track: Color,
    pub volume: Color,
    pub error: Color,
    pub leader_border: Color,
    /// When true, `with_source_accent` becomes a no-op so source switches
    /// don't repaint border/accent/selection in source-specific colors.
    /// Lets the user pick a pure-grayscale `monochrome` preset that stays
    /// monochrome regardless of which tab they're in.
    pub monochrome: bool,
}

impl Theme {
    pub fn from_config(cfg: &ThemeConfig) -> Self {
        let mut t = match cfg.name.as_str() {
            "dracula" => dracula(),
            "nord" => nord(),
            "gruvbox" => gruvbox(),
            "monochrome" | "mono" => monochrome(),
            _ => default_dark(),
        };
        for (k, v) in &cfg.colors {
            if let Some(c) = parse_color(v) {
                match k.as_str() {
                    "fg" => t.fg = c,
                    "bg" => t.bg = c,
                    "border" => t.border = c,
                    "accent" => t.accent = c,
                    "selection_fg" => t.selection_fg = c,
                    "selection_bg" => t.selection_bg = c,
                    "dim" => t.dim = c,
                    "header" => t.header = c,
                    "progress" => t.progress = c,
                    "progress_track" => t.progress_track = c,
                    "volume" => t.volume = c,
                    "error" => t.error = c,
                    "leader_border" => t.leader_border = c,
                    _ => {}
                }
            }
        }
        t
    }

    pub fn block_border(&self) -> Style {
        Style::default().fg(self.border)
    }
    pub fn accent(&self) -> Style {
        Style::default()
            .fg(self.accent)
            .add_modifier(Modifier::BOLD)
    }
    pub fn selection(&self) -> Style {
        Style::default()
            .fg(self.selection_fg)
            .bg(self.selection_bg)
            .add_modifier(Modifier::BOLD)
    }
    pub fn dim(&self) -> Style {
        // Color alone carries the "secondary" weight. The DIM intensity
        // modifier used to compound with a dark color and render the text
        // unreadable on dark terminals, so it's gone — tune via the `dim`
        // color token instead.
        Style::default().fg(self.dim)
    }
    pub fn header(&self) -> Style {
        Style::default()
            .fg(self.header)
            .add_modifier(Modifier::BOLD)
    }
    pub fn progress(&self) -> Style {
        Style::default().fg(self.progress)
    }
    pub fn progress_track(&self) -> Style {
        Style::default().fg(self.progress_track)
    }
    pub fn volume(&self) -> Style {
        Style::default()
            .fg(self.volume)
            .add_modifier(Modifier::BOLD)
    }
    pub fn error(&self) -> Style {
        Style::default().fg(self.error)
    }
    pub fn fg(&self) -> Style {
        Style::default().fg(self.fg)
    }

    /// Per-source palette swap. Mutates `border`, `accent`, and the
    /// selection row colors so the highlight matches the active source
    /// (e.g. red selection in YouTube, green in Spotify). `selection_fg`
    /// picks black or white per source-bg luminance for readable contrast.
    pub fn with_source_accent(mut self, mode: SourceMode) -> Self {
        if self.monochrome {
            return self;
        }
        let (border, accent, sel_bg, sel_fg) = match mode {
            SourceMode::Local => (Color::Gray, Color::White, Color::Gray, Color::Black),
            SourceMode::Spotify => (Color::Green, Color::Green, Color::Green, Color::Black),
            SourceMode::SomaFm => (Color::Yellow, Color::Yellow, Color::Yellow, Color::Black),
            SourceMode::Radio => (Color::Blue, Color::Blue, Color::Blue, Color::White),
            SourceMode::YouTube => (Color::Red, Color::Red, Color::Red, Color::White),
        };
        self.border = border;
        self.accent = accent;
        self.selection_bg = sel_bg;
        self.selection_fg = sel_fg;
        // Playback progress + volume bars track the active source too (green
        // Spotify, red YouTube, …) instead of staying a fixed green across
        // all modes.
        self.progress = accent;
        self.volume = accent;
        self
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Preset name: `default` | `dracula` | `nord` | `gruvbox`.
    pub name: String,
    /// Per-token overrides applied on top of the preset.
    pub colors: HashMap<String, String>,
}

fn default_dark() -> Theme {
    Theme {
        fg: Color::Reset,
        bg: Color::Reset,
        border: Color::DarkGray,
        accent: Color::Cyan,
        selection_fg: Color::Black,
        selection_bg: Color::Cyan,
        dim: Color::Gray,
        header: Color::Yellow,
        progress: Color::Green,
        progress_track: Color::DarkGray,
        volume: Color::Green,
        error: Color::Red,
        leader_border: Color::Magenta,
        monochrome: false,
    }
}

/// Pure grayscale preset. Every color token resolves to a gray shade so
/// the UI reads as monochrome under any terminal palette. `monochrome:
/// true` also suppresses per-source accent swaps so switching tabs
/// doesn't reintroduce green/red/blue highlights.
fn monochrome() -> Theme {
    Theme {
        fg: Color::Reset,
        bg: Color::Reset,
        border: Color::DarkGray,
        accent: Color::White,
        selection_fg: Color::Black,
        selection_bg: Color::Gray,
        dim: Color::DarkGray,
        header: Color::White,
        progress: Color::Gray,
        progress_track: Color::DarkGray,
        volume: Color::Gray,
        error: Color::White,
        leader_border: Color::White,
        monochrome: true,
    }
}

fn dracula() -> Theme {
    Theme {
        fg: rgb(0xF8, 0xF8, 0xF2),
        bg: rgb(0x28, 0x2A, 0x36),
        border: rgb(0x62, 0x72, 0xA4),
        accent: rgb(0xBD, 0x93, 0xF9),
        selection_fg: rgb(0xF8, 0xF8, 0xF2),
        selection_bg: rgb(0x44, 0x47, 0x5A),
        dim: rgb(0x62, 0x72, 0xA4),
        header: rgb(0xF1, 0xFA, 0x8C),
        progress: rgb(0x50, 0xFA, 0x7B),
        progress_track: rgb(0x44, 0x47, 0x5A),
        volume: rgb(0x8B, 0xE9, 0xFD),
        error: rgb(0xFF, 0x55, 0x55),
        leader_border: rgb(0xFF, 0x79, 0xC6),
        monochrome: false,
    }
}

fn nord() -> Theme {
    Theme {
        fg: rgb(0xEC, 0xEF, 0xF4),
        bg: rgb(0x2E, 0x34, 0x40),
        border: rgb(0x4C, 0x56, 0x6A),
        accent: rgb(0x88, 0xC0, 0xD0),
        selection_fg: rgb(0xEC, 0xEF, 0xF4),
        selection_bg: rgb(0x43, 0x4C, 0x5E),
        dim: rgb(0x4C, 0x56, 0x6A),
        header: rgb(0xEB, 0xCB, 0x8B),
        progress: rgb(0xA3, 0xBE, 0x8C),
        progress_track: rgb(0x43, 0x4C, 0x5E),
        volume: rgb(0x81, 0xA1, 0xC1),
        error: rgb(0xBF, 0x61, 0x6A),
        leader_border: rgb(0xB4, 0x8E, 0xAD),
        monochrome: false,
    }
}

fn gruvbox() -> Theme {
    Theme {
        fg: rgb(0xEB, 0xDB, 0xB2),
        bg: rgb(0x28, 0x28, 0x28),
        border: rgb(0x50, 0x49, 0x45),
        accent: rgb(0xFA, 0xBD, 0x2F),
        selection_fg: rgb(0x28, 0x28, 0x28),
        selection_bg: rgb(0xFA, 0xBD, 0x2F),
        dim: rgb(0x92, 0x83, 0x74),
        header: rgb(0xB8, 0xBB, 0x26),
        progress: rgb(0xB8, 0xBB, 0x26),
        progress_track: rgb(0x50, 0x49, 0x45),
        volume: rgb(0x83, 0xA5, 0x98),
        error: rgb(0xFB, 0x49, 0x34),
        leader_border: rgb(0xD3, 0x86, 0x9B),
        monochrome: false,
    }
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Parse `"#RRGGBB"`, `"RRGGBB"`, or a named ANSI color (`red`, `cyan`, ...).
fn parse_color(s: &str) -> Option<Color> {
    let trimmed = s.trim();
    if let Some(hex) = trimmed.strip_prefix('#') {
        return parse_hex(hex);
    }
    if trimmed.len() == 6 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        return parse_hex(trimmed);
    }
    match trimmed.to_ascii_lowercase().as_str() {
        "reset" => Some(Color::Reset),
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        "lightred" => Some(Color::LightRed),
        "lightgreen" => Some(Color::LightGreen),
        "lightyellow" => Some(Color::LightYellow),
        "lightblue" => Some(Color::LightBlue),
        "lightmagenta" => Some(Color::LightMagenta),
        "lightcyan" => Some(Color::LightCyan),
        "white" => Some(Color::White),
        _ => None,
    }
}

fn parse_hex(hex: &str) -> Option<Color> {
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(Color::Rgb(r, g, b))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_with_hash() {
        assert_eq!(parse_color("#ff8800"), Some(Color::Rgb(0xff, 0x88, 0x00)));
    }

    #[test]
    fn parses_named() {
        assert_eq!(parse_color("Cyan"), Some(Color::Cyan));
    }

    #[test]
    fn override_applies_on_top_of_preset() {
        let mut cfg = ThemeConfig {
            name: "default".into(),
            colors: HashMap::new(),
        };
        cfg.colors.insert("accent".into(), "#ff00ff".into());
        let t = Theme::from_config(&cfg);
        assert_eq!(t.accent, Color::Rgb(0xff, 0x00, 0xff));
    }

    #[test]
    fn unknown_preset_falls_back_to_default() {
        let cfg = ThemeConfig {
            name: "weird-name".into(),
            colors: HashMap::new(),
        };
        let t = Theme::from_config(&cfg);
        assert_eq!(t.accent, Color::Cyan);
    }
}
