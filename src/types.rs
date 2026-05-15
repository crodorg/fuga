#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ItemDisplay {
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    /// Small image URL (or library URI) — used for inline thumb rows. Cheap
    /// to fetch + decode, since rows are tiny.
    pub art_uri: Option<String>,
    /// Large image URL (Spotify only) — used for the now-playing pane where
    /// the art occupies a much larger rect. Falls back to `art_uri` when
    /// missing (local/radio/somafm sources don't bother with two sizes).
    pub art_uri_full: Option<String>,
    pub duration: Option<Duration>,
    /// Sort-by-recency hint: Unix seconds for "when this item entered the
    /// user's library / when it was released". Populated by sources that
    /// expose it (Spotify: `added_at`, `release_date`); `None` otherwise.
    /// Drives the `RecentlyAdded` sort axis with newest-first ordering.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort_hint: Option<i64>,
    /// Track number within an album, when known. Drives the `TrackNumber`
    /// sort axis (default for album track listings).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_no: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct Item {
    pub uri: String,
    pub display: ItemDisplay,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EntryKind {
    Directory,
    Track,
    Album,
    Artist,
    Playlist,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub uri: String,
    pub label: String,
    pub kind: EntryKind,
    pub display: Option<ItemDisplay>,
}

#[derive(Debug, Clone)]
pub enum Playable {
    Url(String),
    LibraryUri(String),
}

#[derive(Debug, Clone, Copy)]
pub enum ArtSize {
    Thumb,
    Medium,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlayState {
    Stopped,
    Playing,
    Paused,
}

#[derive(Debug, Clone)]
pub struct PlaybackStatus {
    pub elapsed: Duration,
    pub duration: Option<Duration>,
    pub volume: u8,
    pub state: PlayState,
    /// Codec / container short label, e.g. "FLAC", "MP3", "OGG", "AAC".
    pub codec: Option<String>,
    /// Bitrate in kbps. `0` means unknown / VBR not yet sampled.
    pub bitrate_kbps: Option<u32>,
    /// Live stream title (ICY `StreamTitle` for SomaFM / radio; song title for
    /// local). Surfaced as the now-playing title when present so SomaFM
    /// stations show the current track instead of just the channel name.
    pub stream_title: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub is_active: bool,
    pub volume_percent: Option<u8>,
}

/// User-selectable sort axis for browse views. Per-tab persisted on
/// `CategoryState::sort`. `Year` and `RecentlyAdded` rely on metadata that
/// fuga's source impls don't yet plumb through `ItemDisplay`; selecting
/// them currently falls back to alphabetical with a status toast.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum SortAxis {
    AlphaAsc,
    AlphaDesc,
    Duration,
    Year,
    /// Newest-first by `ItemDisplay::sort_hint`. Falls back to alpha for
    /// rows that don't carry a hint (e.g. local files when filesystem
    /// mtime hasn't been plumbed).
    RecentlyAdded,
    /// Ascending by `ItemDisplay::track_no`. Default for album track
    /// listings; falls back to alpha when missing.
    TrackNumber,
}

impl SortAxis {
    pub fn label(self) -> &'static str {
        match self {
            SortAxis::AlphaAsc => "Alphabetical (A-Z)",
            SortAxis::AlphaDesc => "Alphabetical (Z-A)",
            SortAxis::Duration => "Duration",
            SortAxis::Year => "Year",
            SortAxis::RecentlyAdded => "Recently Added",
            SortAxis::TrackNumber => "Track #",
        }
    }
    pub fn all() -> &'static [SortAxis] {
        &[
            SortAxis::AlphaAsc,
            SortAxis::AlphaDesc,
            SortAxis::Duration,
            SortAxis::Year,
            SortAxis::RecentlyAdded,
            SortAxis::TrackNumber,
        ]
    }
}

