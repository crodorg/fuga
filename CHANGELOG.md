# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.1] — 2026-07-03

### Fixed

- **Local, YouTube, and SomaFM queues now advance automatically.** A multi-track
  album or queue from an MPD-backed source stopped after the first track — only
  Spotify advanced on its own. fuga now detects when an MPD-backed track ends and
  moves to the next item in the queue.
- **Search and the Spotify device picker no longer freeze the interface.** A
  YouTube search (which shells out to yt-dlp) or opening the device picker used to
  block all input and redraw until it finished; both now run in the background and
  the UI stays responsive, with stale search results discarded.
- **`--config <path>` is now honored.** The flag was accepted but silently
  ignored; fuga now loads its configuration from the given path.

### Security

- The Spotify token cache and the IPC control socket are now created with
  owner-only permissions (`0600`), so other local users can't read the cached
  account token or drive playback through the control socket.

## [0.4.0] — 2026-07-01

### Added

- **Per-source column layouts.** Track rows adapt their columns to the source:
  Artist/Song/Album/Time for music, a single full-width Podcast column plus Time
  for podcasts, Artist/Song/Time for YouTube, Artist/Radio/Genre/Time for SomaFM,
  and a single full-width Radio column for internet radio.
- **Column-header bar** on the box's top border, labelling the columns per source
  (config `[ui] column_headers`, default on).
- **Sidebar now-playing art layout** — a third mode on the `e` cycle
  (expanded → collapsed → sidebar): the now-playing status stacks above the cover
  in a full-height right column and the list becomes a clean full-height rectangle.
- **Column text wrapping in icon mode** — with inline thumbnails on, long
  artist/song/album text wraps across the row's two cells instead of truncating
  (config `[ui] wrap_columns`, default on).

### Changed

- The panel title and status notification now sit on the box's **bottom** border,
  freeing the top border for the column headers.
- `radio_split` now defaults to **on**, so Radio and SomaFM start as separate tabs.

### Fixed

- **j/k scroll CPU spike** on large libraries — the visible list was rebuilt and
  re-cloned in full every frame; it is now cached and rebuilt only when the
  underlying data changes.
- **Scroll stutter / dropped rows** when scrolling fast — key input now renders
  immediately instead of beating against the frame-rate cap.
- The hovered-row selection highlight now fills the **full row width** instead of
  stopping where the column text ends.

## [0.3.6] — 2026-06-28

### Fixed

- **100% CPU while idle without a desktop session bus** — when fuga ran somewhere
  with no D-Bus session bus (a headless server, a plain SSH session, a minimal
  window manager, a container), its media-key bridge couldn't reach the bus and
  shut down. That left fuga's main loop endlessly polling a closed channel,
  pinning a CPU core even while nothing was happening. fuga now stops watching a
  source once it goes away, so an idle session uses no CPU. On a normal desktop,
  where the session bus is present, nothing changes.
- **Idle spin if the MPD connection dropped** — the same kind of stall could
  happen if MPD went away mid-session (a service restart, a network blip). The
  local source now pauses cleanly instead of spinning, and the rest of fuga keeps
  working.

## [0.3.5] — 2026-06-27

### Fixed

- **Spotify library views failing to load ("rate-limited" / "load failed")** —
  fuga could overrun Spotify's Web API request limit, after which the library
  stopped loading entirely; worse, retrying during the cooldown made Spotify
  extend it, so a brief limit could snowball into hours. fuga now paces all of
  its Web API calls, stops calling the moment it's rate-limited — showing how
  long until it can retry instead of a generic failure — and remembers that
  cooldown across restarts rather than immediately re-triggering it. Background
  change-polling is also far less frequent and pauses entirely while fuga isn't
  the focused window, so steady listening rarely approaches the limit.
- **Spotify playback stopped after about ten songs** — a track would halt at
  0:00 and refuse to advance. fuga drives the librespot player directly and
  manages its own queue, but it also ran librespot's Spotify Connect controller
  in parallel; with no Connect context that controller stopped the player at
  every track end, racing fuga's queue advance and occasionally killing the next
  track as it started. fuga now authenticates the session directly and no longer
  runs that controller, so the race is gone. (fuga no longer advertises itself as
  a Spotify Connect device.)
- **One unplayable track no longer stalls the queue** — when a Spotify track
  fails to start (an unavailable track, a CDN error, a region block), fuga skips
  to the next item instead of stopping, halting only if several tracks fail in a
  row.
- **Album art going blank in tmux** — after switching away from and back to
  fuga's tmux window, Kitty album art could vanish and leave reddish placeholder
  blocks until the track changed. fuga now repaints the art when the window
  regains focus.

### Changed

