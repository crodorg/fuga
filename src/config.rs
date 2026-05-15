#![allow(dead_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub mpd: MpdConfig,
    pub ui: UiConfig,
    pub paths: Paths,
    pub somafm: SomaFmConfig,
    pub spotify: SpotifyConfig,
    pub youtube: YouTubeConfig,
    pub radio: Vec<crate::source::radio::RadioStation>,
    pub keybindings: KeyBindings,
    pub theme: crate::theme::ThemeConfig,
    pub hooks: Hooks,
}

/// External shell commands invoked on lifecycle events. Each receives state
/// via FUGA_* environment variables (see README).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Hooks {
    pub on_track_change: Option<String>,
    pub on_source_switch: Option<String>,
    pub on_startup: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MpdConfig {
    pub host: String,
    pub port: u16,
    pub password: Option<String>,
    /// Filesystem root for the MPD library. When set, fuga falls back to
    /// sidecar files (`cover.jpg`, `folder.jpg`, ...) in a track's parent
    /// directory if MPD's `albumart`/`readpicture` returns nothing. Match
    /// the value in mpd.conf's `music_directory`.
    pub music_directory: Option<PathBuf>,
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 6600,
            password: None,
            music_directory: None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub default_tab: String,
    pub thumb_mode: String,
    /// Modes the `T` key cycles through. Default = `["kitty", "off"]` so a
    /// single press toggles art on/off. Users in halfblocks/sixel terminals
    /// can add those modes; users who never want them can leave them out.
    /// Always includes the configured startup `thumb_mode` even if absent
    /// from this list (so you start in a valid cycle position).
    pub thumb_cycle: Vec<String>,
    pub thumb_cells: u16,
    pub fps_cap: u16,
    /// Fields shown per row in the Queue tab. Recognized: title, artist,
    /// album, duration, source. Order = display order; missing values skip.
    pub queue_columns: Vec<String>,
    /// Same idea for Library track lists.
    pub library_columns: Vec<String>,
    /// Configured top-level tab list (rmpc-style category bar). When empty,
    /// `App::new` derives a default set from the registered sources.
    /// Recognized ids: queue, albums, artists, playlists, stations, radio,
    /// somafm, search. Unknown ids are ignored with a warning.
    pub tabs: Vec<String>,
    /// Tab bar horizontal alignment: "center" | "left" | "right".
    pub tab_alignment: String,
    /// How merged source lists render: "grouped" | "interleaved_dedupe" |
    /// "interleaved". Slice 1 always behaves as if "grouped".
    pub multi_source_layout: String,
    /// `false` (default): one merged Stations tab combining radio + somafm.
    /// `true`: two separate tabs.
    pub radio_split: bool,
    /// Default state of the now-playing art panel. `false` (default) =
    /// big art; `true` = collapsed to bottom-bar height. Overridden by
    /// the persisted state at `$XDG_DATA_HOME/fuga/state.json` once the
    /// user has toggled at least once.
    pub art_collapsed: bool,
    /// Percentage of available vertical space (terminal height below the
    /// 3-row tab bar) for the now-playing art panel. 100 = the panel's
    /// top edge sits flush against the tab bar's bottom border. Clamped
    /// at use to [20, 100].
    pub art_height_pct: u16,
    /// Percentage of available horizontal space (terminal width minus a
    /// 24-cell margin reserved for the bottom-bar text on the left) for
    /// the now-playing art panel. 100 = panel runs from that margin to
    /// the right edge. Clamped at use to [15, 100].
    pub art_width_pct: u16,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            default_tab: "library".into(),
            thumb_mode: "kitty".into(),
            thumb_cycle: vec!["kitty".into(), "off".into()],
            thumb_cells: 2,
            fps_cap: 30,
            queue_columns: vec![
                "title".into(),
                "artist".into(),
                "album".into(),
                "duration".into(),
            ],
            library_columns: vec!["artist".into(), "album".into(), "title".into()],
            tabs: Vec::new(),
            tab_alignment: "center".into(),
            multi_source_layout: "grouped".into(),
            radio_split: false,
            art_collapsed: false,
            // Defaults that roughly preserve the prior look on a typical
            // 130x50 terminal: ~33 rows tall, ~42 cells wide.
            art_height_pct: 70,
            art_width_pct: 40,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabAlignment {
    Left,
    Center,
    Right,
}

impl TabAlignment {
    pub fn from_str(s: &str) -> Self {
        match s {
            "left" => TabAlignment::Left,
            "right" => TabAlignment::Right,
            _ => TabAlignment::Center,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SomaFmConfig {
    pub enabled: bool,
    pub cache_ttl_hours: u64,
}

impl Default for SomaFmConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            cache_ttl_hours: 6,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SpotifyConfig {
    pub enabled: bool,
    pub client_id: String,
    pub device_name: String,
    /// Stream quality. Accepts: "low" (96 kbps OGG), "normal" (160 kbps OGG),
    /// "high" (320 kbps OGG), "lossless" / "flac" (alias for "high" — Spotify
    /// HiFi tier isn't supported by librespot 0.8, falls back to 320 kbps OGG
    /// with a startup log line). Legacy "96" / "160" / "320" still parse.
    pub quality: String,
    /// Legacy alias for `quality`. Retained so existing configs keep working.
    /// If `quality` is unset and `bitrate` is present, `bitrate` wins.
    pub bitrate: String,
    pub volume_normalisation: bool,
    pub redirect_port: u16,
}

impl Default for SpotifyConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            client_id: String::new(),
            device_name: "fuga".into(),
            quality: "lossless".into(),
            bitrate: String::new(),
            volume_normalisation: true,
            redirect_port: 8888,
        }
    }
}

impl SpotifyConfig {
    /// Resolve the configured quality to a librespot bitrate string. Accepts
    /// human aliases ("lossless" / "flac" / "high" / "normal" / "low") and
    /// numeric values. Anything unrecognized falls back to "320".
    pub fn resolved_bitrate(&self) -> &'static str {
        let raw = if !self.bitrate.is_empty() {
            self.bitrate.as_str()
        } else {
            self.quality.as_str()
        };
        match raw.to_ascii_lowercase().as_str() {
            "low" | "96" => "96",
            "normal" | "160" => "160",
            "high" | "320" => "320",
            "lossless" | "flac" | "hifi" => "320",
            _ => "320",
        }
    }

    /// True when the user asked for FLAC/lossless but librespot can't deliver
    /// it. Used so the startup log line is honest about the fallback.
    pub fn lossless_unsupported(&self) -> bool {
        let raw = if !self.bitrate.is_empty() {
            self.bitrate.as_str()
        } else {
            self.quality.as_str()
        };
        matches!(
            raw.to_ascii_lowercase().as_str(),
            "lossless" | "flac" | "hifi"
        )
    }
}