/// One of the configurable top-level tab categories. The configured tab list
/// is `Vec<Category>`; rendering, key dispatch, and per-tab state all key off
/// this enum. Variants intentionally cover every category the catalog
/// exposes; tabs that aren't in the active list simply never render.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum Category {
    Queue,
    /// MPD directory tree (Local mode landing tab). Currently delegates to the
    /// album list; hooking up real `lsinfo` walks is a follow-up.
    Directories,
    Albums,
    Artists,
    Playlists,
    /// Merged radio + somafm view (when `radio_split = false`).
    Stations,
    /// Standalone radio tab (when `radio_split = true`).
    Radio,
    /// Standalone SomaFM tab (when `radio_split = true`).
    SomaFm,
    /// Spotify-only landing page: Liked Songs, Recently Played, Top Tracks,
    /// Top Artists, Followed Artists, Saved Albums, Playlists. Drills into
    /// the source-specific browse paths.
    Spotify,
    /// Spotify Podcasts (saved shows). Activating descends into episodes.
    Podcasts,
    /// YouTube landing page (v0.2+): locally saved tracks.
    YouTube,
    Search,
}

/// Active source mode — `t` cycles through registered sources. Mode replaces
/// the tab list (Local/Spotify/SomaFm/Radio each carry their own tab set) and
/// drives the per-mode theme palette.
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq)]
pub enum SourceMode {
    Local,
    Spotify,
    SomaFm,
    Radio,
    YouTube,
}

impl SourceMode {
    pub fn scheme(self) -> &'static str {
        match self {
            SourceMode::Local => "local",
            SourceMode::Spotify => "spotify",
            SourceMode::SomaFm => "somafm",
            SourceMode::Radio => "radio",
            SourceMode::YouTube => "youtube",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SourceMode::Local => "local",
            SourceMode::Spotify => "spotify",
            SourceMode::SomaFm => "somafm",
            SourceMode::Radio => "radio",
            SourceMode::YouTube => "youtube",
        }
    }

    pub fn from_scheme(s: &str) -> Option<Self> {
        Some(match s {
            "local" => SourceMode::Local,
            "spotify" => SourceMode::Spotify,
            "somafm" => SourceMode::SomaFm,
            "radio" => SourceMode::Radio,
            "youtube" => SourceMode::YouTube,
            _ => return None,
        })
    }

    /// Fixed cycle order used by `t`. Filtered to only registered modes by
    /// `App::next_mode`.
    pub fn cycle_order() -> &'static [SourceMode] {
        &[
            SourceMode::Local,
            SourceMode::Spotify,
            SourceMode::YouTube,
            SourceMode::SomaFm,
            SourceMode::Radio,
        ]
    }
}

impl Category {
    pub fn id(self) -> &'static str {
        match self {
            Category::Queue => "queue",
            Category::Directories => "directories",
            Category::Albums => "albums",
            Category::Artists => "artists",
            Category::Playlists => "playlists",
            Category::Stations => "stations",
            Category::Radio => "radio",
            Category::SomaFm => "somafm",
            Category::Spotify => "spotify",
            Category::Podcasts => "podcasts",
            Category::YouTube => "youtube",
            Category::Search => "search",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::Queue => "Queue",
            Category::Directories => "Directories",
            Category::Albums => "Albums",
            Category::Artists => "Artists",
            Category::Playlists => "Playlists",
            Category::Stations => "Stations",
            Category::Radio => "Radio",
            Category::SomaFm => "SomaFM",
            Category::Spotify => "Spotify",
            Category::Podcasts => "Podcasts",
            Category::YouTube => "YouTube",
            Category::Search => "Search",
        }
    }

    /// Per-mode label override. The Spotify landing tab renders as "Library"
    /// in Spotify mode; everything else falls through to `label()`.
    pub fn label_for(self, mode: SourceMode) -> &'static str {
        match (self, mode) {
            (Category::Spotify, SourceMode::Spotify) => "Library",
            _ => self.label(),
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            "queue" => Category::Queue,
            "directories" => Category::Directories,
            "albums" => Category::Albums,
            "artists" => Category::Artists,
            "playlists" => Category::Playlists,
            "stations" => Category::Stations,
            "radio" => Category::Radio,
            "somafm" => Category::SomaFm,
            "spotify" => Category::Spotify,
            "podcasts" => Category::Podcasts,
            "youtube" => Category::YouTube,
            "search" => Category::Search,
            _ => return None,
        })
    }

    /// Categories that are pure browse-style (per-tab breadcrumb stack).
    /// Excludes Queue / Search / NowPlaying which keep their own state shape.
    pub fn is_browse(self) -> bool {
        matches!(
            self,
            Category::Directories
                | Category::Albums
                | Category::Artists
                | Category::Playlists
                | Category::Stations
                | Category::Radio
                | Category::SomaFm
                | Category::Spotify
                | Category::Podcasts
                | Category::YouTube
        )
    }
}
