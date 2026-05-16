# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