- **`d` downloads the hovered track** (previously `Y`). The device picker `d`
  used to open is unbound by default — without Spotify Connect it can no longer
  see or control fuga's own playback — but it remains available to bind in your
  config.

## [0.3.4] — 2026-06-24

### Fixed

- **High idle CPU usage** — fuga continuously used roughly 10% of a CPU core
  even when stopped, most noticeable on macOS. The terminal input path polled
  an async event stream that busy-spun on its internal lock with no input
  pending; input now reads on a dedicated blocking thread, dropping idle CPU
  to near zero. A control-socket bind failure (for example when
  `XDG_RUNTIME_DIR` pointed at a directory absent on the host) could also spin
  the event loop — the socket path now falls back to `/tmp` and a failed bind
  no longer burns a core.

### Changed

- **Lower CPU while playing**, with no change to behavior or appearance —
  redraws are gated on the visible elapsed quantum (whole seconds, progress-bar
  steps, active lyrics line) rather than the raw sub-second position, the MPD
  status and current-song polls are batched into a single round-trip, the
  configured `fps_cap` is honored as a redraw cap, and the browse view is built
  without a per-frame deep clone.

## [0.3.3] — 2026-06-22

### Fixed

- **Radio station artwork now loads** — the cover URL set via a station's
  `art_url` (including `.ico` favicons) never displayed, in either the row
  thumbnail or the now-playing panel. The radio source expected its own
  internal station id where the art pipeline hands it the image URL, so every
  fetch failed silently. Artwork now renders for all internet-radio stations.

## [0.3.2] — 2026-06-19

### Added

- **`e` toggles the now-playing art size** — collapse the album-art panel
  into the bottom bar, or expand it back to full size, straight from the
  keyboard. Previously this was only reachable by clicking the panel.

### Fixed

- **Expanding the art panel left it blank** — after collapsing the
  now-playing cover and expanding it again, the full-size image failed to
  repaint until the next track changed. The panel now rebuilds its image
  protocol on each toggle, so the cover always reappears.

## [0.3.1] — 2026-06-19

### Fixed

- **Album art under tmux** — inline Kitty thumbnails and the now-playing
  image no longer fall back to half-blocks, and no longer break after
  switching tmux windows. The pane's `allow-passthrough` is now held at
  `all` after terminal probing so art survives window switches, and Kitty
  support is detected reliably even when several clients are attached to the
  same tmux session — previously a non-Kitty client could win the
  capability-query response race and mask the real terminal's reply.

### Changed

- Migrated to the Rust 2024 edition. No behavior change.

## [0.2.1] — 2026-05-30

### Added

