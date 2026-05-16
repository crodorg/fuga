use std::collections::{HashMap, HashSet};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::{Event, EventStream, KeyEventKind, MouseButton, MouseEvent, MouseEventKind};
use futures::{FutureExt, StreamExt};
use ratatui::layout::Rect;
use mpd_client::client::{ConnectionEvent, ConnectionEvents, Subsystem};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use ratatui_image::protocol::StatefulProtocol;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::time::{self, Interval};
use tokio_util::sync::CancellationToken;

use crate::art_cache::ArtCache;
use crate::config::Config;
use crate::dispatch::Dispatcher;
use crate::config::Hooks;
use crate::keys::{Action, KeyChord, Keymap, LeaderMap};
use crate::queue::{Queue, QueuedItem, RepeatMode};
use crate::source::local::LocalSource;
use crate::term_probe::Term;
use crate::theme::Theme;
use crate::config::TabAlignment;
use crate::types::{
    ArtSize, Category, DeviceEntry, Entry, EntryKind, Item, PlayState, SortAxis, SourceMode,
};
use crate::ui;

/// Modal state for the Add-to-Playlist picker. Opened from the action
/// menu when the hovered row is a Spotify track. Selecting a row calls
/// `playlist_add_items` and closes the modal.
#[derive(Debug, Clone)]
pub struct PlaylistPicker {
    /// Spotify track URI to add (passed back into playlist_add_items).
    pub track_uri: String,
    /// User-writable playlists in display order.
    pub entries: Vec<Entry>,
    pub sel: usize,
}

/// One panel of a browse-category navigation stack.
#[derive(Clone)]
pub enum LibraryView {
    /// One source's browse result (single-source view).
    Entries {
        scheme: &'static str,
        label: String,
        entries: Vec<Entry>,
    },
    /// A flat list of tracks (e.g., album expansion).
    Tracks { label: String, items: Vec<Item> },
    /// Multi-source merged view rendered as grouped sections. Retained
    /// (unused after the mode-toggle redesign) so a future "merged" mode can
    /// re-emit it without re-introducing the type.
    #[allow(dead_code)]
    Sections {
        label: String,
        sections: Vec<Section>,
    },
}

/// One section in a `LibraryView::Sections` view: scheme + display name +
/// the entries from that source.
#[derive(Clone)]
pub struct Section {
    pub scheme: &'static str,
    pub display_name: String,
    pub entries: Vec<Entry>,
}

/// Per-category breadcrumb + cursor + scroll state. Browse-style categories
/// (Albums, Artists, Playlists, Stations, Radio, SomaFm) each get one of
/// these; Queue / Search keep their own state shape elsewhere on `App`.
pub struct CategoryState {
    pub stack: Vec<LibraryView>,
    pub cursor: usize,
    pub top: usize,
    /// True once the initial root view has been fetched; lets the UI show a
    /// "loading…" placeholder before the first fetch lands.
    pub loaded: bool,
    /// Active sort axis for this tab, persisted across visits. `None` =
    /// source-native order.
    pub sort: Option<SortAxis>,
    /// Parent `(cursor, top)` pairs, in stack order — entry `i` is the
    /// position to restore when popping back to `stack[i]`. Pushed on every
    /// descend, popped on every `back`. Cleared on full refresh / mode
    /// switch where the breadcrumb itself is wiped.
    pub parent_cursors: Vec<(usize, usize)>,
    /// URIs descended into, parallel to descents. Top of vec = URI of the
    /// view currently visible (i.e. the one whose `browse(uri)` produced
    /// `stack.last()`). Empty = at root (the root view was produced by
    /// `fetch_category_root`, not by `browse(uri)`).
    ///
    /// Lets context-sensitive actions ("remove from playlist") know what
    /// container they're inside without re-walking the breadcrumb.
    pub descend_uris: Vec<String>,
    /// Parallel to descents — entry `i` records the tab index the user
    /// was on *before* pushing `stack[i+1]`. `Some(idx)` only when the
    /// descent crossed tabs (e.g. Search → detail in Spotify); `None`
    /// for ordinary in-tab descents. On `back()`, popping a `Some`
    /// restores `active_tab_idx` so back from a cross-tab descent
    /// returns to the originating tab.
    pub origin_tabs: Vec<Option<usize>>,
    /// Monotonic counter bumped on each descend push. Tagged into the
    /// `ViewId` of any streaming task spawned for this category so a
    /// back-then-redescend-same-URI sequence can't route the old
    /// stream's late batches into the new view.
    pub descend_epoch: u64,
}

impl CategoryState {
    pub fn new() -> Self {
        Self {
            stack: Vec::new(),
            cursor: 0,
            top: 0,
            loaded: false,
            sort: None,
            parent_cursors: Vec::new(),
            descend_uris: Vec::new(),
            origin_tabs: Vec::new(),
            descend_epoch: 0,
        }
    }
}

impl Default for CategoryState {
    fn default() -> Self {
        Self::new()
    }
}

/// Identifies a specific browse view by (category, depth, epoch). Streaming
/// browse tasks tag each batch with the `ViewId` it was spawned for; the
/// main loop drops batches whose `ViewId` no longer matches the current
/// view. The epoch is bumped on every descent push so a back-then-redescend
/// sequence — same URI or not — gets a fresh ViewId and the previous
/// stream's late batches are filtered out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewId {
    pub category: Category,
    pub depth: usize,
    pub epoch: u64,
}

/// One pagination page (or stream terminator) from a streaming browse task.
/// `finished = true` signals end-of-stream so the main loop can run any
/// post-load work (auto-sort, status toast clear). `batch` is `Ok(rows)`
/// for a normal page or `Err(_)` for a mid-stream error.
#[derive(Debug)]
pub struct RowBatch {
    pub view_id: ViewId,
    pub batch: Result<Vec<Entry>>,
    pub finished: bool,
}

pub struct App {
    /// Configured visible tab list. Index `active_tab_idx` selects the active.
    pub tabs: Vec<Category>,
    /// Per-source-scheme tab override map from `[ui.tabs]`. Consulted on
    /// every `set_mode()` so the bar swaps to the user's mode-specific
    /// list when they hit `t`.
    pub tab_overrides: HashMap<String, Vec<String>>,
    pub active_tab_idx: usize,
    pub tab_alignment: TabAlignment,
    /// Per-browse-category state. Queue / Search keep their own fields.
    pub category_states: HashMap<Category, CategoryState>,

    pub status: Option<String>,
    /// Wall-clock instant the current status toast was set. `on_tick` clears
    /// the toast once it's been visible for ~3s so the top-left cell doesn't
    /// keep stale feedback like "queued: …" forever.
    pub status_set_at: Option<Instant>,
    pub dirty: bool,

    pub queue: Queue,
    pub queue_cursor: usize,
    pub queue_top: usize,

    pub dispatcher: Dispatcher,
    pub local: Arc<LocalSource>,
    pub art_cache: Arc<ArtCache>,
    pub term: Term,

    pub protocols: HashMap<String, StatefulProtocol>,
    pub fetching: HashSet<String>,
    pub wake_tx: UnboundedSender<()>,
    /// Sender side of the row-batch channel. Streaming browse tasks
    /// (spawned from `LibraryActivate::DescendEntry`) clone this and
    /// forward each pagination page as a `RowBatch`. The main loop reads
    /// from the corresponding `row_batch_rx` and appends rows to the
    /// matching view via `handle_row_batch`.
    pub row_batch_tx: UnboundedSender<RowBatch>,
    pub thumb_cells: u16,
    /// Vertical-axis size knob for the now-playing art panel, from
    /// `[ui] art_height_pct`. 100 = full available height. Clamped to
    /// [20, 100] at use.
    pub art_height_pct: u16,
    /// Horizontal-axis size knob for the now-playing art panel, from
    /// `[ui] art_width_pct`. 100 = full available width. Clamped to
    /// [15, 100] at use.
    pub art_width_pct: u16,
    /// Modes the `T` key walks through. Built from `[ui] thumb_cycle` plus
    /// the startup mode if it wasn't in the list (so we always start on a
    /// member of the cycle).
    pub thumb_cycle: Vec<crate::term_probe::ThumbMode>,

    pub now_playing_protocol: Option<StatefulProtocol>,
    pub now_playing_uri: Option<String>,
    /// Natural pixel dimensions of the now-playing image, captured at
    /// protocol-build time. Used by `compute_art_dims` to shape the panel
    /// to the source aspect (no whitespace letterbox for non-square art).
    pub now_playing_aspect: Option<(u32, u32)>,

    pub playback: Option<crate::types::PlaybackStatus>,
    pub master_volume: u8,
    /// Last volume-step instant. OS autorepeat blasts many Press events when
    /// `+`/`-` is held; debounce so each held repeat below 120ms is dropped.
    /// Initial press always fires (None = fire).
    pub last_volume_at: Option<Instant>,
    pub shuffle: bool,
    pub repeat: RepeatMode,
    tick_counter: u32,

    pub keymap: Keymap,
    pub leader: Option<LeaderMap>,
    pub leader_deadline: Option<Instant>,

    pub search_query: String,
    pub search_input_focused: bool,
    pub search_results: Vec<SearchGroup>,
    pub search_cursor: usize,
    pub search_top: usize,

    pub command_buffer: String,
    pub command_input_focused: bool,

    pub theme: Theme,
    /// User-selected palette before per-source accent swap. `set_mode` rebuilds
    /// `theme = base_theme.with_source_accent(mode)` each toggle so the source
    /// palette doesn't accumulate across mode changes.
    pub base_theme: Theme,
    /// Currently active source mode — drives tab list, theme accent, and
    /// per-tab content filtering. Toggled by `t`.
    pub active_source: SourceMode,
    /// Modes the user actually has configured. `t` cycles only through this
    /// list (skip-disabled per design).
    pub available_modes: Vec<SourceMode>,

    pub help_visible: bool,
    pub help_scroll: u16,
    /// Spotify Connect device-picker modal state. Open via `d` (default).
    pub device_modal_open: bool,
    pub device_modal_loading: bool,
    pub devices: Vec<DeviceEntry>,
    pub device_modal_sel: usize,
    /// Sort modal state. Open via `o` (default).
    pub sort_modal_open: bool,
    pub sort_modal_sel: usize,
    /// Cached liked state of the current track. Refreshed after track-change
    /// or after the user toggles. None = unknown / source doesn't support saves.
    pub current_liked: Option<bool>,

    pub hooks: Hooks,
    pub last_active_scheme: Option<&'static str>,

    /// Active in-view filter input. `Some(buf)` while the user is typing a
    /// `/` filter; `None` otherwise. Filter UI scaffolding is added in a
    /// follow-up commit — for now this just exists so `Action::FilterInPage`
    /// can flip it without scattering todos around.
    pub filter_input: Option<String>,
    /// Committed filter per tab. Empty/missing entry = no filter for that
    /// tab. Cleared by `Esc` on an already-empty input.
    pub filter_active: HashMap<crate::types::Category, String>,
    /// Cached original-row indices for the current browse view, populated
    /// each frame by `render_browse`. Lets activate/enqueue map a filtered
    /// cursor back to its original list position without re-running the
    /// row build.
    pub filtered_browse_indices: Option<Vec<usize>>,

    /// Last-rendered click targets, so mouse events know what they hit.
    /// Populated by `ui::render`; consumed by `handle_mouse`. Stores the
    /// configured-tab-list index that each rect represents.
    pub tab_rects: Vec<(Rect, usize)>,
    pub body_rect: Option<Rect>,
    /// Heights (cells) of the currently-visible rows, in display order
    /// starting from `body_top_at_render`. Per-row variable-height layout
    /// (smart thumbs: rows with art = `thumb_cells`, rows without = 1)
    /// means click-to-row math has to walk this rather than divide by a
    /// single `row_h`.
    pub body_row_heights: Vec<u16>,
    pub body_top_at_render: usize,
    /// Last-rendered progress bar rect (the inner clickable bar, excluding
    /// time labels). Click here → seek to that fraction of the track.
    pub progress_bar_rect: Option<Rect>,
    /// Last-rendered now-playing art panel rect. Mouse clicks inside this
    /// area are swallowed so they don't pass through to the body rows
    /// underneath.
    pub art_panel_rect: Option<Rect>,
    /// User clicked the art to hide its body protrusion. The art panel still
    /// renders, just shrunk to bottom-bar height (no overlay into the list).
    /// Click again to expand. Reset on track change.
    pub art_collapsed: bool,
    /// Path to the cross-run state file (`state.json`). When set, mouse
    /// clicks that flip `art_collapsed` also persist the new value here.
    pub state_path: Option<std::path::PathBuf>,
    /// When `Some(uri)`, the renderer overlays that cover full-screen
    /// (centered, 60% of terminal area, single-line border). Click anywhere
    /// or press any non-allowed key to close. Set by mouse clicks on
    /// inline thumbnails; cleared on overlay-close.
    pub expanded_art_uri: Option<String>,
    /// Last-rendered hit-rects for inline thumbnails (image cell only,
    /// not the row text). Populated by `widgets::thumb_list`; consumed
    /// by `handle_mouse` to detect clicks on the thumb image.
    pub thumb_hits: Vec<(Rect, String)>,
    /// URIs the user has pinned to the top of browse views. Persisted in
    /// `state.json` so the set survives restarts.
    pub pinned: std::collections::HashSet<String>,
    /// Open the song-action modal on the hovered row. Rendered as a
    /// centered popup with a vim-style key list. `None` = closed.
    pub action_menu_open: bool,
    pub action_menu_sel: usize,
    /// Add-to-Playlist picker modal state. `None` = closed.
    pub playlist_picker: Option<PlaylistPicker>,
    /// Last-rendered volume readout rect. Mouse-wheel inside it nudges
    /// volume up/down without paging the body list.
    pub volume_rect: Option<Rect>,
    /// Last-rendered now-playing text rect (title + artist + album rows
    /// in the bottom bar, excluding the progress bar and the right-side
    /// volume / state cells). Click handler maps mouse buttons to
    /// transport: left = previous, middle = play/pause, right = next.
    pub now_playing_text_rect: Option<Rect>,
    /// Shared 0..=100 download-progress slot. `255` = no active download.
    /// Updated by the YouTube source's spawned download task; read by
    /// the status-toast renderer.
    pub download_progress: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// One-shot toast slot written by background tasks. The wake handler
    /// in the event loop drains this into `status` so user-visible
    /// messages still go through `set_status` (which timestamps for
    /// auto-clear). `None` means no pending toast.
    pub toast_inbox: std::sync::Arc<std::sync::Mutex<Option<String>>>,

    pub shutdown: CancellationToken,