/// YouTube via `yt-dlp` shell-out. Disabled by default — requires
/// `yt-dlp` installed separately. See README's Legal section for ToS notes.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct YouTubeConfig {
    pub enabled: bool,
    /// Path to the `yt-dlp` binary. Default `"yt-dlp"` resolves via PATH.
    pub yt_dlp_bin: String,
}

impl Default for YouTubeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            yt_dlp_bin: "yt-dlp".into(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Paths {
    pub cache_dir: Option<PathBuf>,
    pub data_dir: Option<PathBuf>,
    pub config_dir: Option<PathBuf>,
}

/// Keybindings table — `[keybindings.global]` maps action_name -> key,
/// `[keybindings.leaders.<chord>]` maps next-chord -> { label, action }.
/// Action strings recognized: see `keys::parse_action`. Use form
/// `source_jump:spotify` or `tab:0` for parameterized actions.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct KeyBindings {
    pub global: HashMap<String, String>,
    pub leaders: HashMap<String, HashMap<String, LeaderEntry>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LeaderEntry {
    pub label: String,
    pub action: String,
}

impl KeyBindings {
    pub fn merge_defaults(&mut self) {
        for (k, v) in default_global() {
            self.global
                .entry((*k).to_string())
                .or_insert_with(|| (*v).to_string());
        }
        for (leader, sub) in default_leaders() {
            let entry = self.leaders.entry((*leader).to_string()).or_default();
            for (k, label, action) in sub.iter() {
                entry
                    .entry((*k).to_string())
                    .or_insert_with(|| LeaderEntry {
                        label: (*label).to_string(),
                        action: (*action).to_string(),
                    });
            }
        }
    }
}

fn default_global() -> &'static [(&'static str, &'static str)] {
    &[
        ("quit", "q"),
        ("down", "j"),
        ("up", "k"),
        ("page_down", "C-d"),
        ("page_up", "C-u"),
        ("bottom", "G"),
        ("next_tab", "Tab"),
        ("prev_tab", "BackTab"),
        ("next_tab_alt", "C-n"),
        ("prev_tab_alt", "C-p"),
        ("tab_1", "1"),
        ("tab_2", "2"),
        ("tab_3", "3"),
        ("tab_4", "4"),
        ("tab_5", "5"),
        ("tab_6", "6"),
        ("tab_7", "7"),
        ("tab_8", "8"),
        ("tab_9", "9"),
        ("focus_search", "s"),
        ("focus_command", ":"),
        ("activate", "Enter"),
        ("activate_alt", "l"),
        ("enqueue", "a"),
        ("back", "Esc"),
        ("back_alt", "h"),
        ("seek_back", "H"),
        ("seek_forward", "L"),
        ("play_pause", "Space"),
        ("next_track", "n"),
        ("prev_track", "p"),
        ("stop", "S"),
        ("refresh", "r"),
        ("toggle_thumb", "T"),
        ("cycle_source", "t"),
        ("vol_up", "+"),
        ("vol_down", "-"),
        ("toggle_help", "?"),
        ("toggle_like", "F"),
        ("open_devices", "d"),
        ("toggle_shuffle", "z"),
        ("cycle_repeat", "x"),
        ("open_sort", "o"),
        ("follow_playing", "f"),
        ("clear_queue", "C"),
        ("remove_from_queue", "D"),
        ("expand_art", "V"),
        ("open_action_menu", "m"),
        ("toggle_pin", "P"),
        ("filter_in_page", "/"),
        ("download_hovered", "Y"),
    ]
}