- **Synced lyrics** — press `B` on any playing track to open a dedicated
  lyrics view in the body pane. Timestamped lyrics scroll with playback,
  active line centered and highlighted; untimed lyrics render as a static
  block. Works for local, Spotify, and YouTube tracks — any source whose
  rows carry a track title, artist, and fixed duration (live radio and
  SomaFM streams have no track duration, so lrclib can't match them).
  Lyrics come from the free
  [lrclib.net](https://lrclib.net) API; local files with embedded
  `SYNCEDLYRICS` / `LYRICS` tags use those instead of the network. `Esc`
  or `h` closes the view. Fetch is lazy — nothing hits the network until
  you first open lyrics on a track.

## [0.2.0] — 2026-05-15

Performance and UX pass. Remote sources stream rows in as each page
arrives; the now-playing block tints by the *playing* track's source
even when you browse elsewhere; macOS gets first-class media keys.
Homebrew install lands as the recommended path on macOS.

### Added

- **Streaming browse** — Spotify and YouTube views render rows as each
  page arrives instead of blocking on the full pagination. First rows
  visible in ~200ms (was ~800ms for Saved Albums). Animated loading
  indicator on the right of the view header while pages are still in.
- **Source-aware now-playing tint** — art-panel border and bottom-bar
  title follow the *playing* track's source, not the active browse
  mode. Browse Local with Spotify playing → now-playing block stays
  Spotify-green; rest of the UI stays Local-white.
- **macOS media keys** — Touch Bar / keyboard Play, Pause, Next, Prev
  drive fuga via `MPRemoteCommandCenter`. macOS "Now Playing" widget
  reflects fuga's current track. Background daemon-like via
  NSApplication Accessory policy (no dock icon).
- **XDG paths on macOS** — `$XDG_CONFIG_HOME`, `$XDG_CACHE_HOME`,
  `$XDG_DATA_HOME` are honored (otherwise falls back to
  `~/Library/Application Support/fuga`).
- **`[ui.tabs]` per-source tab override** — each source picks its own
  tab list. Source key order in `[ui.tabs]` also drives the `t`-cycle
  order and the startup source.
- **`[youtube] download_dir`** — explicit override; falls back to MPD
  `music_directory` → XDG-Downloads → `~/Downloads`.
- **`art_height_pct` / `art_width_pct`** — resize the album-art panel
  by percent. Lanczos3 scaling.
- **Window title reflects playing track** — picked up by tmux, window
  managers, waybar `title` modules.
- **Explicit transport row** on the bottom bar: `<<  [▶ playing]  >>`.
- **"Open in browser" action** on Spotify and YouTube rows.
- **`g`-leader source jumps** (`g s`, `g l`, `g y`, `g r`, `g f`)
  switch full source mode, not just the active tab.

### Fixed

- `/` filter clears on descend; `Esc` cancels a committed filter (was:
  only cleared the input buffer, left the filter active).
- Filter no longer persists across tab switches.
- Album-art scaling fills the configured rect (was: capped at native
  size, left small art floating in a half-empty panel).
- Album cell collapses when it matches the row title.
- Cross-tab `Esc`-back restores the originating tab.
- `seek_back` / `seek_forward` no longer shadowed by example-config
  default overrides.
- Expanded-art protocol isolated from inline thumb cache so the two
  don't fight over the same Kitty placement IDs.
- Worker-thread panics on macOS now exit cleanly instead of hanging
  the UI.

### Changed

- Loading indicator moved from the view body to the view-header right
  edge; animates `loading.  /loading.. /loading...` while pages are
  still arriving; 600ms minimum visibility so fast paths still
  register.
- Homebrew tap is the recommended macOS install path:
  `brew install crodorg/fuga/fuga`.

## [0.1.0] — 2026-05-12

First public release. Local files, internet radio, SomaFM, Spotify, and
YouTube all play end-to-end through one TUI, one queue, one keyboard.

### Sources

- **Local** — MPD client. Browse library by album/artist/playlist, search,
  play, enqueue. Stored MPD playlists exposed at `local:playlists` and
  `local:playlist:<name>`.
- **Spotify** — embedded `librespot` for audio, Web API + mercury fallback
  for metadata. OAuth PKCE auth via `fuga --spotify-auth`. Saved Albums,
  Liked Songs, Playlists, Followed Artists, Recently Played, Top Tracks,
  Top Artists. Spirc registration so phones see fuga as a Connect device;
  `d` opens the device picker to transfer playback.
- **YouTube** — `yt-dlp` shell-out for search, stream, and optional
  download. Save / unsave with `L`. Downloaded tracks land in the MPD
  music directory (falls back to `~/Downloads`). fuga itself never talks
  to Google — only invokes the local `yt-dlp` binary.
- **SomaFM** — channels pulled from `api.somafm.com/channels.json`,
  cached locally; handed off to MPD for playback.
- **Internet radio** — user-defined `[[radio]]` stations from config;
  `.pls` / `.m3u` resolution.

### TUI

- rmpc-style configurable tab bar that merges content across sources
  (`[ui] tabs = [...]`, `tab_alignment`, `multi_source_layout`).
- Per-tab breadcrumb stack — switching tabs preserves cursor + scroll.
- Inline album-art thumbnails on every list row via the Kitty graphics
  protocol (Unicode placeholders); halfblocks fallback on non-Kitty
  terminals; sixel option for now-playing art. `T` cycles thumb mode.
- Columned track lists: `Artist | Title | Album | Len`.
- Universal search (`/`), command bar (`:`), help overlay (`?`).
- Sort modal (`o`) with four axes (Alphabetical A–Z / Z–A, Duration,
  Year, Recently Added) and per-tab persistence.
- Shuffle (`z`) and Repeat (`x` cycles Off → All → Track), honored on
  queue advance and Spotify `EndOfTrack`.
- Bottom-bar icons: playback state, liked star, shuffle, repeat,
  per-source dot.
- Album-art panel in the bottom-right with click-swallow.
- Mouse: click tab to switch, click row to play, scroll wheel for
  cursor, scroll over volume cell to adjust volume, click progress bar
  to seek, right-click to pause/resume.
- Status toast auto-clears after 3 s.
- Per-source theme palette and configurable theme presets / overrides.

### Integrations

- **MPRIS D-Bus bridge** — media keys, system mixer, and `playerctl`
  drive fuga out of the box; outside volume changes sync back.
- **Unix-socket IPC** — `fuga play|next|prev|pause|stop|vol|status` from
  any shell while a fuga instance is running.
- **Lifecycle hooks** — `on_track_change`, `on_source_switch`,
  `on_startup` shell commands receive state through `FUGA_*` env vars.

### Architecture

- `MusicSource` trait — adding a new backend is one file.
- Audio dispatcher gates source switches so two backends never play at
  once; 750ms drain on Spotify → other-source handoff.