    /// Outbound channel to the MPRIS D-Bus bridge. None when MPRIS init failed
    /// or the platform doesn't support it; calls become no-ops.
    pub mpris_cmd_tx: Option<UnboundedSender<crate::mpris::MprisCommand>>,
    /// Snapshot of last MPRIS-pushed state — `sync_mpris` diffs against these
    /// to avoid spamming D-Bus subscribers when nothing changed.
    mpris_last_uri: Option<String>,
    mpris_last_state: Option<PlayState>,
    mpris_last_volume: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct SearchGroup {
    pub scheme: &'static str,
    pub items: Vec<Item>,
}

/// True iff the queue item's title / artist / album contains the given
/// lowercase pattern. Used by the in-view filter (`/`) to hide non-matching
/// rows. Substring match — no fuzzy scoring in v1.
fn queue_item_matches(item: &crate::queue::QueuedItem, q_lower: &str) -> bool {
    let t = item.display.title.to_lowercase();
    if t.contains(q_lower) {
        return true;
    }
    if let Some(a) = &item.display.artist {
        if a.to_lowercase().contains(q_lower) {
            return true;
        }
    }
    if let Some(a) = &item.display.album {
        if a.to_lowercase().contains(q_lower) {
            return true;
        }
    }
    false
}

impl App {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        local: Arc<LocalSource>,
        dispatcher: Dispatcher,
        art_cache: Arc<ArtCache>,
        term: Term,
        thumb_cells: u16,
        art_height_pct: u16,
        art_width_pct: u16,
        keymap: Keymap,
        theme: Theme,
        base_theme: Theme,
        hooks: Hooks,
        tabs: Vec<Category>,
        tab_overrides: HashMap<String, Vec<String>>,
        tab_alignment: TabAlignment,
        active_source: SourceMode,
        available_modes: Vec<SourceMode>,
        thumb_cycle: Vec<crate::term_probe::ThumbMode>,
    ) -> (Self, UnboundedReceiver<()>, UnboundedReceiver<RowBatch>) {
        let mut category_states: HashMap<Category, CategoryState> = HashMap::new();
        for c in &tabs {
            if c.is_browse() {
                category_states.insert(*c, CategoryState::new());
            }
        }
        let (wake_tx, wake_rx) = mpsc::unbounded_channel();
        let (row_batch_tx, row_batch_rx) = mpsc::unbounded_channel::<RowBatch>();
        let app = Self {
            tabs,
            tab_overrides,
            active_tab_idx: 0,
            tab_alignment,
            category_states,
            status: None,
            status_set_at: None,
            dirty: true,
            queue: Queue::new(),
            queue_cursor: 0,
            queue_top: 0,
            dispatcher,
            local,
            art_cache,
            term,
            protocols: HashMap::new(),
            fetching: HashSet::new(),
            wake_tx,
            row_batch_tx,
            thumb_cells,
            art_height_pct,
            art_width_pct,
            thumb_cycle,
            now_playing_protocol: None,
            now_playing_uri: None,
            now_playing_aspect: None,
            playback: None,
            master_volume: 80,
            last_volume_at: None,
            shuffle: false,
            repeat: RepeatMode::Off,
            tick_counter: 0,
            keymap,
            leader: None,
            leader_deadline: None,
            search_query: String::new(),
            search_input_focused: false,
            search_results: Vec::new(),
            search_cursor: 0,
            search_top: 0,
            command_buffer: String::new(),
            command_input_focused: false,
            theme,
            base_theme,
            active_source,
            available_modes,
            help_visible: false,
            help_scroll: 0,
            device_modal_open: false,
            device_modal_loading: false,
            devices: Vec::new(),
            device_modal_sel: 0,
            sort_modal_open: false,
            sort_modal_sel: 0,
            current_liked: None,
            hooks,
            last_active_scheme: None,
            filter_input: None,
            filter_active: HashMap::new(),
            filtered_browse_indices: None,
            tab_rects: Vec::new(),
            body_rect: None,
            body_row_heights: Vec::new(),
            body_top_at_render: 0,
            progress_bar_rect: None,
            art_panel_rect: None,
            art_collapsed: false,
            state_path: None,
            expanded_art_uri: None,
            thumb_hits: Vec::new(),
            pinned: std::collections::HashSet::new(),
            action_menu_open: false,
            action_menu_sel: 0,
            playlist_picker: None,
            volume_rect: None,
            now_playing_text_rect: None,
            download_progress: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(255)),
            toast_inbox: std::sync::Arc::new(std::sync::Mutex::new(None)),
            shutdown: CancellationToken::new(),
            mpris_cmd_tx: None,
            mpris_last_uri: None,
            mpris_last_state: None,
            mpris_last_volume: None,
        };
        (app, wake_rx, row_batch_rx)
    }

    /// Push diff'd playback state to the MPRIS bridge. Cheap when nothing
    /// changed: each branch early-returns on equality with the cached
    /// snapshot. Called from the main event loop after every action.
    pub fn sync_mpris(&mut self) {
        let Some(tx) = &self.mpris_cmd_tx else {
            return;
        };
        // Track / metadata change.
        let cur_uri = self.queue.current().map(|c| c.uri.clone());
        if cur_uri != self.mpris_last_uri {
            if let Some(item) = self.queue.current() {
                let d = &item.display;
                let _ = tx.send(crate::mpris::MprisCommand::Metadata {
                    title: d.title.clone(),
                    artists: d
                        .artist
                        .clone()
                        .map(|a| vec![a])
                        .unwrap_or_default(),
                    album: d.album.clone(),
                    duration_ms: d
                        .duration
                        .map(|x| x.as_millis().min(u32::MAX as u128) as u32)
                        .unwrap_or(0),
                    art_url: d.art_uri.clone(),
                });
            }
            self.mpris_last_uri = cur_uri;
        }
        // Playback status.
        let state = self.playback.as_ref().map(|p| p.state);
        if state != self.mpris_last_state {
            let s = match state {
                Some(PlayState::Playing) => crate::mpris::MprisStatus::Playing,
                Some(PlayState::Paused) => crate::mpris::MprisStatus::Paused,
                Some(PlayState::Stopped) | None => crate::mpris::MprisStatus::Stopped,
            };
            let _ = tx.send(crate::mpris::MprisCommand::PlaybackStatus(s));
            self.mpris_last_state = state;
        }
        // Volume — track master_volume, not per-source playback.volume, since
        // master is what fuga's slider actually controls.
        if Some(self.master_volume) != self.mpris_last_volume {
            let _ = tx.send(crate::mpris::MprisCommand::Volume(self.master_volume));
            self.mpris_last_volume = Some(self.master_volume);
        }
    }

    pub fn active_category(&self) -> Category {
        self.tabs[self.active_tab_idx]
    }

    /// Pick the next available source mode in canonical cycle order, skipping
    /// disabled (unregistered) sources. Wraps. Returns the current mode when
    /// only one source is registered.
    pub fn next_mode(&self) -> SourceMode {
        if self.available_modes.is_empty() {
            return self.active_source;
        }
        let cur_idx = self
            .available_modes
            .iter()
            .position(|m| *m == self.active_source)
            .unwrap_or(0);
        let next_idx = (cur_idx + 1) % self.available_modes.len();
        self.available_modes[next_idx]
    }

    /// Append one streaming row batch to the matching view. Drops the batch
    /// when the user has navigated away (stack depth changed) so partial
    /// pages from a stale stream don't pollute a different view. Runs the
    /// usual auto-sort detection once `batch.finished == true` so disc /
    /// recently-added ordering still kicks in after the full set arrives.
    pub fn handle_row_batch(&mut self, batch: RowBatch) {
        let RowBatch { view_id, batch, finished } = batch;
        let Some(state) = self.category_states.get_mut(&view_id.category) else { return };
        // Epoch guard: dropping batches from a previous descend in the same
        // category — covers both "user backed out and went elsewhere" and
        // "user backed out and re-descended (same URI or not)". The depth
        // check stays as a cheap pre-filter for the common case.
        if state.stack.len() != view_id.depth { return }
        if state.descend_epoch != view_id.epoch { return }
        let originating_uri = state.descend_uris.last().cloned().unwrap_or_default();
        let Some(LibraryView::Entries { entries, .. }) = state.stack.last_mut() else { return };
        match batch {
            Ok(rows) if !rows.is_empty() => {
                entries.extend(rows);
                self.dirty = true;
            }
            Ok(_) => {} // empty batch — finished sentinel or no-op page
            Err(e) => {
                self.set_status(format!("load failed: {e}"));
                return;
            }
        }
        if finished {
            // Auto-sort once the full stream is in. Mirrors the non-streaming
            // path's heuristic: track_no → TrackNumber; sort_hint OR spotify
            // playlist URI → RecentlyAdded. The spotify-playlist override
            // exists because some playlists don't populate `added_at`, but
            // the desktop convention is still newest-first.
            let has_track_no = entries.iter()
                .any(|e| e.display.as_ref().and_then(|d| d.track_no).is_some());
            let has_hint = entries.iter()
                .any(|e| e.display.as_ref().and_then(|d| d.sort_hint).is_some());
            let is_spotify_playlist = originating_uri.starts_with("spotify:playlist:");
            let auto_axis = if has_track_no {
                Some(SortAxis::TrackNumber)
            } else if is_spotify_playlist || has_hint {
                Some(SortAxis::RecentlyAdded)
            } else {
                None
            };
            if let Some(axis) = auto_axis {
                if let Some(view) = state.stack.last_mut() {
                    sort_library_view(view, axis);
                    state.sort = Some(axis);
                }
            }
            self.dirty = true;
        }
    }

    /// Theme tinted by the *playing* track's source rather than the active
    /// browse mode. Falls back to the active-mode theme when nothing is
    /// playing or the playing scheme doesn't map to a known `SourceMode`.
    /// Used for now-playing visuals (art panel border, bottom-bar title) so
    /// they stay tied to what's actually playing while the user browses
    /// elsewhere.
    pub fn playing_theme(&self) -> std::borrow::Cow<'_, Theme> {
        let scheme = match self.queue.current() {
            Some(q) => q.source_scheme,
            None => return std::borrow::Cow::Borrowed(&self.theme),
        };
        match SourceMode::from_scheme(scheme) {
            Some(mode) if mode != self.active_source => std::borrow::Cow::Owned(
                self.base_theme.clone().with_source_accent(mode),
            ),
            _ => std::borrow::Cow::Borrowed(&self.theme),
        }
    }

    /// Switch source mode: rebuild tab list, swap theme palette, reset active
    /// tab + cursor, force re-fetch of the new root view. Idempotent when
    /// `mode == self.active_source`.
    pub async fn set_mode(&mut self, mode: SourceMode) {
        if mode == self.active_source {
            return;
        }
        self.active_source = mode;
        self.theme = self.base_theme.clone().with_source_accent(mode);
        self.tabs = crate::tabs_for_mode(mode, &self.tab_overrides);
        // Make sure every browse tab in the new list has a CategoryState slot.
        for c in &self.tabs {
            if c.is_browse() {
                self.category_states.entry(*c).or_default();
            }
        }
        self.active_tab_idx = 0;
        // Clear every browse-category state on mode switch. A tab like
        // `Albums` means "Local Albums" in Local mode but "Saved Albums"
        // (Spotify) in Spotify mode — leaving the old loaded view in place
        // would show the wrong source's data after a `t` toggle.
        for s in self.category_states.values_mut() {
            s.stack.clear();
            s.parent_cursors.clear();
            s.descend_uris.clear();
            s.origin_tabs.clear();
            s.cursor = 0;
            s.top = 0;
            s.loaded = false;
            // sort: keep prior preference so re-fetch lands on the same axis.
        }
        let from = self.last_active_scheme;
        self.set_status(format!("mode: {}", mode.label()));
        crate::hooks::on_source_switch(&self.hooks, from, mode.scheme());
        self.ensure_active_loaded().await;
        self.dirty = true;
    }

    /// Topmost view in the active category's stack, or None if not yet
    /// loaded (or if the active category is non-browse like Queue/Search).
    pub fn current_view(&self) -> Option<&LibraryView> {
        let cat = self.active_category();
        self.category_states.get(&cat).and_then(|s| s.stack.last())
    }

    /// URI of the container currently visible in the active browse tab
    /// (e.g. `spotify:playlist:...` when inside a playlist). `None` at root.
    pub fn current_descend_uri(&self) -> Option<&str> {
        let cat = self.active_category();
        self.category_states
            .get(&cat)
            .and_then(|s| s.descend_uris.last())
            .map(|s| s.as_str())
    }

    pub fn current_view_len(&self) -> usize {
        match self.current_view() {
            Some(LibraryView::Entries { entries, .. }) => entries.len(),
            Some(LibraryView::Tracks { items, .. }) => items.len(),
            Some(LibraryView::Sections { sections, .. }) => {
                // header rows + entry rows
                sections.iter().map(|s| s.entries.len() + 1).sum()
            }
            None => 0,
        }
    }

    pub fn current_view_title(&self) -> String {
        match self.current_view() {
            Some(LibraryView::Entries { label, .. }) => label.clone(),
            Some(LibraryView::Tracks { label, .. }) => label.clone(),
            Some(LibraryView::Sections { label, .. }) => label.clone(),
            None => self.active_category().label().to_string(),
        }
    }

    fn set_status<S: Into<String>>(&mut self, s: S) {
        let mut s: String = s.into();
        // Toasts live in a tiny corner overlay. Anything over ~60 chars
        // overflows useful screen real estate without telling the user
        // anything they couldn't see in the log. Truncate hard.
        const MAX: usize = 60;
        if s.chars().count() > MAX {
            s = s.chars().take(MAX.saturating_sub(1)).collect::<String>() + "…";
        }
        self.status = Some(s);
        self.status_set_at = Some(Instant::now());
        self.dirty = true;
    }

    /// (cursor, length) for the active tab.
    fn cursor_for_tab(&self) -> (usize, usize) {
        let cat = self.active_category();
        let len = match cat {
            Category::Queue => self
                .filtered_queue_len()
                .unwrap_or_else(|| self.queue.len()),
            Category::Search => self.search_results_flat_len(),
            _ => self
                .filtered_browse_len()
                .unwrap_or_else(|| self.current_view_len()),
        };
        let cur = match cat {
            Category::Queue => self.queue_cursor,
            Category::Search => self.search_cursor,
            _ => self.category_states.get(&cat).map(|s| s.cursor).unwrap_or(0),
        };
        (cur, len)
    }

    /// Active in-view filter pattern for the current tab, if any.
    pub fn current_filter(&self) -> Option<&str> {
        let cat = self.active_category();
        self.filter_active
            .get(&cat)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }

    /// Indices of queue items that match the active filter, in queue order.
    /// `None` when no filter is active for the current tab — callers should
    /// fall back to the unfiltered queue.
    pub fn filtered_queue_indices(&self) -> Option<Vec<usize>> {
        let q = self.current_filter()?;
        let q = q.to_lowercase();
        let items = self.queue.items();
        Some(
            items
                .iter()
                .enumerate()
                .filter(|(_, it)| queue_item_matches(it, &q))
                .map(|(i, _)| i)
                .collect(),
        )
    }

    fn filtered_queue_len(&self) -> Option<usize> {
        if !matches!(self.active_category(), Category::Queue) {
            return None;
        }
        self.filtered_queue_indices().map(|v| v.len())
    }

    /// Filtered length for the current browse view's rows. Reads the cached
    /// indices populated by the most recent `render_browse` (via
    /// `set_filtered_browse_indices`); `None` when no filter is active for
    /// the current tab.
    fn filtered_browse_len(&self) -> Option<usize> {
        self.current_filter()?;
        self.filtered_browse_indices
            .as_ref()
            .map(|v| v.len())
    }

    /// Resolve a filtered cursor (browse tab) to the original row index. The
    /// renderer caches the filter mapping each frame; activate / enqueue
    /// paths consume it here.
    pub fn filtered_browse_cursor_to_orig(&self, cursor: usize) -> Option<usize> {
        let _ = self.current_filter()?;
        self.filtered_browse_indices
            .as_ref()
            .and_then(|v| v.get(cursor).copied())
    }

    /// Cache the original-row indices for the current browse view so action
    /// handlers can map a filtered cursor back to its original position.
    pub fn set_filtered_browse_indices(&mut self, indices: Option<Vec<usize>>) {
        self.filtered_browse_indices = indices;
    }

    /// Resolve a filtered cursor (queue tab) to the original queue index.
    /// Returns `None` if no filter is active or the cursor is past the end.
    pub fn filtered_queue_cursor_to_orig(&self, cursor: usize) -> Option<usize> {
        let indices = self.filtered_queue_indices()?;
        indices.get(cursor).copied()
    }

    /// Clamp tab cursors to the filtered list length so movement keys can't
    /// step past the visible tail when a filter shrinks the row count.
    pub fn clamp_cursor_to_filter(&mut self) {
        let cat = self.active_category();
        let new_len = match cat {
            Category::Queue => self
                .filtered_queue_len()
                .unwrap_or_else(|| self.queue.len()),
            _ => return,
        };
        if new_len == 0 {
            self.set_cursor(0);
        } else if matches!(cat, Category::Queue) && self.queue_cursor >= new_len {
            self.queue_cursor = new_len - 1;
            self.dirty = true;
        }
    }

    fn set_cursor(&mut self, idx: usize) {
        let cat = self.active_category();
        match cat {
            Category::Queue => self.queue_cursor = idx,
            Category::Search => self.search_cursor = idx,
            _ => {
                if let Some(s) = self.category_states.get_mut(&cat) {
                    s.cursor = idx;
                }
            }
        }
        self.dirty = true;
    }

    /// Total rows in the flattened search-results list. Headers are no
    /// longer rendered (tab bar + theme accent convey source), so the
    /// flat length is just the sum of item counts.
    pub fn search_results_flat_len(&self) -> usize {
        self.search_results.iter().map(|g| g.items.len()).sum()
    }

    async fn handle_action(&mut self, action: Action) -> Result<()> {
        match action {
            Action::Quit => self.shutdown.cancel(),
            Action::NextTab => {
                if !self.tabs.is_empty() {
                    self.active_tab_idx = (self.active_tab_idx + 1) % self.tabs.len();
                    self.ensure_active_loaded().await;
                    self.dirty = true;
                }
            }
            Action::PrevTab => {
                if !self.tabs.is_empty() {
                    self.active_tab_idx = if self.active_tab_idx == 0 {
                        self.tabs.len() - 1
                    } else {
                        self.active_tab_idx - 1
                    };
                    self.ensure_active_loaded().await;
                    self.dirty = true;
                }
            }
            Action::TabByIndex(n) => {
                let n = n as usize;
                if n < self.tabs.len() && n != self.active_tab_idx {
                    self.active_tab_idx = n;
                    self.ensure_active_loaded().await;
                    self.dirty = true;
                }
            }
            Action::JumpRoots => {
                // Pop active category back to its root view. If non-browse tab,
                // no-op.
                let cat = self.active_category();
                if let Some(s) = self.category_states.get_mut(&cat) {
                    s.stack.truncate(1);
                    s.parent_cursors.clear();
                    s.descend_uris.clear();
                    s.origin_tabs.clear();
                    s.cursor = 0;
                    s.top = 0;
                    self.dirty = true;
                }
            }
            Action::SourceJump(scheme) => {
                // Source-jump only meaningful pre-merge. Best-effort: switch to
                // the first configured browse tab whose backing source matches
                // and reset its stack. If none match, status toast.
                let target = self.tabs.iter().enumerate().find(|(_, c)| match (scheme.as_str(), c) {
                    ("local", Category::Albums) => self.dispatcher.get("local").is_some(),
                    ("spotify", Category::Spotify) => self.dispatcher.get("spotify").is_some(),
                    ("spotify", Category::Albums)
                    | ("spotify", Category::Artists)
                    | ("spotify", Category::Playlists) => self.dispatcher.get("spotify").is_some(),
                    ("radio", Category::Stations) | ("somafm", Category::Stations) => true,
                    ("radio", Category::Radio) => true,
                    ("somafm", Category::SomaFm) => true,
                    _ => false,
                });
                if let Some((idx, _)) = target {
                    self.active_tab_idx = idx;
                    self.ensure_active_loaded().await;
                    self.dirty = true;
                } else {
                    self.set_status(format!("source not registered: {scheme}"));
                }
            }
            Action::VolumeUp => {
                if self.volume_debounce_fired() {
                    self.master_volume = self.master_volume.saturating_add(10).min(100);
                    self.push_volume().await;
                    self.set_status(format!("vol: {}%", self.master_volume));
                }
            }
            Action::VolumeDown => {
                if self.volume_debounce_fired() {
                    self.master_volume = self.master_volume.saturating_sub(10);
                    self.push_volume().await;
                    self.set_status(format!("vol: {}%", self.master_volume));
                }
            }
            Action::SetVolume(v) => {
                self.master_volume = v.min(100);
                self.push_volume().await;
                self.set_status(format!("vol: {}%", self.master_volume));
            }
            Action::FocusSearch => {
                if let Some(idx) = self.tabs.iter().position(|c| *c == Category::Search) {
                    self.active_tab_idx = idx;
                }
                self.search_input_focused = true;
                self.dirty = true;
            }
            Action::FocusCommand => {
                self.command_input_focused = true;
                self.command_buffer.clear();
                self.dirty = true;
            }
            Action::Down => {
                let (cur, len) = self.cursor_for_tab();
                if len > 0 {
                    self.set_cursor((cur + 1).min(len - 1));
                }
            }
            Action::Up => {
                let (cur, _) = self.cursor_for_tab();
                self.set_cursor(cur.saturating_sub(1));
            }
            Action::PageDown => {
                let (cur, len) = self.cursor_for_tab();
                if len > 0 {
                    self.set_cursor((cur + 10).min(len - 1));
                }
            }
            Action::PageUp => {
                let (cur, _) = self.cursor_for_tab();
                self.set_cursor(cur.saturating_sub(10));
            }
            Action::Top => self.set_cursor(0),
            Action::Bottom => {
                let (_, len) = self.cursor_for_tab();
                self.set_cursor(len.saturating_sub(1));
            }
            Action::Activate => self.activate().await?,
            Action::Enqueue => self.enqueue_current().await?,
            Action::Back => self.back(),
            Action::PlayPause => self.toggle_pause().await?,
            Action::NextTrack => {
                let vol = self.master_volume;
                let shuf = self.shuffle;
                let rep = self.repeat;
                self.dispatcher
                    .advance_with(&mut self.queue, shuf, rep, vol)
                    .await?;
                self.refresh_now_playing().await;
                self.dirty = true;
            }
            Action::PrevTrack => {
                // ncspot-like: first press within the first 5s of the track
                // jumps to the previous queue entry; later presses restart
                // the current track. Avoids accidental skip-back when the
                // user just wanted to start the song over.
                let elapsed = self
                    .playback
                    .as_ref()
                    .map(|p| p.elapsed)
                    .unwrap_or(Duration::ZERO);
                if elapsed >= Duration::from_secs(5) {
                    if let Some(scheme) = self.dispatcher.active_scheme() {
                        if let Some(src) = self.dispatcher.get(scheme).cloned() {
                            if let Err(e) = src.seek(Duration::ZERO).await {
                                tracing::warn!("seek-to-start: {e}");
                            }
                        }
                    }
                } else {
                    let vol = self.master_volume;
                    self.dispatcher.previous(&mut self.queue, vol).await?;
                    self.refresh_now_playing().await;
                }
                self.dirty = true;
            }
            Action::Stop => {
                self.dispatcher.stop().await?;
                self.dirty = true;
            }
            Action::Refresh => self.refresh_current_view().await,
            Action::ToggleThumb => self.cycle_thumb_mode().await,
            Action::CycleSource => {
                let next = self.next_mode();
                self.set_mode(next).await;
            }
            Action::ToggleHelp => {
                self.help_visible = !self.help_visible;
                self.dirty = true;
            }
            Action::ToggleLike => self.toggle_like().await,
            Action::OpenDevicePicker => self.open_device_picker().await,
            Action::TransferToSelectedDevice => self.transfer_to_selected_device().await,
            Action::SeekToPermille(p) => self.seek_to_permille(p).await,
            Action::SeekRelative(secs) => self.seek_relative(secs).await,
            Action::ToggleShuffle => {
                self.shuffle = !self.shuffle;
                self.set_status(if self.shuffle { "shuffle on" } else { "shuffle off" });
            }
            Action::CycleRepeat => {
                self.repeat = self.repeat.cycle();
                let label = match self.repeat {
                    RepeatMode::Off => "repeat off",
                    RepeatMode::All => "repeat all",
                    RepeatMode::Track => "repeat track",
                };
                self.set_status(label);
            }
            Action::OpenSortModal => {
                if !self.active_category().is_browse() {
                    self.set_status("sort: not a browse tab");
                } else {
                    self.sort_modal_open = true;
                    let cur = self.category_states.get(&self.active_category()).and_then(|s| s.sort);
                    self.sort_modal_sel = cur
                        .and_then(|a| SortAxis::all().iter().position(|x| *x == a))
                        .unwrap_or(0);
                    self.dirty = true;
                }
            }
            Action::ApplySelectedSort => self.apply_selected_sort(),
            Action::FollowPlaying => self.follow_playing(),
            Action::ClearQueue => self.clear_queue().await,
            Action::RemoveFromQueue => self.remove_from_queue().await,
            Action::ExpandHoveredArt => self.expand_hovered_art().await,
            Action::OpenActionMenu => self.open_action_menu(),
            Action::TogglePinHovered => self.toggle_pin_hovered(),
            Action::FilterInPage => self.begin_filter_input(),
            Action::DownloadHovered => self.download_hovered().await,
            Action::None => {}
        }
        Ok(())
    }

    /// Empty the queue and stop the active source. Triggered by
    /// `Action::ClearQueue` (default key `C`) and the `:clear` command.
    /// Remove the row under the cursor in the Queue tab. If the removed
    /// row is currently playing, stop playback and advance cursor stays at
    /// same index (which is now the next item or end-of-list).
    async fn remove_from_queue(&mut self) {
        if !matches!(self.active_category(), Category::Queue) {
            return;
        }
        let idx = self.queue_cursor;
        let was_current = self.queue.current_index() == Some(idx);
        if !self.queue.remove(idx) {
            return;
        }
        let len = self.queue.len();
        if len == 0 {
            self.queue_cursor = 0;
        } else if self.queue_cursor >= len {
            self.queue_cursor = len - 1;
        }
        if was_current {
            if let Err(e) = self.dispatcher.stop().await {
                tracing::warn!("remove_from_queue stop: {e}");
            }
            self.refresh_now_playing().await;
        }
        self.set_status("removed from queue");
        self.dirty = true;
    }

    async fn clear_queue(&mut self) {
        self.queue.clear();
        self.queue_cursor = 0;
        if let Err(e) = self.dispatcher.stop().await {
            tracing::warn!("clear_queue stop: {e}");
        }
        self.last_active_scheme = None;
        self.set_status("queue cleared");
        self.dirty = true;
    }

    fn begin_filter_input(&mut self) {
        // Guard: Search tab without results has nothing to filter — and the
        // current `filter_input` machinery is keyed off `active_category`,
        // which doesn't apply to the flat search-results list anyway. Tell
        // the user instead of silently entering an unresponsive mode.
        if matches!(self.active_category(), Category::Search) {
            if self.search_results.is_empty() {
                self.set_status("filter: run a search first (press s)");
                return;
            }
            self.set_status("filter: not supported on Search tab — refine your query");
            return;
        }
        self.filter_input = Some(String::new());
        self.set_status("filter: typing… Enter=commit, Esc=cancel");
        self.dirty = true;
    }

    /// Open the Spotify device-picker modal and synchronously fetch the list.
    /// The fetch typically takes <300ms; the brief block keeps the code simple
    /// versus a deferred-results channel.
    async fn open_device_picker(&mut self) {
        let Some(spotify) = self.dispatcher.get("spotify").cloned() else {
            self.set_status("device picker: Spotify source not enabled");
            return;
        };
        self.device_modal_open = true;
        self.device_modal_loading = true;
        self.devices.clear();
        self.device_modal_sel = 0;
        self.dirty = true;
        match spotify.list_devices().await {
            Ok(devs) => {
                if let Some(idx) = devs.iter().position(|d| d.is_active) {
                    self.device_modal_sel = idx;
                }
                self.devices = devs;
            }
            Err(e) => {
                self.set_status(format!("device list: {e}"));
                self.device_modal_open = false;
            }
        }
        self.device_modal_loading = false;
        self.dirty = true;
    }

    /// Relative seek by `secs` (negative = back). Clamped to [0, duration].
    /// No-op if no track or duration unknown (streams).
    async fn seek_relative(&mut self, secs: i32) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let Some(dur) = playback.duration else {
            self.set_status("seek: stream has no duration");
            return;
        };
        let cur = playback.elapsed.as_secs() as i64;
        let dur_s = dur.as_secs() as i64;
        let mut tgt = cur + secs as i64;
        if tgt < 0 {
            tgt = 0;
        }
        if tgt > dur_s {
            tgt = dur_s;
        }
        let target = std::time::Duration::from_secs(tgt as u64);
        let Some(scheme) = self.dispatcher.active_scheme() else {
            return;
        };
        let Some(src) = self.dispatcher.get(scheme).cloned() else {
            return;
        };
        if let Err(e) = src.seek(target).await {
            self.set_status(format!("seek: {e}"));
        } else {
            self.dirty = true;
        }
    }

    async fn seek_to_permille(&mut self, permille: u16) {
        let Some(playback) = self.playback.as_ref() else {
            return;
        };
        let Some(dur) = playback.duration else {
            self.set_status("seek: no track duration available");
            return;
        };
        let target_ms =
            (dur.as_millis() as u64).saturating_mul(permille.min(1000) as u64) / 1000;
        let target = std::time::Duration::from_millis(target_ms);
        let Some(scheme) = self.dispatcher.active_scheme() else {
            return;
        };
        let Some(src) = self.dispatcher.get(scheme).cloned() else {
            return;
        };
        if let Err(e) = src.seek(target).await {
            self.set_status(format!("seek: {e}"));
        } else {
            self.dirty = true;
        }
    }

    async fn transfer_to_selected_device(&mut self) {
        let Some(target) = self.devices.get(self.device_modal_sel).cloned() else {
            self.device_modal_open = false;
            return;
        };
        let Some(spotify) = self.dispatcher.get("spotify").cloned() else {
            self.set_status("transfer: Spotify source not enabled");
            self.device_modal_open = false;
            return;
        };
        match spotify.transfer_to_device(&target.id).await {
            Ok(()) => self.set_status(format!("transfer → {}", target.name)),
            Err(e) => self.set_status(format!("transfer: {e}")),
        }
        self.device_modal_open = false;
        self.dirty = true;
    }

    fn back(&mut self) {
        let cat = self.active_category();
        // Restore origin-tab BEFORE the borrow of `self.category_states`
        // ends, so we can use `self.active_tab_idx` after the if-let.
        let restore_tab: Option<usize> = if let Some(s) = self.category_states.get_mut(&cat) {
            if s.stack.len() > 1 {
                s.stack.pop();
                // Restore the parent's cursor/top stashed at descend time;
                // fall back to (0, 0) if missing (shouldn't happen, but
                // belt-and-braces).
                let (c, t) = s.parent_cursors.pop().unwrap_or((0, 0));
                s.cursor = c;
                s.top = t;
                s.descend_uris.pop();
                let origin = s.origin_tabs.pop().unwrap_or(None);
                self.dirty = true;
                origin
            } else {
                None
            }
        } else {
            None
        };
        if let Some(tab_idx) = restore_tab {
            if tab_idx < self.tabs.len() {
                self.active_tab_idx = tab_idx;
            }
        }
    }

    /// Lazy-load the active browse category's root view if it hasn't been
    /// fetched yet. Single-source for Slice 1: takes the first source whose
    /// scheme can populate this category. Stations is the merge exception.
    pub async fn ensure_active_loaded(&mut self) {
        let cat = self.active_category();
        if !cat.is_browse() {
            return;
        }
        let already = self
            .category_states
            .get(&cat)
            .map(|s| s.loaded)
            .unwrap_or(true);
        if already {
            return;
        }
        let mut view = match self.fetch_category_root(cat).await {
            Ok(v) => v,
            Err(e) => {
                self.set_status(format!("load {}: {e}", cat.label()));
                LibraryView::Entries {
                    scheme: "local",
                    label: cat.label().to_string(),
                    entries: Vec::new(),
                }
            }
        };
        // Apply default sort per category if the user hasn't already
        // picked one. Spotify saved-* endpoints already return newest-first;
        // explicit re-sort here guarantees the order survives view
        // construction and handles the local fallback paths too.
        //
        // Spotify Saved Albums carry release-date as `sort_hint`; user wants
        // newest-released first there. Local Albums have no sort_hint, so
        // keep AlphaAsc for that mode.
        let default_axis = match (cat, self.active_source) {
            (Category::Albums, SourceMode::Spotify) => Some(SortAxis::RecentlyAdded),
            _ => default_sort_for(cat),
        };
        if let Some(s) = self.category_states.get(&cat) {
            if s.sort.is_none() {
                if let Some(axis) = default_axis {
                    sort_library_view(&mut view, axis);
                }
            } else if let Some(axis) = s.sort {
                sort_library_view(&mut view, axis);
            }
        }
        apply_pinning(&mut view, &self.pinned);
        if let Some(s) = self.category_states.get_mut(&cat) {
            // Stash the default axis on the state so the sort modal opens
            // pre-selected to the right entry.
            if s.sort.is_none() {
                s.sort = default_axis;
            }
            s.stack = vec![view];
            s.cursor = 0;
            s.top = 0;
            s.loaded = true;
        }
        self.dirty = true;
    }

    /// Build the initial view for a browse category. Mode-driven: content
    /// pulls from `self.active_source` only. The legacy multi-source `Sections`
    /// merge is gone — toggle `t` to switch sources.
    async fn fetch_category_root(&self, cat: Category) -> Result<LibraryView> {
        match cat {
            Category::Directories => {
                // Local mode landing. Real MPD `lsinfo` walk: top-level dirs
                // + files at the music_directory root. Descend by activating
                // a directory entry (URIs of the form `local:dir:<path>`).
                let src = self
                    .dispatcher
                    .get("local")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Local not enabled"))?;
                let entries = src.browse("local:dir:").await?;
                Ok(LibraryView::Entries {
                    scheme: "local",
                    label: "Directories".into(),
                    entries,
                })
            }
            Category::Albums => match self.active_source {
                SourceMode::Local => {
                    let src = self
                        .dispatcher
                        .get("local")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Local not enabled"))?;
                    let entries = src.browse("").await?;
                    Ok(LibraryView::Entries {
                        scheme: "local",
                        label: "Albums".into(),
                        entries,
                    })
                }
                SourceMode::Spotify => {
                    let src = self
                        .dispatcher
                        .get("spotify")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Spotify not enabled"))?;
                    let entries = src.browse("spotify:view:saved_albums").await?;
                    Ok(LibraryView::Entries {
                        scheme: "spotify",
                        label: "Saved Albums".into(),
                        entries,
                    })
                }
                _ => Err(anyhow::anyhow!("Albums: not available in this mode")),
            },
            Category::Artists => {
                let src = self
                    .dispatcher
                    .get("spotify")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Spotify not enabled"))?;
                let entries = src.browse("spotify:view:followed_artists").await?;
                Ok(LibraryView::Entries {
                    scheme: "spotify",
                    label: "Followed Artists".into(),
                    entries,
                })
            }
            Category::Playlists => match self.active_source {
                SourceMode::Local => {
                    let src = self
                        .dispatcher
                        .get("local")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Local not enabled"))?;
                    let entries = src.browse("local:playlists").await?;
                    Ok(LibraryView::Entries {
                        scheme: "local",
                        label: "Playlists".into(),
                        entries,
                    })
                }
                SourceMode::Spotify => {
                    let src = self
                        .dispatcher
                        .get("spotify")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("Spotify not enabled"))?;
                    let entries = src.browse("spotify:view:playlists").await?;
                    Ok(LibraryView::Entries {
                        scheme: "spotify",
                        label: "Playlists".into(),
                        entries,
                    })
                }
                _ => Err(anyhow::anyhow!("Playlists: not available in this mode")),
            },
            Category::Radio => {
                let src = self
                    .dispatcher
                    .get("radio")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Radio not enabled"))?;
                let entries = src.browse("").await?;
                Ok(LibraryView::Entries {
                    scheme: "radio",
                    label: "Radio".into(),
                    entries,
                })
            }
            Category::SomaFm => {
                let src = self
                    .dispatcher
                    .get("somafm")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("SomaFM not enabled"))?;
                let entries = src.browse("").await?;
                Ok(LibraryView::Entries {
                    scheme: "somafm",
                    label: "SomaFM".into(),
                    entries,
                })
            }
            Category::Spotify => {
                let src = self
                    .dispatcher
                    .get("spotify")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Spotify not enabled"))?;
                let entries = src.browse("").await?;
                Ok(LibraryView::Entries {
                    scheme: "spotify",
                    label: "Library".into(),
                    entries,
                })
            }
            Category::Podcasts => {
                let src = self
                    .dispatcher
                    .get("spotify")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("Spotify not enabled"))?;
                let entries = src.browse("spotify:view:saved_shows").await?;
                Ok(LibraryView::Entries {
                    scheme: "spotify",
                    label: "Podcasts".into(),
                    entries,
                })
            }
            Category::YouTube => {
                let src = self
                    .dispatcher
                    .get("youtube")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("YouTube not enabled"))?;
                let entries = src.browse("youtube:saved").await?;
                Ok(LibraryView::Entries {
                    scheme: "youtube",
                    label: "Saved".into(),
                    entries,
                })
            }
            Category::Stations => {
                // Mode-driven: SomaFm mode → somafm channels, Radio mode →
                // user radio list. Other modes shouldn't render this tab.
                let (scheme, label): (&'static str, &str) = match self.active_source {
                    SourceMode::SomaFm => ("somafm", "SomaFM"),
                    SourceMode::Radio => ("radio", "Radio"),
                    _ => return Err(anyhow::anyhow!("Stations: not available in this mode")),
                };
                let src = self
                    .dispatcher
                    .get(scheme)
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{label} not enabled"))?;
                let entries = src.browse("").await?;
                Ok(LibraryView::Entries {
                    scheme,
                    label: label.to_string(),
                    entries,
                })
            }
            _ => Err(anyhow::anyhow!("non-browse category: {}", cat.label())),
        }
    }


    async fn cycle_thumb_mode(&mut self) {
        // Walk the configured cycle list. If current mode isn't in the
        // list, jump to its head; otherwise step to the next entry,
        // wrapping. Single-entry list = no-op (status only).
        let next = if self.thumb_cycle.is_empty() {
            self.term.mode.cycle()
        } else {
            let cur_idx = self
                .thumb_cycle
                .iter()
                .position(|m| *m == self.term.mode);
            match cur_idx {
                Some(i) => self.thumb_cycle[(i + 1) % self.thumb_cycle.len()],
                None => self.thumb_cycle[0],
            }
        };
        self.term.apply_mode(next);
        self.set_status(format!("thumbs: {}", next.as_str()));
        // Drop all cached protocols so they rebuild with the new picker mode.
        self.protocols.clear();
        self.fetching.clear();
        self.now_playing_protocol = None;
        self.now_playing_uri = None;
        self.now_playing_aspect = None;
        self.refresh_now_playing().await;
    }

    async fn activate(&mut self) -> Result<()> {
        // Command bar Enter routes through the parser; bar wins regardless of tab.
        if self.command_input_focused {
            return self.run_command().await;
        }
        match self.active_category() {
            Category::Queue => self.activate_queue().await,
            Category::Search => self.activate_search().await,
            _ => self.activate_browse().await,
        }
    }

    async fn run_command(&mut self) -> Result<()> {
        let raw = std::mem::take(&mut self.command_buffer);
        self.command_input_focused = false;
        self.dirty = true;
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Ok(());
        }
        let mut parts = trimmed.split_whitespace();
        let cmd = parts.next().unwrap();
        let args: Vec<&str> = parts.collect();
        match cmd {
            "q" | "quit" => self.shutdown.cancel(),
            "add" => {
                let uri = args.join(" ");
                if uri.is_empty() {
                    self.set_status(":add needs a URI");
                } else {
                    self.cmd_add(&uri).await?;
                }
            }
            "play" => {
                let uri = args.join(" ");
                if uri.is_empty() {
                    self.set_status(":play needs a URI");
                } else {
                    self.cmd_play(&uri).await?;
                }
            }
            "goto" => {
                if let Some(idx) = args.first().and_then(|s| s.parse::<usize>().ok()) {
                    self.cmd_goto(idx).await?;
                } else {
                    self.set_status(":goto <n>");
                }
            }
            "vol" => {
                if let Some(v) = args.first().and_then(|s| s.parse::<u8>().ok()) {
                    self.master_volume = v.min(100);
                    self.push_volume().await;
                    self.set_status(format!("vol: {}%", self.master_volume));
                } else {
                    self.set_status(":vol <0..100>");
                }
            }
            "clear" => self.clear_queue().await,
            other => self.set_status(format!("unknown command: {other}")),
        }
        Ok(())
    }

    async fn cmd_add(&mut self, uri: &str) -> Result<()> {
        let scheme = uri.split(':').next().unwrap_or("");
        let src = self
            .dispatcher
            .get(scheme)
            .ok_or_else(|| anyhow::anyhow!("no source for scheme: {scheme}"))?
            .clone();
        let scheme_static = src.scheme();
        let qi = QueuedItem {
            source_scheme: scheme_static,
            uri: uri.to_string(),
            display: crate::types::ItemDisplay {
                title: uri.to_string(),
                artist: None,
                album: None,
                art_uri: None,
                art_uri_full: None,
                duration: None,
                sort_hint: None,
                track_no: None,
            },
        };
        self.queue.push(qi);
        self.set_status(format!("queued: {uri}"));
        Ok(())
    }

    async fn cmd_play(&mut self, uri: &str) -> Result<()> {
        let scheme = uri.split(':').next().unwrap_or("");
        let src = self
            .dispatcher
            .get(scheme)
            .ok_or_else(|| anyhow::anyhow!("no source for scheme: {scheme}"))?
            .clone();
        let scheme_static = src.scheme();
        let qi = QueuedItem {
            source_scheme: scheme_static,
            uri: uri.to_string(),
            display: crate::types::ItemDisplay {
                title: uri.to_string(),
                artist: None,
                album: None,
                art_uri: None,
                art_uri_full: None,
                duration: None,
                sort_hint: None,
                track_no: None,
            },
        };
        self.queue.push(qi.clone());
        let last = self.queue.len() - 1;
        self.queue.set_current(last);
        let vol = self.master_volume;
        self.dispatcher.play(&qi, vol).await?;
        self.set_status(format!("playing: {uri}"));
        self.refresh_now_playing().await;
        Ok(())
    }

    async fn cmd_goto(&mut self, idx: usize) -> Result<()> {
        if idx >= self.queue.len() {
            self.set_status(format!(
                ":goto out of range (queue len {})",
                self.queue.len()
            ));
            return Ok(());
        }
        let item = self.queue.items()[idx].clone();
        self.queue.set_current(idx);
        let vol = self.master_volume;
        self.dispatcher.play(&item, vol).await?;
        self.refresh_now_playing().await;
        self.set_status(format!("queue index {idx}"));
        Ok(())
    }

    /// Add the cursor's current item to the queue without changing the
    /// currently-playing track. Mirrors `activate()` for the play paths but
    /// skips `dispatcher.play()` + `set_current()`. Browse/descend behavior
    /// stays the same as Activate.
    async fn enqueue_current(&mut self) -> Result<()> {
        match self.active_category() {
            Category::Queue => Ok(()),
            Category::Search => self.enqueue_search().await,
            _ => self.enqueue_browse().await,
        }
    }

    async fn enqueue_browse(&mut self) -> Result<()> {
        let mapped_cursor = self.filtered_browse_cursor_to_orig(
            self.category_states
                .get(&self.active_category())
                .map(|s| s.cursor)
                .unwrap_or(0),
        );
        let cat = self.active_category();
        let Some(state) = self.category_states.get(&cat) else {
            return Ok(());
        };
        let cur = mapped_cursor.unwrap_or(state.cursor);
        let qi: Option<QueuedItem> = match state.stack.last() {
            Some(LibraryView::Entries {
                scheme, entries, ..
            }) => entries.get(cur).and_then(|e| match e.kind {
                EntryKind::Track => Some(QueuedItem {
                    source_scheme: scheme,
                    uri: e.uri.clone(),
                    display: e.display.clone().unwrap_or(crate::types::ItemDisplay {
                        title: e.label.clone(),
                        artist: None,
                        album: None,
                        art_uri: None,
                        art_uri_full: None,
                        duration: None,
                        sort_hint: None,
                        track_no: None,
                    }),
                }),
                _ => None,
            }),
            Some(LibraryView::Tracks { items, .. }) => items.get(cur).map(|it| QueuedItem {
                source_scheme: "local",
                uri: it.uri.clone(),
                display: it.display.clone(),
            }),
            Some(LibraryView::Sections { sections, .. }) => {
                sections_row_at(sections, cur).and_then(|hit| match hit {
                    SectionHit::Header => None,
                    SectionHit::Entry { scheme, entry } => match entry.kind {
                        EntryKind::Track => Some(QueuedItem {
                            source_scheme: scheme,
                            uri: entry.uri.clone(),
                            display: entry.display.clone().unwrap_or(crate::types::ItemDisplay {
                                title: entry.label.clone(),
                                artist: None,
                                album: None,
                                art_uri: None,
                                art_uri_full: None,
                                duration: None,
                                sort_hint: None,
                                track_no: None,
                            }),
                        }),
                        _ => None,
                    },
                })
            }
            None => None,
        };
        if let Some(qi) = qi {
            let title = qi.display.title.clone();
            self.queue.push_manual(qi);
            self.set_status(format!("queued: {title}"));
            self.dirty = true;
        } else {
            self.set_status("not a track");
        }
        Ok(())
    }

    async fn enqueue_search(&mut self) -> Result<()> {
        let mut idx = self.search_cursor;
        for group in &self.search_results {
            if idx < group.items.len() {
                let item = group.items[idx].clone();
                let scheme = group.scheme;
                let qi = QueuedItem {
                    source_scheme: scheme,
                    uri: item.uri.clone(),
                    display: item.display.clone(),
                };
                let title = qi.display.title.clone();
                self.queue.push_manual(qi);
                self.set_status(format!("queued: {title}"));
                self.dirty = true;
                return Ok(());
            }
            idx -= group.items.len();
        }
        Ok(())
    }

    async fn activate_search(&mut self) -> Result<()> {
        if self.search_input_focused {
            // Enter inside the input box runs the query.
            return self.run_search().await;
        }
        // Otherwise: cursor sits on a flattened row — find which group/item.
        let mut idx = self.search_cursor;
        for group in &self.search_results {
            if idx < group.items.len() {
                let item = group.items[idx].clone();
                let scheme = group.scheme;
                // Collection URIs (album / playlist / show / artist) descend
                // into their tracks/episodes via the source's browse(). Plain
                // track URIs (or bare base62 IDs) fall through to play.
                if scheme == "spotify"
                    && (item.uri.starts_with("spotify:album:")
                        || item.uri.starts_with("spotify:playlist:")
                        || item.uri.starts_with("spotify:show:")
                        || item.uri.starts_with("spotify:artist:"))
                {
                    return self.descend_from_search(scheme, &item).await;
                }
                let qi = QueuedItem {
                    source_scheme: scheme,
                    uri: item.uri.clone(),
                    display: item.display.clone(),
                };
                self.queue.push(qi.clone());
                let last = self.queue.len() - 1;
                self.queue.set_current(last);
                let vol = self.master_volume;
                self.dispatcher.play(&qi, vol).await?;
                self.set_status(format!("playing: {}", item.display.title));
                self.refresh_now_playing().await;
                return Ok(());
            }
            idx -= group.items.len();
        }
        Ok(())
    }

    /// Switch to the source's library/landing tab, browse the collection URI,
    /// and push the result onto the category stack. Lets Enter on an album /
    /// playlist / podcast / artist search result open its contents instead of
    /// no-op'ing on "not a track URI".
    async fn descend_from_search(&mut self, scheme: &'static str, item: &Item) -> Result<()> {
        let src = self
            .dispatcher
            .get(scheme)
            .ok_or_else(|| anyhow::anyhow!("source missing: {scheme}"))?
            .clone();
        let entries = src.browse(&item.uri).await?;
        // Pick a landing tab for this scheme. For Spotify, the `Spotify`
        // category is the catch-all library/landing view; pushing onto its
        // stack lands the user on the descended collection.
        let target_cat = if scheme == "spotify" {
            crate::types::Category::Spotify
        } else {
            crate::types::Category::Albums
        };
        // Record the originating tab BEFORE switching, so back() can
        // return the user to the Search tab they descended from.
        let origin_tab_idx = self.active_tab_idx;
        let switched_tabs =
            if let Some(idx) = self.tabs.iter().position(|c| *c == target_cat) {
                let crossed = idx != self.active_tab_idx;
                self.active_tab_idx = idx;
                crossed
            } else {
                false
            };
        let s = self.category_states.entry(target_cat).or_default();
        s.parent_cursors.push((s.cursor, s.top));
        s.descend_uris.push(item.uri.clone());
        s.origin_tabs
            .push(if switched_tabs { Some(origin_tab_idx) } else { None });
        s.stack.push(LibraryView::Entries {
            scheme,
            label: item.display.title.clone(),
            entries,
        });
        s.cursor = 0;
        s.top = 0;
        self.set_status(format!("opened: {}", item.display.title));
        self.dirty = true;
        Ok(())
    }

    /// Fan out the current query across every registered source in parallel.
    /// Empty query clears results.
    pub async fn run_search(&mut self) -> Result<()> {
        let q = self.search_query.trim().to_string();
        if q.is_empty() {
            self.search_results.clear();
            self.search_cursor = 0;
            self.search_input_focused = false;
            self.dirty = true;
            return Ok(());
        }
        // Mode-driven: search the active source only. `t` cycles which.
        let active_scheme = self.active_source.scheme();
        let schemes: Vec<&'static str> = self
            .dispatcher
            .schemes()
            .filter(|s| *s == active_scheme)
            .collect();
        let sources: Vec<(&'static str, std::sync::Arc<dyn crate::source::MusicSource>)> = schemes
            .iter()
            .filter_map(|s| self.dispatcher.get(s).map(|src| (*s, src.clone())))
            .collect();
        self.set_status(format!("searching: {q}"));
        tracing::info!("run_search: query={q:?} sources={}", sources.len());

        let futs = sources.into_iter().map(|(scheme, src)| {
            let q = q.clone();
            async move {
                let res = src.search(&q).await;
                (scheme, res)
            }
        });
        let outcomes = futures::future::join_all(futs).await;

        let mut groups: Vec<SearchGroup> = Vec::new();
        for (scheme, res) in outcomes {
            match res {
                Ok(items) if !items.is_empty() => groups.push(SearchGroup { scheme, items }),
                Ok(_) => {}
                Err(e) => tracing::warn!("search {scheme}: {e}"),
            }
        }
        let total: usize = groups.iter().map(|g| g.items.len()).sum();
        self.search_results = groups;
        self.search_cursor = 0;
        self.search_top = 0;
        self.search_input_focused = false;
        self.set_status(format!("results: {total}"));
        self.dirty = true;
        Ok(())
    }

    async fn activate_browse(&mut self) -> Result<()> {
        // Remap the cursor through the active filter (`/`) so the user picks
        // the row they actually see, not the row at the same index in the
        // unfiltered list.
        let mapped_cursor = self.filtered_browse_cursor_to_orig(
            self.category_states
                .get(&self.active_category())
                .map(|s| s.cursor)
                .unwrap_or(0),
        );
        let cat = self.active_category();
        let Some(state) = self.category_states.get(&cat) else {
            return Ok(());
        };
        let cur = mapped_cursor.unwrap_or(state.cursor);
        let action = match state.stack.last() {
            Some(LibraryView::Entries {
                scheme, entries, ..
            }) => entries.get(cur).map(|e| match e.kind {
                EntryKind::Track => {
                    let _ = scheme;
                    LibraryActivate::PlayEntry { entry: e.clone() }
                }
                EntryKind::Directory if e.uri.contains("?offset=") => {
                    LibraryActivate::ExtendCurrent {
                        scheme,
                        uri: e.uri.clone(),
                    }
                }
                EntryKind::Album if *scheme == "local" => LibraryActivate::ExpandAlbum {
                    label: e.label.clone(),
                },
                EntryKind::Album
                | EntryKind::Directory
                | EntryKind::Artist
                | EntryKind::Playlist => LibraryActivate::DescendEntry {
                    scheme,
                    uri: e.uri.clone(),
                    label: e.label.clone(),
                },
            }),
            Some(LibraryView::Tracks { items, .. }) => {
                items.get(cur).map(|it| LibraryActivate::PlayItem { item: it.clone() })
            }
            Some(LibraryView::Sections { sections, .. }) => {
                sections_row_at(sections, cur).and_then(|hit| match hit {
                    SectionHit::Header => None,
                    SectionHit::Entry { scheme, entry } => Some(match entry.kind {
                        EntryKind::Track => LibraryActivate::PlayEntry {
                            entry: entry.clone(),
                        },
                        EntryKind::Directory if entry.uri.contains("?offset=") => {
                            LibraryActivate::ExtendCurrent {
                                scheme,
                                uri: entry.uri.clone(),
                            }
                        }
                        _ => LibraryActivate::DescendEntry {
                            scheme,
                            uri: entry.uri.clone(),
                            label: entry.label.clone(),
                        },
                    }),
                })
            }
            None => None,
        };

        let Some(action) = action else { return Ok(()) };
        match action {
            LibraryActivate::DescendEntry { scheme, uri, label } => {
                let src = self
                    .dispatcher
                    .get(scheme)
                    .ok_or_else(|| anyhow::anyhow!("source missing: {scheme}"))?
                    .clone();
                // Push an empty Entries view synchronously so the user gets
                // immediate feedback (breadcrumb advances, "loading…" state
                // is visible). The streaming task then appends rows as each
                // pagination page lands; `handle_row_batch` runs auto-sort
                // detection once the stream finishes.
                let view_id = if let Some(s) = self.category_states.get_mut(&cat) {
                    s.parent_cursors.push((s.cursor, s.top));
                    s.descend_uris.push(uri.clone());
                    s.origin_tabs.push(None);
                    s.stack.push(LibraryView::Entries {
                        scheme,
                        label,
                        entries: Vec::new(),
                    });
                    s.cursor = 0;
                    s.top = 0;
                    s.descend_epoch = s.descend_epoch.wrapping_add(1);
                    Some(ViewId {
                        category: cat,
                        depth: s.stack.len(),
                        epoch: s.descend_epoch,
                    })
                } else {
                    None
                };
                self.dirty = true;
                if let Some(view_id) = view_id {
                    let row_tx = self.row_batch_tx.clone();
                    let uri_owned = uri.clone();
                    tokio::spawn(async move {
                        let (batch_tx, mut batch_rx) =
                            tokio::sync::mpsc::channel::<Result<Vec<Entry>>>(16);
                        let stream_fut = async move {
                            src.browse_streaming(&uri_owned, batch_tx).await;
                        };
                        let forward_fut = async {
                            while let Some(batch) = batch_rx.recv().await {
                                if row_tx
                                    .send(RowBatch { view_id, batch, finished: false })
                                    .is_err()
                                {
                                    return;
                                }
                            }
                            let _ = row_tx.send(RowBatch {
                                view_id,
                                batch: Ok(Vec::new()),
                                finished: true,
                            });
                        };
                        tokio::join!(stream_fut, forward_fut);
                    });
                }
            }
            LibraryActivate::ExpandAlbum { label } => {
                let items = self.local.songs_in_album(&label).await?;
                if let Some(s) = self.category_states.get_mut(&cat) {
                    s.parent_cursors.push((s.cursor, s.top));
                    s.origin_tabs.push(None);
                    // Local album view: sort by track # when MPD plumbed it.
                    // Falls back to alpha for files without track tag.
                    let mut view = LibraryView::Tracks { label, items };
                    sort_library_view(&mut view, SortAxis::TrackNumber);
                    s.stack.push(view);
                    s.cursor = 0;
                    s.top = 0;
                }
                self.dirty = true;
            }
            LibraryActivate::ExtendCurrent { scheme, uri } => {
                let src = self
                    .dispatcher
                    .get(scheme)
                    .ok_or_else(|| anyhow::anyhow!("source missing: {scheme}"))?
                    .clone();
                let more = src.browse(&uri).await?;
                if let Some(s) = self.category_states.get_mut(&cat) {
                    if let Some(LibraryView::Entries { entries, .. }) = s.stack.last_mut() {
                        entries.pop();
                        let cursor = entries.len();
                        entries.extend(more);
                        s.cursor = cursor;
                        self.dirty = true;
                    }
                }
            }
            LibraryActivate::PlayEntry { entry } => {
                self.play_track_in_context(&entry.uri).await?;
            }
            LibraryActivate::PlayItem { item } => {
                self.play_track_in_context(&item.uri).await?;
            }
        }
        Ok(())
    }

    /// Smart auto-queue: replace the auto-queue with every track in the
    /// current view (preserving manual prefix), set current to the picked
    /// track, play. Honors shuffle by shuffling the auto-section once on
    /// insert. `selected_uri` is the user-clicked track's URI, used to find
    /// the offset within the view's track list after any filtering.
    async fn play_track_in_context(&mut self, selected_uri: &str) -> Result<()> {
        let cat = self.active_category();
        let tracks: Vec<QueuedItem> = match self
            .category_states
            .get(&cat)
            .and_then(|s| s.stack.last())
        {
            Some(LibraryView::Entries {
                scheme, entries, ..
            }) => entries
                .iter()
                .filter(|e| matches!(e.kind, EntryKind::Track))
                .map(|e| entry_to_queued(scheme, e))
                .collect(),
            Some(LibraryView::Tracks { items, .. }) => items
                .iter()
                .map(|it| QueuedItem {
                    source_scheme: "local",
                    uri: it.uri.clone(),
                    display: it.display.clone(),
                })
                .collect(),
            Some(LibraryView::Sections { sections, .. }) => sections
                .iter()
                .flat_map(|sec| {
                    let scheme = sec.scheme;
                    sec.entries
                        .iter()
                        .filter(|e| matches!(e.kind, EntryKind::Track))
                        .map(move |e| entry_to_queued(scheme, e))
                })
                .collect(),
            None => Vec::new(),
        };
        if tracks.is_empty() {
            return Ok(());
        }
        let mut tracks = tracks;
        let mut chosen_idx = tracks
            .iter()
            .position(|qi| qi.uri == selected_uri)
            .unwrap_or(0);
        if self.shuffle {
            shuffle_in_place(&mut tracks);
            chosen_idx = tracks
                .iter()
                .position(|qi| qi.uri == selected_uri)
                .unwrap_or(0);
        }
        let title = tracks[chosen_idx].display.title.clone();
        self.queue.replace_auto(tracks, chosen_idx);
        let item = self
            .queue
            .current()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("queue empty after replace"))?;
        let vol = self.master_volume;
        self.dispatcher.play(&item, vol).await?;
        self.set_status(format!("playing: {title}"));
        self.refresh_now_playing().await;
        Ok(())
    }

    async fn activate_queue(&mut self) -> Result<()> {
        // When a `/` filter is active, queue_cursor indexes the FILTERED row
        // list shown to the user. Translate it to an original queue index
        // before touching the queue or playback engine.
        let orig_idx = self
            .filtered_queue_cursor_to_orig(self.queue_cursor)
            .unwrap_or(self.queue_cursor);
        if let Some(qi) = self.queue.items().get(orig_idx).cloned() {
            self.queue.set_current(orig_idx);
            let vol = self.master_volume;
            self.dispatcher.play(&qi, vol).await?;
            self.set_status(format!("playing: {}", qi.display.title));
            self.refresh_now_playing().await;
        }
        Ok(())
    }

    /// Apply the modal's selected sort axis to the active category's top
    /// view. Year / RecentlyAdded fall back to alpha-asc with a status toast
    /// since fuga's `ItemDisplay` doesn't yet plumb those fields through
    /// from the sources.
    fn apply_selected_sort(&mut self) {
        let axis = match SortAxis::all().get(self.sort_modal_sel) {
            Some(a) => *a,
            None => {
                self.sort_modal_open = false;
                return;
            }
        };
        let cat = self.active_category();
        if let Some(s) = self.category_states.get_mut(&cat) {
            s.sort = Some(axis);
            if let Some(view) = s.stack.last_mut() {
                sort_library_view(view, axis);
            }
            s.cursor = 0;
            s.top = 0;
        }
        self.sort_modal_open = false;
        let label = match axis {
            SortAxis::AlphaAsc => "sort: A-Z",
            SortAxis::AlphaDesc => "sort: Z-A",
            SortAxis::Duration => "sort: duration",
            SortAxis::Year => "sort: year (alpha fallback)",
            SortAxis::RecentlyAdded => "sort: recently-added",
            SortAxis::TrackNumber => "sort: track #",
        };
        self.set_status(label);
    }

    /// Scroll the active list to the row that's currently playing. Queue
    /// tab snaps the cursor to `queue.current_index()`; browse tabs scan
    /// the topmost view's items for a matching `(scheme, uri)` and move
    /// the cursor there. No-op (with a toast) when the playing track isn't
    /// reachable from the active view.
    fn follow_playing(&mut self) {
        let Some(cur) = self.queue.current().cloned() else {
            self.set_status("nothing playing");
            return;
        };
        let cat = self.active_category();
        if matches!(cat, Category::Queue) {
            if let Some(idx) = self.queue.current_index() {
                self.queue_cursor = idx;
                self.dirty = true;
            }
            return;
        }
        if !cat.is_browse() {
            self.set_status("follow: switch to Queue or a library tab");
            return;
        }
        let Some(state) = self.category_states.get_mut(&cat) else {
            self.set_status("follow: view not loaded yet");
            return;
        };
        let Some(view) = state.stack.last() else {
            self.set_status("follow: empty view");
            return;
        };
        let target_uri = cur.uri.as_str();
        let target_scheme = cur.source_scheme;
        let idx: Option<usize> = match view {
            LibraryView::Entries { scheme, entries, .. } => {
                if *scheme == target_scheme {
                    entries.iter().position(|e| e.uri == target_uri)
                } else {
                    None
                }
            }
            LibraryView::Tracks { items, .. } => {
                if target_scheme == "local" {
                    items.iter().position(|it| it.uri == target_uri)
                } else {
                    None
                }
            }
            LibraryView::Sections { sections, .. } => {
                let mut row = 0usize;
                let mut found: Option<usize> = None;
                for sec in sections {
                    row += 1; // section header row
                    if sec.scheme == target_scheme {
                        if let Some(off) = sec.entries.iter().position(|e| e.uri == target_uri) {
                            found = Some(row + off);
                            break;
                        }
                    }
                    row += sec.entries.len();
                }
                found
            }
        };
        if let Some(i) = idx {
            state.cursor = i;
            self.dirty = true;
            return;
        }
        // Not in this view — fall back to the Queue tab so the user can
        // see what's playing without searching every category. Move cursor
        // to the playing index there too.
        if let Some(qidx) = self.tabs.iter().position(|c| matches!(c, Category::Queue)) {
            self.active_tab_idx = qidx;
            if let Some(cur) = self.queue.current_index() {
                self.queue_cursor = cur;
            }
            self.set_status("jumped to Queue (not in current view)");
            self.dirty = true;
        } else {
            self.set_status("not in current view");
        }
    }

    async fn refresh_current_view(&mut self) {
        // Re-fetch the topmost view of the active browse category. If the
        // active tab is non-browse (Queue/Search) or the active category has
        // no state, mark dirty and bail.
        let cat = self.active_category();
        if !cat.is_browse() {
            self.dirty = true;
            return;
        }
        // Drop the loaded flag and re-run the lazy fetch so the category's
        // root view comes back fresh. Refreshing intermediate descents would
        // need the original browse path, which we don't preserve in the
        // stack — out of scope for Slice 1.
        if let Some(s) = self.category_states.get_mut(&cat) {
            s.stack.clear();
            s.parent_cursors.clear();
            s.descend_uris.clear();
            s.origin_tabs.clear();
            s.loaded = false;
            s.cursor = 0;
            s.top = 0;
        }
        self.ensure_active_loaded().await;
        self.set_status("refreshed");
    }

    async fn toggle_pause(&mut self) -> Result<()> {
        // Pick branch from cached playback state instead of trial-and-erroring
        // resume → pause. Spotify's resume returns Ok unconditionally, so the
        // old fallback never fired and Space never actually paused.
        let paused = matches!(
            self.playback.as_ref().map(|p| p.state),
            Some(PlayState::Paused)
        );
        if paused {
            self.dispatcher.resume().await?;
        } else {
            self.dispatcher.pause().await?;
        }
        Ok(())
    }

    /// Toggle the saved/liked state. Targets the hovered row when one is
    /// visible (any browse list / queue), falling back to the currently
    /// playing track when nothing is hovered (e.g. on the Now Playing tab).
    /// Refreshes `current_liked` only when toggling the currently-playing
    /// track so the bottom-bar star stays accurate.
    async fn toggle_like(&mut self) {
        // Resolve target URI + source scheme. Hovered wins.
        let (uri, scheme, is_current_track): (String, String, bool) =
            if let Some(uri) = self.hovered_uri() {
                let scheme = uri
                    .split(':')
                    .next()
                    .unwrap_or("")
                    .to_string();
                let is_current = self
                    .queue
                    .current()
                    .map(|c| c.uri == uri)
                    .unwrap_or(false);
                (uri, scheme, is_current)
            } else if let Some(cur) = self.queue.current().cloned() {
                (cur.uri, cur.source_scheme.to_string(), true)
            } else {
                self.set_status("like: nothing under cursor or playing");
                return;
            };
        let Some(src) = self.dispatcher.get(scheme.as_str()).cloned() else {
            self.set_status(format!("like: source `{scheme}` not enabled"));
            return;
        };
        // Probe current saved-state for the target so the toggle direction
        // matches reality (don't trust `current_liked` — it tracks the
        // currently playing track, not the hovered row).
        let was_liked = match src.is_saved(&uri).await {
            Ok(b) => b,
            Err(e) => {
                self.set_status(format!("like: {e}"));
                return;
            }
        };
        let res = if was_liked {
            src.unsave(&uri).await
        } else {
            src.save(&uri).await
        };
        match res {
            Ok(_) => {
                if is_current_track {
                    self.current_liked = Some(!was_liked);
                }
                // Invalidate the YouTube Saved view so the new entry shows
                // up immediately when the user switches to the tab.
                if scheme == "youtube" {
                    if let Some(s) = self.category_states.get_mut(&Category::YouTube) {
                        s.stack.clear();
                    }
                }
                self.set_status(if was_liked { "unliked" } else { "liked" });
                self.dirty = true;
            }
            Err(e) => self.set_status(format!("like failed: {e}")),
        }
    }

    /// Refresh `current_liked` from the source. No-op if no current track or
    /// the source doesn't track saves.
    async fn refresh_liked(&mut self) {
        let Some(cur) = self.queue.current().cloned() else {
            self.current_liked = None;
            return;
        };
        let Some(src) = self.dispatcher.get(cur.source_scheme).cloned() else {
            self.current_liked = None;
            return;
        };
        match src.is_saved(&cur.uri).await {
            Ok(b) => self.current_liked = Some(b),
            Err(e) => {
                tracing::debug!("is_saved failed: {e}");
                self.current_liked = None;
            }
        }
    }

    /// Write current cross-run state (art_collapsed) to disk. Silent on
    /// IO error — losing the state is harmless, next launch falls back to
    /// the config default.
    fn persist_state(&self) {
        if let Some(p) = &self.state_path {
            let mut pinned: Vec<String> = self.pinned.iter().cloned().collect();
            pinned.sort();
            crate::app_state::AppState {
                art_collapsed: self.art_collapsed,
                pinned,
            }
            .save(p);
        }
    }

    /// Toggle pin state on the hovered row's URI. Persists to state.json
    /// so the pin survives restart. New pins land at the top of the next
    /// re-fetched view (existing views show the change after refresh).
    fn toggle_pin_hovered(&mut self) {
        let Some(uri) = self.hovered_uri() else {
            self.set_status("pin: nothing under cursor");
            return;
        };
        let added = if self.pinned.remove(&uri) {
            false
        } else {
            self.pinned.insert(uri.clone());
            true
        };
        // Re-pin the active view in-place so the user sees the change
        // without needing to re-fetch.
        let cat = self.active_category();
        if let Some(s) = self.category_states.get_mut(&cat) {
            if let Some(top) = s.stack.last_mut() {
                apply_pinning(top, &self.pinned);
            }
        }
        self.persist_state();
        self.set_status(if added { "pinned" } else { "unpinned" });
        self.dirty = true;
    }

    /// EntryKind of the row under the cursor, when the active view carries
    /// one (browse Entries / Sections). Track lists and the Queue return
    /// `Track`. Returns `None` if no row is hovered.
    fn hovered_kind(&self) -> Option<EntryKind> {
        match self.active_category() {
            Category::Queue => Some(EntryKind::Track),
            _ => {
                let cat = self.active_category();
                let s = self.category_states.get(&cat)?;
                let cur = self.filtered_browse_cursor_to_orig(s.cursor).unwrap_or(s.cursor);
                match s.stack.last()? {
                    LibraryView::Entries { entries, .. } => Some(entries.get(cur)?.kind.clone()),
                    LibraryView::Tracks { .. } => Some(EntryKind::Track),
                    LibraryView::Sections { sections, .. } => {
                        let hit = sections_row_at(sections, cur)?;
                        if let SectionHit::Entry { entry, .. } = hit {
                            Some(entry.kind.clone())
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    /// URI of the row under the cursor (any browse view + queue). Falls
    /// back to `display.art_uri`-based heuristic when raw `uri` isn't on
    /// the row (rare).
    fn hovered_uri(&self) -> Option<String> {
        match self.active_category() {
            Category::Queue => {
                let q = self.queue.items().get(self.queue_cursor)?;
                Some(q.uri.clone())
            }
            Category::Search => {
                let mut idx = self.search_cursor;
                for group in &self.search_results {
                    if idx < group.items.len() {
                        return Some(group.items.get(idx)?.uri.clone());
                    }
                    idx -= group.items.len();
                }
                None
            }
            _ => {
                let cat = self.active_category();
                let s = self.category_states.get(&cat)?;
                let cur = self.filtered_browse_cursor_to_orig(s.cursor).unwrap_or(s.cursor);
                match s.stack.last()? {
                    LibraryView::Entries { entries, .. } => {
                        Some(entries.get(cur)?.uri.clone())
                    }
                    LibraryView::Tracks { items, .. } => {
                        Some(items.get(cur)?.uri.clone())
                    }
                    LibraryView::Sections { sections, .. } => {
                        let hit = sections_row_at(sections, cur)?;
                        if let SectionHit::Entry { entry, .. } = hit {
                            Some(entry.uri.clone())
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    fn open_action_menu(&mut self) {
        if self.hovered_uri().is_none() {
            self.set_status("action menu: nothing under cursor");
            return;
        }
        self.action_menu_open = true;
        self.action_menu_sel = 0;
        self.dirty = true;
    }

    /// Returns the labels for action-menu rows in selection order. Items
    /// adapt to the hovered row's EntryKind + source. A row that doesn't
    /// support an action (e.g. "Go to artist" on an Artist) drops it from
    /// the menu so the user only sees what's actionable.
    pub fn action_menu_labels(&self) -> Vec<&'static str> {
        let uri = self.hovered_uri().unwrap_or_default();
        let is_spotify = uri.starts_with("spotify:");
        let is_spotify_track = uri.starts_with("spotify:track:");
        let is_spotify_show = uri.starts_with("spotify:show:");
        let is_youtube = uri.starts_with("youtube:");
        let pinned = self.pinned.contains(&uri);
        let kind = self.hovered_kind();
        let mut out = Vec::new();
        out.push(if pinned { "Unpin" } else { "Pin to top" });

        use EntryKind::*;
        let queueable = matches!(kind, Some(Track) | Some(Album) | Some(Directory))
            || is_spotify_show
            || is_youtube;
        if queueable {
            out.push("Add to queue");
        }

        if is_spotify_track {
            out.push("Like / Unlike");
            out.push("Go to artist");
            out.push("Go to album");
            out.push("Add to playlist");
            // "Remove from playlist" only when the current browse view is
            // a Spotify playlist (i.e. we're looking at tracks inside one).
            if self
                .current_descend_uri()
                .map(|u| u.starts_with("spotify:playlist:"))
                .unwrap_or(false)
            {
                out.push("Remove from playlist");
            }
            out.push("Song radio");
        } else if is_spotify && matches!(kind, Some(Album)) {
            out.push("Go to artist");
        } else if is_youtube {
            out.push("Like / Unlike");
            out.push("Download");
        }
        out
    }

    pub async fn run_action_menu(&mut self) {
        let labels = self.action_menu_labels();
        let Some(&label) = labels.get(self.action_menu_sel) else {
            return;
        };
        self.action_menu_open = false;
        self.dirty = true;
        match label {
            "Pin to top" | "Unpin" => self.toggle_pin_hovered(),
            "Add to queue" => {
                // Re-use the existing enqueue flow (action `a`): push hovered
                // row as a manual queue insert.
                if let Err(e) = self.handle_action(Action::Enqueue).await {
                    self.set_status(format!("enqueue: {e}"));
                }
            }
            "Like / Unlike" => self.toggle_like().await,
            "Go to album" => self.navigate_to_relation("album").await,
            "Go to artist" => self.navigate_to_relation("artist").await,
            "Add to playlist" => self.open_playlist_picker().await,
            "Remove from playlist" => self.remove_hovered_from_current_playlist().await,
            "Song radio" => self.queue_song_radio().await,
            "Download" => self.download_hovered().await,
            _ => {}
        }
    }

    /// Download the hovered YouTube track to disk via yt-dlp. Spawns the
    /// work in a background task so the UI keeps rendering; the
    /// `download_progress` atomic is updated as percentages stream in
    /// from yt-dlp's stderr, and the status toast carries a terse
    /// completion / failure message at the end.
    async fn download_hovered(&mut self) {
        let Some(uri) = self.hovered_uri() else {
            self.set_status("nothing under cursor");
            return;
        };
        if !uri.starts_with("youtube:") {
            self.set_status("not a YouTube track");
            return;
        }
        let Some(src) = self.dispatcher.get("youtube").cloned() else {
            self.set_status("YouTube not enabled");
            return;
        };
        use std::sync::atomic::Ordering;
        if self.download_progress.load(Ordering::Relaxed) <= 100 {
            self.set_status("download in progress");
            return;
        }
        self.download_progress.store(0, Ordering::Relaxed);
        self.set_status("downloading");
        self.dirty = true;
        let progress = self.download_progress.clone();
        let wake = self.wake_tx.clone();
        let toast = self.toast_inbox.clone();
        tokio::spawn(async move {
            let res = src.download(&uri, Some(progress.clone())).await;
            progress.store(255, Ordering::Relaxed);
            let msg = match res {
                Ok(path) => {
                    tracing::info!("youtube download done: {}", path.display());
                    "downloaded".to_string()
                }
                Err(e) => {
                    tracing::warn!("youtube download failed: {e:#}");
                    format!("download: {}", short_err(&e))
                }
            };
            if let Ok(mut g) = toast.lock() {
                *g = Some(msg);
            }
            let _ = wake.send(());
        });
    }

    /// Drain the toast inbox written by background tasks (e.g. the
    /// YouTube download task) into the normal status line. Called on
    /// each wake.
    pub fn drain_toast_inbox(&mut self) {
        let msg = {
            let mut g = match self.toast_inbox.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            g.take()
        };
        if let Some(msg) = msg {
            self.set_status(msg);
        }
    }

    /// Remove the hovered Spotify track from the playlist currently being
    /// viewed. Looks up the playlist URI via `current_descend_uri`. On
    /// success, refreshes the view so the row disappears.
    async fn remove_hovered_from_current_playlist(&mut self) {
        let Some(track_uri) = self.hovered_uri() else {
            self.set_status("remove: nothing under cursor");
            return;
        };
        if !track_uri.starts_with("spotify:track:") {
            self.set_status("remove: not a Spotify track");
            return;
        }
        let Some(playlist_uri) = self.current_descend_uri().map(|s| s.to_string()) else {
            self.set_status("remove: not inside a playlist");
            return;
        };
        if !playlist_uri.starts_with("spotify:playlist:") {
            self.set_status("remove: current view isn't a playlist");
            return;
        }
        let Some(src) = self.dispatcher.get("spotify").cloned() else {
            self.set_status("remove: Spotify source not enabled");
            return;
        };
        match src.remove_from_playlist(&playlist_uri, &track_uri).await {
            Ok(()) => {
                self.set_status("removed from playlist");
                self.refresh_current_view().await;
            }
            Err(e) => self.set_status(format!("remove from playlist: {e}")),
        }
    }

    /// Commit the playlist picker selection: call add_to_playlist on the
    /// Spotify source with the selected playlist URI + the cached track
    /// URI. Closes the modal regardless of result.
    async fn commit_playlist_picker(&mut self) {
        let Some(picker) = self.playlist_picker.take() else {
            return;
        };
        self.dirty = true;
        let Some(pl) = picker.entries.get(picker.sel) else {
            self.set_status("add to playlist: no selection");
            return;
        };
        let Some(src) = self.dispatcher.get("spotify").cloned() else {
            self.set_status("add to playlist: Spotify source not enabled");
            return;
        };
        match src.add_to_playlist(&pl.uri, &picker.track_uri).await {
            Ok(()) => self.set_status(format!("added to {}", pl.label)),
            Err(e) => self.set_status(format!("add to playlist: {e}")),
        }
    }

    /// Open the Add-to-Playlist picker: load the user's writable
    /// playlists, render a modal, and on select call playlist_add_items
    /// with the hovered Spotify track URI.
    async fn open_playlist_picker(&mut self) {
        let Some(uri) = self.hovered_uri() else {
            self.set_status("add to playlist: nothing under cursor");
            return;
        };
        if !uri.starts_with("spotify:track:") {
            self.set_status("add to playlist: not a Spotify track");
            return;
        }
        let Some(src) = self.dispatcher.get("spotify").cloned() else {
            self.set_status("add to playlist: Spotify source not enabled");
            return;
        };
        let playlists = match src.browse("spotify:view:saved_playlists_picker").await {
            Ok(v) => v,
            Err(e) => {
                self.set_status(format!("add to playlist: {e}"));
                return;
            }
        };
        self.playlist_picker = Some(PlaylistPicker {
            track_uri: uri,
            entries: playlists,
            sel: 0,
        });
        self.dirty = true;
    }

    /// Replace the auto-queue with ~30 tracks similar to the hovered track
    /// and start playing the first one. Manual-prefix (items added via `a`)
    /// survives — the radio behaves like clicking a track in a playlist:
    /// the soft suffix swaps, the pinned queue doesn't.
    async fn queue_song_radio(&mut self) {
        let Some(uri) = self.hovered_uri() else {
            self.set_status("song radio: nothing under cursor");
            return;
        };
        if !uri.starts_with("spotify:track:") {
            self.set_status("song radio: only works on Spotify tracks");
            return;
        }
        let Some(src) = self.dispatcher.get("spotify").cloned() else {
            self.set_status("song radio: Spotify source not enabled");
            return;
        };
        let path = format!("spotify:radio:track:{}", uri.trim_start_matches("spotify:track:"));
        let entries = match src.browse(&path).await {
            Ok(v) => v,
            Err(e) => {
                self.set_status(format!("song radio: {e}"));
                return;
            }
        };
        let tracks: Vec<QueuedItem> = entries
            .into_iter()
            .filter(|e| matches!(e.kind, EntryKind::Track))
            .map(|e| QueuedItem {
                source_scheme: "spotify",
                uri: e.uri,
                display: e.display.unwrap_or_default(),
            })
            .collect();
        if tracks.is_empty() {
            self.set_status("song radio: no tracks returned");
            return;
        }
        let count = tracks.len();
        self.queue.replace_auto(tracks, 0);
        let Some(qi) = self.queue.current().cloned() else {
            self.set_status("song radio: queue empty after replace");
            return;
        };
        let vol = self.master_volume;
        if let Err(e) = self.dispatcher.play(&qi, vol).await {
            self.set_status(format!("song radio: play: {e}"));
            return;
        }
        self.set_status(format!("song radio: {count} tracks"));
        self.refresh_now_playing().await;
    }

    /// Navigate to the artist or album of the hovered track. Asks the
    /// Spotify source to resolve the related URI, then pushes a browse
    /// view onto the Spotify category stack.
    async fn navigate_to_relation(&mut self, kind: &str) {
        let Some(uri) = self.hovered_uri() else {
            self.set_status(format!("go to {kind}: no row"));
            return;
        };
        if !uri.starts_with("spotify:track:") {
            self.set_status(format!("go to {kind}: not a Spotify track"));
            return;
        }
        let Some(src) = self.dispatcher.get("spotify").cloned() else {
            self.set_status("go to: Spotify source not enabled");
            return;
        };
        let rel_uri = match src.relation_uri(&uri, kind).await {
            Ok(u) => u,
            Err(e) => {
                self.set_status(format!("go to {kind}: {e}"));
                return;
            }
        };
        // For artist, we want the artist landing page (top tracks). Use
        // the existing artist subview path so the browse arm fires.
        let path = if kind == "artist" {
            let id = rel_uri.trim_start_matches("spotify:artist:");
            format!("spotify:artistview:{id}:top")
        } else {
            rel_uri.clone()
        };
        let entries = match src.browse(&path).await {
            Ok(e) => e,
            Err(e) => {
                self.set_status(format!("go to {kind}: {e}"));
                return;
            }
        };
        let origin_tab_idx = self.active_tab_idx;
        let switched_tabs = if let Some(idx) = self
            .tabs
            .iter()
            .position(|c| *c == crate::types::Category::Spotify)
        {
            let crossed = idx != self.active_tab_idx;
            self.active_tab_idx = idx;
            crossed
        } else {
            false
        };
        let s = self
            .category_states
            .entry(crate::types::Category::Spotify)
            .or_default();
        s.parent_cursors.push((s.cursor, s.top));
        s.origin_tabs
            .push(if switched_tabs { Some(origin_tab_idx) } else { None });
        s.stack.push(LibraryView::Entries {
            scheme: "spotify",
            label: format!("{kind}: {rel_uri}"),
            entries,
        });
        s.cursor = 0;
        s.top = 0;
        self.set_status(format!("go to {kind}"));
        self.dirty = true;
    }

    /// Open the expanded-art overlay on the row under the cursor. Picks
    /// the largest art URL the row exposes (`art_uri_full` first, else
    /// `art_uri`). Spawns a fetch so Spotify's high-res CDN URL lands in
    /// the cache even when only the small thumb was previously fetched.
    async fn expand_hovered_art(&mut self) {
        let (uri, scheme) = match self.hovered_art_uri() {
            Some(p) => p,
            None => {
                self.set_status("expand: no art on this row");
                return;
            }
        };
        // Prefetch full-size art if not cached. ArtCache keys by URI so the
        // small/large variants live in separate cache entries — fetching the
        // full URI here doesn't disturb the small thumb cache.
        if self.art_cache.peek(&uri).is_none() {
            self.kick_art_fetch(uri.clone(), scheme);
        }
        self.expanded_art_uri = Some(uri);
        self.dirty = true;
    }

    /// Look up the cursor's display, returning (art_uri_full || art_uri,
    /// source_scheme). Handles browse views (Entries/Tracks/Sections) and
    /// the Queue tab.
    fn hovered_art_uri(&self) -> Option<(String, &'static str)> {
        match self.active_category() {
            Category::Queue => {
                let q = self.queue.items().get(self.queue_cursor)?;
                let uri = q
                    .display
                    .art_uri_full
                    .clone()
                    .or_else(|| q.display.art_uri.clone())?;
                Some((uri, q.source_scheme))
            }
            Category::Search => {
                let mut idx = self.search_cursor;
                for group in &self.search_results {
                    if idx < group.items.len() {
                        let item = group.items.get(idx)?;
                        let uri = item
                            .display
                            .art_uri_full
                            .clone()
                            .or_else(|| item.display.art_uri.clone())?;
                        return Some((uri, group.scheme));
                    }
                    idx -= group.items.len();
                }
                None
            }
            _ => {
                let cat = self.active_category();
                let s = self.category_states.get(&cat)?;
                let cur = self.filtered_browse_cursor_to_orig(s.cursor).unwrap_or(s.cursor);
                match s.stack.last()? {
                    LibraryView::Entries { scheme, entries, .. } => {
                        let e = entries.get(cur)?;
                        let d = e.display.as_ref()?;
                        let uri = d.art_uri_full.clone().or_else(|| d.art_uri.clone())?;
                        Some((uri, *scheme))
                    }
                    LibraryView::Tracks { items, .. } => {
                        let it = items.get(cur)?;
                        let uri = it.display.art_uri_full.clone().or_else(|| it.display.art_uri.clone())?;
                        Some((uri, "local"))
                    }
                    LibraryView::Sections { sections, .. } => {
                        let hit = sections_row_at(sections, cur)?;
                        if let SectionHit::Entry { scheme, entry } = hit {
                            let d = entry.display.as_ref()?;
                            let uri = d.art_uri_full.clone().or_else(|| d.art_uri.clone())?;
                            Some((uri, scheme))
                        } else {
                            None
                        }
                    }
                }
            }
        }
    }

    /// Spawn an art fetch for `uri` against `scheme`'s source. Mirrors the
    /// thumb-list fetch path so the overlay benefits from the same
    /// concurrency cap + disk persistence.
    fn kick_art_fetch(&mut self, uri: String, scheme: &'static str) {
        if self.fetching.contains(&uri) {
            return;
        }
        let Some(src) = self.dispatcher.get(scheme).cloned() else {
            return;
        };
        self.fetching.insert(uri.clone());
        let cache = self.art_cache.clone();
        let wake = self.wake_tx.clone();
        let key = uri.clone();
        tokio::spawn(async move {
            let _ = cache
                .get(&key, || async { src.art(&key, crate::types::ArtSize::Full).await })
                .await;
            let _ = wake.send(());
        });
    }

    /// Returns true if a volume step should fire now; false if the previous
    /// step was less than 120ms ago (OS autorepeat held key). Updates the
    /// timestamp on a successful fire.
    fn volume_debounce_fired(&mut self) -> bool {
        let now = Instant::now();
        let fire = match self.last_volume_at {
            Some(prev) => now.duration_since(prev) >= Duration::from_millis(120),
            None => true,
        };
        if fire {
            self.last_volume_at = Some(now);
        }
        fire
    }

    pub async fn push_volume(&mut self) {
        if let Some(scheme) = self.dispatcher.active_scheme() {
            if let Some(src) = self.dispatcher.get(scheme) {
                let _ = src.set_volume(self.master_volume).await;
            }
        }
        self.dirty = true;
    }

    /// Periodic tick: drop stale leader, refresh playback status from the
    /// active source.
    pub async fn on_tick(&mut self) {
        if let Some(deadline) = self.leader_deadline {
            if Instant::now() > deadline {
                self.leader = None;
                self.leader_deadline = None;
                self.dirty = true;
            }
        }
        // Auto-clear status toast after ~3s so transient feedback doesn't
        // linger in the top-left after the user moves on. Suppress the
        // clear while a download is in progress so the "downloading"
        // toast stays pinned until the task finishes.
        let dl_active = self
            .download_progress
            .load(std::sync::atomic::Ordering::Relaxed)
            <= 100;
        if let Some(set_at) = self.status_set_at {
            if set_at.elapsed() >= Duration::from_secs(3) && !dl_active {
                self.status = None;
                self.status_set_at = None;
                self.dirty = true;
            }
        }
        self.tick_counter = self.tick_counter.wrapping_add(1);
        let playing = matches!(
            self.playback.as_ref().map(|p| p.state),
            Some(PlayState::Playing)
        );
        if !playing && !self.tick_counter.is_multiple_of(4) {
            return;
        }
        let Some(scheme) = self.dispatcher.active_scheme() else {
            return;
        };
        let Some(src) = self.dispatcher.get(scheme).cloned() else {
            return;
        };
        if let Ok(Some(s)) = src.playback_status().await {
            let changed = self
                .playback
                .as_ref()
                .map(|p| p.elapsed != s.elapsed || p.state != s.state || p.volume != s.volume)
                .unwrap_or(true);
            // Sync master_volume from the source so external changes (Spotify
            // Connect from phone, MPRIS, MPD client, etc.) reflect in the UI
            // without the user having to touch +/-.
            if s.volume != self.master_volume {
                self.master_volume = s.volume;
            }
            self.playback = Some(s);
            if changed {
                self.dirty = true;
            }
        }
    }

    /// Translate a key event through the keymap, with leader-buffer + input-mode
    /// support. Returns `Action::None` when the key was consumed (e.g. typing
    /// into the search box). Returns the appropriate Action otherwise.
    pub fn key_to_action(&mut self, ev: crossterm::event::KeyEvent) -> Action {
        // Device picker modal: j/k navigate, Enter transfers, Esc/q closes.
        if self.device_modal_open {
            use crossterm::event::KeyCode;
            self.dirty = true;
            match ev.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.device_modal_open = false;
                    return Action::None;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if !self.devices.is_empty() {
                        self.device_modal_sel =
                            (self.device_modal_sel + 1).min(self.devices.len() - 1);
                    }
                    return Action::None;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.device_modal_sel = self.device_modal_sel.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Enter => return Action::TransferToSelectedDevice,
                _ => return Action::None,
            }
        }
        // Sort modal: j/k navigate, Enter applies, Esc/q closes.
        if self.sort_modal_open {
            use crossterm::event::KeyCode;
            self.dirty = true;
            let len = SortAxis::all().len();
            match ev.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    self.sort_modal_open = false;
                    return Action::None;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.sort_modal_sel = (self.sort_modal_sel + 1).min(len - 1);
                    return Action::None;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.sort_modal_sel = self.sort_modal_sel.saturating_sub(1);
                    return Action::None;
                }
                KeyCode::Enter => return Action::ApplySelectedSort,
                _ => return Action::None,
            }
        }
        // Help overlay: vim-style scroll (j/k/C-d/C-u/g/G), Esc/q/? closes.
        if self.help_visible {
            use crossterm::event::{KeyCode, KeyModifiers};
            match (ev.code, ev.modifiers) {
                (KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?'), _) => {
                    self.help_visible = false;
                    self.help_scroll = 0;
                    self.dirty = true;
                }
                (KeyCode::Char('j') | KeyCode::Down, _) => {
                    self.help_scroll = self.help_scroll.saturating_add(1);
                    self.dirty = true;
                }
                (KeyCode::Char('k') | KeyCode::Up, _) => {
                    self.help_scroll = self.help_scroll.saturating_sub(1);
                    self.dirty = true;
                }
                (KeyCode::Char('d'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.help_scroll = self.help_scroll.saturating_add(10);
                    self.dirty = true;
                }
                (KeyCode::Char('u'), m) if m.contains(KeyModifiers::CONTROL) => {
                    self.help_scroll = self.help_scroll.saturating_sub(10);
                    self.dirty = true;
                }
                (KeyCode::Char('g'), _) => {
                    self.help_scroll = 0;
                    self.dirty = true;
                }
                (KeyCode::Char('G'), _) => {
                    self.help_scroll = u16::MAX;
                    self.dirty = true;
                }
                _ => {}
            }
            return Action::None;
        }
        // In-view filter (`/`) takes precedence over normal key bindings.
        // While input is focused, characters extend the live filter pattern;
        // Enter commits, Esc cancels and clears any committed filter for
        // the active tab so the user is never stuck with a hidden filter.
        if self.filter_input.is_some() {
            use crossterm::event::KeyCode;
            self.dirty = true;
            match ev.code {
                KeyCode::Esc => {
                    self.filter_input = None;
                    let cat = self.active_category();
                    self.filter_active.remove(&cat);
                    self.clamp_cursor_to_filter();
                    return Action::None;
                }
                KeyCode::Enter => {
                    let buf = self.filter_input.take().unwrap_or_default();
                    let cat = self.active_category();
                    if buf.is_empty() {
                        self.filter_active.remove(&cat);
                    } else {
                        self.filter_active.insert(cat, buf);
                    }
                    self.clamp_cursor_to_filter();
                    return Action::None;
                }
                KeyCode::Backspace => {
                    if let Some(buf) = self.filter_input.as_mut() {
                        buf.pop();
                    }
                    let buf = self.filter_input.clone().unwrap_or_default();
                    let cat = self.active_category();
                    if buf.is_empty() {
                        self.filter_active.remove(&cat);
                    } else {
                        self.filter_active.insert(cat, buf);
                    }
                    self.clamp_cursor_to_filter();
                    return Action::None;
                }
                KeyCode::Char(c) => {
                    if let Some(buf) = self.filter_input.as_mut() {
                        buf.push(c);
                    }
                    let buf = self.filter_input.clone().unwrap_or_default();
                    let cat = self.active_category();
                    self.filter_active.insert(cat, buf);
                    self.clamp_cursor_to_filter();
                    return Action::None;
                }
                _ => return Action::None,
            }
        }
        // Command bar takes precedence over everything else.
        if self.command_input_focused {
            use crossterm::event::KeyCode;
            self.dirty = true;
            match ev.code {
                KeyCode::Esc => {
                    self.command_input_focused = false;
                    self.command_buffer.clear();
                    return Action::None;
                }
                KeyCode::Enter => return Action::Activate,
                KeyCode::Backspace => {
                    self.command_buffer.pop();
                    return Action::None;
                }
                KeyCode::Char(c) => {
                    self.command_buffer.push(c);
                    return Action::None;
                }
                _ => return Action::None,
            }
        }
        // Search input mode: only when /-focused, intercepts text.
        if self.search_input_focused {
            use crossterm::event::KeyCode;
            self.dirty = true;
            match ev.code {
                KeyCode::Esc => {
                    self.search_input_focused = false;
                    return Action::None;
                }
                KeyCode::Enter => return Action::Activate,
                KeyCode::Backspace => {
                    self.search_query.pop();
                    return Action::None;
                }
                KeyCode::Char(c) => {
                    self.search_query.push(c);
                    return Action::None;
                }
                _ => return Action::None,
            }
        }
        let chord = KeyChord::from_event(ev);
        if let Some(active) = self.leader.take() {
            self.leader_deadline = None;
            self.dirty = true;
            return active.lookup(chord).cloned().unwrap_or(Action::None);
        }
        if let Some(leader) = self.keymap.leader(chord).cloned() {
            self.leader = Some(leader);
            self.leader_deadline = Some(Instant::now() + Duration::from_millis(1500));
            self.dirty = true;
            return Action::None;
        }
        self.keymap.lookup(chord).cloned().unwrap_or(Action::None)
    }

    async fn refresh_now_playing(&mut self) {
        let Some(cur) = self.queue.current().cloned() else {
            self.now_playing_protocol = None;
            self.now_playing_uri = None;
            self.now_playing_aspect = None;
            self.current_liked = None;
            return;
        };
        if self.now_playing_uri.as_deref() == Some(&cur.uri) {
            return;
        }
        // Don't auto-reset art_collapsed on track change — preference now
        // persists across runs, and users on small terminals want it to
        // stay collapsed.
        self.now_playing_uri = Some(cur.uri.clone());
        // Probe liked status in the background — Spotify has the only real impl.
        self.refresh_liked().await;
        crate::hooks::on_track_change(&self.hooks, &cur);
        let new_scheme = cur.source_scheme;
        if self.last_active_scheme != Some(new_scheme) {
            crate::hooks::on_source_switch(&self.hooks, self.last_active_scheme, new_scheme);
            self.last_active_scheme = Some(new_scheme);
        }

        // Prefer the largest CDN URL when the source supplied one (Spotify
        // populates this on tracks); fall back to small thumb URL, then the
        // library URI as a last resort.
        let art_uri = cur
            .display
            .art_uri_full
            .clone()
            .or_else(|| cur.display.art_uri.clone())
            .unwrap_or_else(|| cur.uri.clone());

        // Route via the *source's* scheme rather than parsing the URI:
        // `art_uri_full` for Spotify is an `https://i.scdn.co/...` URL, which
        // would split to scheme "https" and miss every registered source.
        // Mirrors the inline-thumb path in `widgets/thumb_list.rs`.
        let img = {
            let cache = &self.art_cache;
            let scheme = cur.source_scheme;
            let src_opt = self.dispatcher.get(scheme).cloned();
            let key = art_uri.clone();
            cache
                .get(&key, || async {
                    let src = src_opt
                        .ok_or_else(|| anyhow::anyhow!("no source for scheme: {scheme}"))?;
                    src.art(&key, ArtSize::Full).await
                })
                .await
        };

        match img {
            Ok(arc) => {
                use image::GenericImageView;
                let (w, h) = (*arc).dimensions();
                self.now_playing_aspect = Some((w, h));
                let proto = self.term.picker.new_resize_protocol((*arc).clone());
                self.now_playing_protocol = Some(proto);
            }
            Err(e) => {
                tracing::debug!("now-playing art unavailable: {e}");
                self.now_playing_protocol = None;
                self.now_playing_aspect = None;
            }
        }
        self.dirty = true;
    }

    async fn handle_spotify_event(&mut self, ev: crate::source::spotify::SpotifyEvent) {
        use crate::source::spotify::SpotifyEvent;
        match ev {
            SpotifyEvent::EndOfTrack => {
                let vol = self.master_volume;
                let shuf = self.shuffle;
                let rep = self.repeat;
                if let Err(e) = self
                    .dispatcher
                    .advance_with(&mut self.queue, shuf, rep, vol)
                    .await
                {
                    self.set_status(format!("queue advance error: {e}"));
                } else {
                    self.refresh_now_playing().await;
                }
            }
            SpotifyEvent::Playing => self.set_status("spotify: playing"),
            SpotifyEvent::Paused => self.set_status("spotify: paused"),
            SpotifyEvent::Stopped => {}
            SpotifyEvent::Loading => self.set_status("spotify: loading"),
            SpotifyEvent::Error(s) => self.set_status(format!("spotify error: {s}")),
        }
        self.dirty = true;
    }

    /// Execute one IPC line and return a one-line reply for the client.
    pub async fn handle_ipc(&mut self, line: &str) -> String {
        let mut parts = line.split_whitespace();
        let cmd = parts.next().unwrap_or("");
        let rest: Vec<&str> = parts.collect();
        match cmd {
            "play" => {
                let uri = rest.join(" ");
                if uri.is_empty() {
                    return "err: play needs a uri".into();
                }
                match self.cmd_play(&uri).await {
                    Ok(_) => format!("ok: playing {uri}"),
                    Err(e) => format!("err: {e}"),
                }
            }
            "next" => {
                let vol = self.master_volume;
                match self.dispatcher.advance(&mut self.queue, vol).await {
                    Ok(_) => {
                        self.refresh_now_playing().await;
                        "ok".into()
                    }
                    Err(e) => format!("err: {e}"),
                }
            }
            "prev" => {
                let vol = self.master_volume;
                match self.dispatcher.previous(&mut self.queue, vol).await {
                    Ok(_) => {
                        self.refresh_now_playing().await;
                        "ok".into()
                    }
                    Err(e) => format!("err: {e}"),
                }
            }
            "pause" => match self.toggle_pause().await {
                Ok(_) => "ok".into(),
                Err(e) => format!("err: {e}"),
            },
            "stop" => match self.dispatcher.stop().await {
                Ok(_) => "ok".into(),
                Err(e) => format!("err: {e}"),
            },
            "vol" => match rest.first().and_then(|s| s.parse::<u8>().ok()) {
                Some(v) => {
                    self.master_volume = v.min(100);
                    self.push_volume().await;
                    format!("ok: vol {}", self.master_volume)
                }
                None => "err: vol needs 0..100".into(),
            },
            "status" => {
                let cur = self.queue.current();
                let title = cur.map(|c| c.display.title.clone()).unwrap_or_default();
                let artist = cur
                    .and_then(|c| c.display.artist.clone())
                    .unwrap_or_default();
                let scheme = cur.map(|c| c.source_scheme.to_string()).unwrap_or_default();
                let (el, dur) = self
                    .playback
                    .as_ref()
                    .map(|p| (p.elapsed.as_secs(), p.duration.map(|d| d.as_secs())))
                    .unwrap_or((0, None));
                let dur_s = match dur {
                    Some(d) => format!("{}:{:02}", d / 60, d % 60),
                    None => "?".into(),
                };
                format!(
                    "{title} | {artist} | {}:{:02}/{dur_s} | {scheme}",
                    el / 60,
                    el % 60
                )
            }
            other => format!("err: unknown cmd '{other}'"),
        }
    }

    /// Resolve a click/scroll into an Action. Wheel = up/down. Click on the
    /// tab bar selects that tab. Click in the body sets the cursor to the
    /// hit row and activates it (single-click play, like rmpc).
    pub fn handle_mouse(&mut self, ev: MouseEvent) -> Action {
        let (x, y) = (ev.column, ev.row);
        // Expanded-art overlay locks the mouse plane. Any mouse event closes
        // the overlay; we don't pass anything through to underlying widgets.
        if self.expanded_art_uri.is_some() {
            self.expanded_art_uri = None;
            self.dirty = true;
            return Action::None;
        }
        match ev.kind {
            MouseEventKind::ScrollDown => {
                if let Some(r) = self.volume_rect {
                    if rect_contains(&r, x, y) {
                        return Action::VolumeDown;
                    }
                }
                Action::Down
            }
            MouseEventKind::ScrollUp => {
                if let Some(r) = self.volume_rect {
                    if rect_contains(&r, x, y) {
                        return Action::VolumeUp;
                    }
                }
                Action::Up
            }
            MouseEventKind::Down(MouseButton::Right) => {
                if let Some(bar) = self.progress_bar_rect {
                    if rect_contains(&bar, x, y) {
                        return Action::PlayPause;
                    }
                }
                if let Some(r) = self.now_playing_text_rect {
                    if rect_contains(&r, x, y) {
                        return Action::NextTrack;
                    }
                }
                Action::None
            }
            MouseEventKind::Down(MouseButton::Middle) => {
                if let Some(r) = self.now_playing_text_rect {
                    if rect_contains(&r, x, y) {
                        return Action::PlayPause;
                    }
                }
                Action::None
            }
            MouseEventKind::Down(MouseButton::Left) => {
                // Expanded-art overlay active: any click closes. Mouse
                // handler short-circuits above this match arm too, but
                // keep the explicit close here for safety.
                if self.expanded_art_uri.is_some() {
                    self.expanded_art_uri = None;
                    self.dirty = true;
                    return Action::None;
                }
                // Click on an inline row thumbnail no longer opens the
                // overlay (user feedback: too click-happy). Use `V` to
                // open the art for the hovered row instead. Click still
                // moves the cursor to that row via the body-hit path
                // below.
                // Art panel: left-click toggles collapsed mode (art shrinks
                // to bottom-bar height instead of protruding into the body).
                // Click still swallowed so it never falls through to a body
                // row underneath. New state persisted to state.json so it
                // survives restarts.
                if let Some(art) = self.art_panel_rect {
                    if rect_contains(&art, x, y) {
                        self.art_collapsed = !self.art_collapsed;
                        self.persist_state();
                        self.dirty = true;
                        return Action::None;
                    }
                }
                // Progress bar? Click anywhere on the bar to seek there.
                if let Some(bar) = self.progress_bar_rect {
                    if rect_contains(&bar, x, y) && bar.width > 0 {
                        let offset = x.saturating_sub(bar.x) as u32;
                        let permille =
                            ((offset * 1000) / bar.width as u32).min(1000) as u16;
                        return Action::SeekToPermille(permille);
                    }
                }
                // Now-playing text rows: left click = previous track.
                // Volume rect was matched first above for scroll; check it
                // again so left-clicks on the volume strip don't
                // accidentally trigger a previous-track jump.
                if let Some(r) = self.now_playing_text_rect {
                    if rect_contains(&r, x, y)
                        && !self
                            .volume_rect
                            .map(|v| rect_contains(&v, x, y))
                            .unwrap_or(false)
                    {
                        return Action::PrevTrack;
                    }
                }
                // Tab bar?
                for (rect, idx) in &self.tab_rects {
                    if rect_contains(rect, x, y) {
                        if *idx < u8::MAX as usize {
                            return Action::TabByIndex(*idx as u8);
                        }
                        return Action::None;
                    }
                }
                // Body? Walk per-row heights (rows can be 1 or `thumb_cells`
                // tall now) instead of dividing by a single row_h.
                if let Some(body) = self.body_rect {
                    if rect_contains(&body, x, y) && !self.body_row_heights.is_empty() {
                        let mut acc: u16 = 0;
                        let rel_y = y.saturating_sub(body.y);
                        let mut visible_row: Option<usize> = None;
                        for (i, h) in self.body_row_heights.iter().enumerate() {
                            if rel_y < acc + *h {
                                visible_row = Some(i);
                                break;
                            }
                            acc += *h;
                        }
                        if let Some(vr) = visible_row {
                            let target = self.body_top_at_render + vr;
                            let (_, len) = self.cursor_for_tab();
                            if target < len {
                                self.set_cursor(target);
                                return Action::Activate;
                            }
                        }
                    }
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    async fn handle_mpd_event(&mut self, ev: ConnectionEvent) {
        match ev {
            ConnectionEvent::SubsystemChange(Subsystem::Player) => {
                if let Ok(Some(cur)) = self.local.current_song().await {
                    self.set_status(format!("MPD: {}", cur.display.title));
                }
            }
            ConnectionEvent::SubsystemChange(_) => {}
            ConnectionEvent::ConnectionClosed(e) => {
                self.set_status(format!("MPD connection closed: {e:?}"));
            }
        }
        self.dirty = true;
    }
}

fn rect_contains(r: &Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x.saturating_add(r.width) && y >= r.y && y < r.y.saturating_add(r.height)
}

/// Compress an anyhow error to a single short user-facing line. Keeps
/// only the top-level message (drops the cause chain `e:#` would show)
/// and strips noisy substrings — URL bodies in particular blow out the
/// toast for what's usually a one-word "rate-limited" or "403" problem.
fn short_err(e: &anyhow::Error) -> String {
    let mut s = e.to_string();
    // Drop everything past the first colon-followed-by-URL. The pattern
    // we see most often is "GET https://...: <status>" — keep <status>,
    // drop the URL.
    if let Some(idx) = s.find("https://") {
        // Find next ": " after the URL to keep the status code.
        let tail = &s[idx..];
        if let Some(end) = tail.find(": ") {
            s = format!("{}{}", &s[..idx], &tail[end + 2..]);
        } else {
            s.truncate(idx);
        }
    }
    // Same for http://
    if let Some(idx) = s.find("http://") {
        let tail = &s[idx..];
        if let Some(end) = tail.find(": ") {
            s = format!("{}{}", &s[..idx], &tail[end + 2..]);
        } else {
            s.truncate(idx);
        }
    }
    s.trim().to_string()
}

enum LibraryActivate {
    DescendEntry {
        scheme: &'static str,
        uri: String,
        label: String,
    },
    ExpandAlbum {
        label: String,
    },
    /// Append more entries to the current view (paged "load more" sentinel).
    ExtendCurrent {
        scheme: &'static str,
        uri: String,
    },
    PlayEntry { entry: Entry },
    PlayItem { item: Item },
}

/// Result of resolving a flattened row index inside a `LibraryView::Sections`.
pub enum SectionHit<'a> {
    /// Cursor sits on a section header. Activation no-ops.
    Header,
    Entry {
        scheme: &'static str,
        entry: &'a Entry,
    },
}

/// Reorder the given view in place. Year / RecentlyAdded fall back to
/// alpha-asc since fuga doesn't yet carry those fields on `ItemDisplay`.
/// Convert a track-typed `Entry` into a `QueuedItem`. Falls back to the
/// label when no `ItemDisplay` is attached.
fn entry_to_queued(scheme: &'static str, e: &Entry) -> QueuedItem {
    QueuedItem {
        source_scheme: scheme,
        uri: e.uri.clone(),
        display: e.display.clone().unwrap_or(crate::types::ItemDisplay {
            title: e.label.clone(),
            artist: None,
            album: None,
            art_uri: None,
            art_uri_full: None,
            duration: None,
            sort_hint: None,
            track_no: None,
        }),
    }
}

/// Fisher-Yates shuffle using wall-clock nanos as a cheap seed source. Same
/// approach as `Queue::advance_with` — avoids pulling `rand` for one call.
fn shuffle_in_place(v: &mut [QueuedItem]) {
    if v.len() < 2 {
        return;
    }
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0xDEAD_BEEF)
        | 1;
    for i in (1..v.len()).rev() {
        // xorshift64
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed as usize) % (i + 1);
        v.swap(i, j);
    }
}

/// Float every entry whose URI is in `pinned` to the top of the view,
/// preserving relative order both inside the pinned partition and inside
/// the unpinned tail. Stable, O(n).
fn apply_pinning(view: &mut LibraryView, pinned: &std::collections::HashSet<String>) {
    if pinned.is_empty() {
        return;
    }
    match view {
        LibraryView::Entries { entries, .. } => {
            let mut pin: Vec<Entry> = Vec::new();
            let mut rest: Vec<Entry> = Vec::new();
            for e in entries.drain(..) {
                if pinned.contains(&e.uri) {
                    pin.push(e);
                } else {
                    rest.push(e);
                }
            }
            entries.extend(pin);
            entries.extend(rest);
        }
        LibraryView::Tracks { items, .. } => {
            let mut pin: Vec<Item> = Vec::new();
            let mut rest: Vec<Item> = Vec::new();
            for it in items.drain(..) {
                if pinned.contains(&it.uri) {
                    pin.push(it);
                } else {
                    rest.push(it);
                }
            }
            items.extend(pin);
            items.extend(rest);
        }
        LibraryView::Sections { sections, .. } => {
            for sec in sections {
                let mut pin: Vec<Entry> = Vec::new();
                let mut rest: Vec<Entry> = Vec::new();
                for e in sec.entries.drain(..) {
                    if pinned.contains(&e.uri) {
                        pin.push(e);
                    } else {
                        rest.push(e);
                    }
                }
                sec.entries.extend(pin);
                sec.entries.extend(rest);
            }
        }
    }
}

/// Default sort axis for a browse category's root view. `None` keeps the
/// source-returned order (rare — most tabs benefit from an explicit axis).
fn default_sort_for(cat: Category) -> Option<SortAxis> {
    use Category::*;
    match cat {
        // Filesystem layout + saved/added order. Source-returned order is
        // already useful (MPD: alpha by path; Spotify: recently-added).
        Directories | Playlists | Podcasts => Some(SortAxis::RecentlyAdded),
        // Album list = alpha by title.
        Albums | Artists => Some(SortAxis::AlphaAsc),
        _ => None,
    }
}

fn sort_library_view(view: &mut LibraryView, axis: SortAxis) {
    fn entry_key_alpha(e: &Entry) -> String {
        e.label.to_lowercase()
    }
    fn entry_key_dur(e: &Entry) -> u64 {
        e.display
            .as_ref()
            .and_then(|d| d.duration)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX)
    }
    fn item_key_dur(it: &Item) -> u64 {
        it.display
            .duration
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX)
    }
    // Newest first: sort by `-sort_hint` so larger timestamps land at top.
    // Rows without a hint group together and preserve source order via
    // stable sort (empty tie-break string means equal keys).
    fn entry_key_recent(e: &Entry) -> (i64, &'static str) {
        match e.display.as_ref().and_then(|d| d.sort_hint) {
            Some(t) => (-t, ""),
            None => (i64::MAX, ""),
        }
    }
    fn item_key_recent(it: &Item) -> (i64, &'static str) {
        match it.display.sort_hint {
            Some(t) => (-t, ""),
            None => (i64::MAX, ""),
        }
    }
    fn entry_key_track(e: &Entry) -> (u32, String) {
        let no = e
            .display
            .as_ref()
            .and_then(|d| d.track_no)
            .unwrap_or(u32::MAX);
        (no, entry_key_alpha(e))
    }
    fn item_key_track(it: &Item) -> (u32, String) {
        let no = it.display.track_no.unwrap_or(u32::MAX);
        (no, it.display.title.to_lowercase())
    }
    let alpha_asc = matches!(axis, SortAxis::AlphaAsc | SortAxis::Year);
    let alpha_desc = matches!(axis, SortAxis::AlphaDesc);
    let by_dur = matches!(axis, SortAxis::Duration);
    let by_recent = matches!(axis, SortAxis::RecentlyAdded);
    let by_track = matches!(axis, SortAxis::TrackNumber);
    match view {
        LibraryView::Entries { entries, .. } => {
            if alpha_asc {
                entries.sort_by_cached_key(entry_key_alpha);
            } else if alpha_desc {
                entries.sort_by_cached_key(|e| std::cmp::Reverse(entry_key_alpha(e)));
            } else if by_dur {
                entries.sort_by_key(entry_key_dur);
            } else if by_recent {
                entries.sort_by_cached_key(entry_key_recent);
            } else if by_track {
                entries.sort_by_cached_key(entry_key_track);
            }
        }
        LibraryView::Tracks { items, .. } => {
            if alpha_asc {
                items.sort_by_cached_key(|it| it.display.title.to_lowercase());
            } else if alpha_desc {
                items.sort_by_cached_key(|it| std::cmp::Reverse(it.display.title.to_lowercase()));
            } else if by_dur {
                items.sort_by_key(item_key_dur);
            } else if by_recent {
                items.sort_by_cached_key(item_key_recent);
            } else if by_track {
                items.sort_by_cached_key(item_key_track);
            }
        }
        LibraryView::Sections { sections, .. } => {
            for sec in sections {
                if alpha_asc {
                    sec.entries.sort_by_cached_key(entry_key_alpha);
                } else if alpha_desc {
                    sec.entries
                        .sort_by_cached_key(|e| std::cmp::Reverse(entry_key_alpha(e)));
                } else if by_dur {
                    sec.entries.sort_by_key(entry_key_dur);
                } else if by_recent {
                    sec.entries.sort_by_cached_key(entry_key_recent);
                } else if by_track {
                    sec.entries.sort_by_cached_key(entry_key_track);
                }
            }
        }
    }
}

/// Map a flattened row index into a section + entry (or a header). Layout:
/// header, entry…, header, entry…  — one row per header, one per entry.
pub fn sections_row_at<'a>(sections: &'a [Section], idx: usize) -> Option<SectionHit<'a>> {
    let mut remaining = idx;
    for sec in sections {
        if remaining == 0 {
            return Some(SectionHit::Header);
        }
        remaining -= 1;
        if remaining < sec.entries.len() {
            return Some(SectionHit::Entry {
                scheme: sec.scheme,
                entry: &sec.entries[remaining],
            });
        }
        remaining -= sec.entries.len();
    }
    None
}

pub async fn run(
    _config: Config,
    mpd_events: ConnectionEvents,
    mut app: App,
    wake_rx: UnboundedReceiver<()>,
    row_batch_rx: UnboundedReceiver<RowBatch>,
    spotify_events: UnboundedReceiver<crate::source::spotify::SpotifyEvent>,
    mpris: Option<crate::mpris::MprisHandles>,
) -> Result<()> {
    // Spawn the unix-socket control plane so `fuga play <uri>` etc. work
    // against this running instance. Listener task lives until process exit.
    let (ipc_tx, ipc_rx) = mpsc::unbounded_channel::<crate::ipc::IpcRequest>();
    tokio::spawn(async move {
        if let Err(e) = crate::ipc::serve(ipc_tx).await {
            tracing::warn!("ipc server: {e}");
        }
    });

    let (mpris_event_rx, mpris_cmd_tx) = match mpris {
        Some(h) => (Some(h.event_rx), Some(h.command_tx)),
        None => (None, None),
    };
    app.mpris_cmd_tx = mpris_cmd_tx;

    // Populate the initial active tab BEFORE entering the TUI loop. Without
    // this, the first render shows the body "loading Directories…"
    // placeholder forever (no tab switch fires `ensure_active_loaded`).
    app.ensure_active_loaded().await;

    crossterm::terminal::enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    crossterm::execute!(
        stdout,
        crossterm::terminal::EnterAlternateScreen,
        crossterm::event::EnableMouseCapture
    )
    .context("enter alt screen")?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).context("init terminal")?;

    let res = run_loop(
        &mut terminal,
        &mut app,
        mpd_events,
        wake_rx,
        row_batch_rx,
        spotify_events,
        ipc_rx,
        mpris_event_rx,
    )
    .await;

    crossterm::terminal::disable_raw_mode().ok();
    crossterm::execute!(
        io::stdout(),
        crossterm::event::DisableMouseCapture,
        crossterm::terminal::LeaveAlternateScreen
    )
    .ok();
    terminal.show_cursor().ok();

    res
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    mut mpd_events: ConnectionEvents,
    mut wake_rx: UnboundedReceiver<()>,
    mut row_batch_rx: UnboundedReceiver<RowBatch>,
    mut spotify_events: UnboundedReceiver<crate::source::spotify::SpotifyEvent>,
    mut ipc_rx: UnboundedReceiver<crate::ipc::IpcRequest>,
    mut mpris_events: Option<UnboundedReceiver<crate::mpris::MprisEvent>>,
) -> Result<()> {
    let mut term_events = EventStream::new();
    let mut tick = tick_interval();

    loop {
        if app.dirty {
            terminal.draw(|f| ui::render(app, f)).context("draw")?;
            app.dirty = false;
        }

        tokio::select! {
            biased;
            _ = app.shutdown.cancelled() => break,
            ev = term_events.next() => match ev {
                Some(Ok(Event::Key(k))) => {
                    // Drop auto-repeat key events: holding +/- or H/L should
                    // act per-press, not blast actions at the OS repeat rate.
                    // Press + Release pass; Repeat ignored.
                    if k.kind == KeyEventKind::Repeat {
                        continue;
                    }
                    let action = app.key_to_action(k);
                    // Add-to-playlist picker: j/k navigate, Enter commits, Esc cancels.
                    if app.playlist_picker.is_some() {
                        let n = app
                            .playlist_picker
                            .as_ref()
                            .map(|p| p.entries.len())
                            .unwrap_or(0);
                        match action {
                            Action::Down if n > 0 => {
                                if let Some(p) = app.playlist_picker.as_mut() {
                                    p.sel = (p.sel + 1) % n;
                                }
                                app.dirty = true;
                            }
                            Action::Up if n > 0 => {
                                if let Some(p) = app.playlist_picker.as_mut() {
                                    p.sel = if p.sel == 0 { n - 1 } else { p.sel - 1 };
                                }
                                app.dirty = true;
                            }
                            Action::Activate => app.commit_playlist_picker().await,
                            Action::Back | Action::Quit => {
                                app.playlist_picker = None;
                                app.dirty = true;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    // Action menu modal: j/k navigate, Enter selects, Esc closes.
                    if app.action_menu_open {
                        let n = app.action_menu_labels().len();
                        match action {
                            Action::Down if n > 0 => {
                                app.action_menu_sel = (app.action_menu_sel + 1) % n;
                                app.dirty = true;
                            }
                            Action::Up if n > 0 => {
                                app.action_menu_sel = if app.action_menu_sel == 0 {
                                    n - 1
                                } else {
                                    app.action_menu_sel - 1
                                };
                                app.dirty = true;
                            }
                            Action::Activate => app.run_action_menu().await,
                            Action::Back | Action::Quit => {
                                app.action_menu_open = false;
                                app.dirty = true;
                            }
                            _ => {}
                        }
                        continue;
                    }
                    // Expanded-art overlay locks the input plane. Only Space
                    // (play/pause) + q (Quit) survive; everything else closes
                    // the overlay and is otherwise swallowed.
                    if app.expanded_art_uri.is_some() {
                        match action {
                            Action::PlayPause => {
                                if let Err(e) = app.handle_action(Action::PlayPause).await {
                                    app.set_status(format!("error: {e}"));
                                }
                            }
                            Action::Quit => app.shutdown.cancel(),
                            _ => {
                                app.expanded_art_uri = None;
                                app.dirty = true;
                            }
                        }
                        continue;
                    }
                    if action != Action::None {
                        if let Err(e) = app.handle_action(action).await {
                            app.set_status(format!("error: {e}"));
                        }
                    }
                }
                Some(Ok(Event::Mouse(m))) => {
                    let action = app.handle_mouse(m);
                    if action != Action::None {
                        if let Err(e) = app.handle_action(action).await {
                            app.set_status(format!("error: {e}"));
                        }
                    }
                }
                Some(Ok(Event::Resize(_, _))) => app.dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    app.set_status(format!("term err: {e}"));
                }
                None => break,
            },
            ev = mpd_events.next().fuse() => match ev {
                Some(e) => app.handle_mpd_event(e).await,
                None => app.set_status("MPD event stream ended"),
            },
            ev = spotify_events.recv() => {
                if let Some(e) = ev {
                    app.handle_spotify_event(e).await;
                }
            },
            _ = wake_rx.recv() => {
                while wake_rx.try_recv().is_ok() {}
                app.drain_toast_inbox();
                app.dirty = true;
            }
            batch = row_batch_rx.recv() => {
                if let Some(b) = batch {
                    app.handle_row_batch(b);
                }
            }
            _ = tick.tick() => app.on_tick().await,
            req = ipc_rx.recv() => {
                if let Some(req) = req {
                    let reply = app.handle_ipc(&req.line).await;
                    let _ = req.reply.send(reply);
                }
            }
            ev = recv_mpris(&mut mpris_events) => {
                if let Some(e) = ev {
                    let action = mpris_event_to_action(e);
                    if action != Action::None {
                        if let Err(err) = app.handle_action(action).await {
                            app.set_status(format!("error: {err:#}"));
                        }
                    }
                }
            }
        }
        // Push state diffs to MPRIS subscribers after every loop iteration.
        // Cheap when nothing changed (early-return on equality).
        app.sync_mpris();
    }
    Ok(())
}

/// Helper that returns Pending if mpris isn't enabled, so the select arm just
/// never fires on non-Linux / disabled builds.
async fn recv_mpris(
    rx: &mut Option<UnboundedReceiver<crate::mpris::MprisEvent>>,
) -> Option<crate::mpris::MprisEvent> {
    match rx.as_mut() {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

fn mpris_event_to_action(ev: crate::mpris::MprisEvent) -> Action {
    use crate::mpris::MprisEvent;
    match ev {
        MprisEvent::PlayPause | MprisEvent::Play | MprisEvent::Pause => Action::PlayPause,
        MprisEvent::Next => Action::NextTrack,
        MprisEvent::Previous => Action::PrevTrack,
        MprisEvent::Stop => Action::Stop,
        MprisEvent::SetVolume(v) => Action::SetVolume(v),
    }
}

fn tick_interval() -> Interval {
    let mut i = time::interval(Duration::from_millis(250));
    i.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    i
}