type DefaultLeaderEntry = (&'static str, &'static str, &'static str);
type DefaultLeaderGroup = (&'static str, &'static [DefaultLeaderEntry]);

fn default_leaders() -> &'static [DefaultLeaderGroup] {
    &[(
        "g",
        &[
            ("g", "top", "top"),
            ("l", "Local", "source_jump:local"),
            ("s", "Spotify", "source_jump:spotify"),
            ("r", "Radio", "source_jump:radio"),
            ("f", "SomaFM", "source_jump:somafm"),
            ("y", "YouTube", "source_jump:youtube"),
        ],
    )]
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = config_dir().join("config.toml");
        let mut cfg = if path.exists() {
            let text = std::fs::read_to_string(&path)
                .with_context(|| format!("reading {}", path.display()))?;
            toml::from_str::<Self>(&text).with_context(|| format!("parsing {}", path.display()))?
        } else {
            Self::default()
        };
        // Fill in any missing keybindings from compile-time defaults so users
        // don't have to redeclare every key when overriding one.
        cfg.keybindings.merge_defaults();
        Ok(cfg)
    }

    pub fn cache_dir(&self) -> PathBuf {
        self.paths
            .cache_dir
            .clone()
            .unwrap_or_else(|| resolve_dir("XDG_CACHE_HOME", ".cache", dirs::cache_dir()))
    }

    pub fn data_dir(&self) -> PathBuf {
        self.paths
            .data_dir
            .clone()
            .unwrap_or_else(|| resolve_dir("XDG_DATA_HOME", ".local/share", dirs::data_dir()))
    }
}

pub fn config_dir() -> PathBuf {
    resolve_dir("XDG_CONFIG_HOME", ".config", dirs::config_dir())
}

/// Resolve a per-user directory under the conventional "fuga/" suffix.
/// Order: $XDG_<KIND>_HOME → $HOME/<dotfile_subpath>/fuga (if it already
/// exists) → platform default from the `dirs` crate. The dotfile check
/// is existence-only so a fresh macOS install still creates state under
/// ~/Library/... by default; users who opt into XDG layout get it picked
/// up automatically.
fn resolve_dir(env_var: &str, dotfile_subpath: &str, fallback: Option<PathBuf>) -> PathBuf {
    if let Some(v) = std::env::var_os(env_var) {
        if !v.is_empty() {
            return PathBuf::from(v).join("fuga");
        }
    }
    if let Some(home) = dirs::home_dir() {
        let dotfile = home.join(dotfile_subpath).join("fuga");
        if dotfile.exists() {
            return dotfile;
        }
    }
    fallback
        .unwrap_or_else(|| PathBuf::from("."))
        .join("fuga")
}
