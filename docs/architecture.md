# fuga architecture

Reference for contributors. The repository code is the source of truth;
this document captures the intent and trade-offs behind the structure so
that changes can be evaluated against the original design.

---

## 1. What fuga is

A **library connector**, not a "music player with three modes." Each source —
local, radio, somafm, spotify, youtube — implements the same `MusicSource`
trait. The user works one **source mode** at a time (`t` cycles them); each
mode carries its own tabs, columns and accent palette. Search is scoped to
the active source. Across every mode there is a single unified queue and a
single now-playing pane, and a dispatcher routes playback to the right
backend.

> Note: an earlier design called for one merged search box and a single
> library tree with all sources as top-level nodes. fuga settled instead on
> the per-mode layout as the per-source tabs/columns/palettes were built, so
> search runs against the active source only. This reversal is intentional.

Inspired by:
- **ncspot** — keyboard UX, vim navigation, fast quit/resume
- **spotatui** — Spotify depth (saved albums, followed artists, playlists, audio features, lyrics)
- **rmpc** — terminal image protocol detection, config polish
- **ncmpcpp** — screen/Actions architecture, idle-driven update model
- **yazi** — inline thumbs via Kitty Unicode placeholders

Deliberately not inspired by:
- Mopidy (Python, alpha Spotify support, would replace what fuga does anyway)
- Spotify desktop (Electron, 500 MB)
- Cantata / RompR (web/Qt)

---

## 2. What fuga is NOT

Hard non-goals for v0.1:

- **No web UI.** TUI only.
- **No daemon mode.** Single foreground binary.
- **No plugin system.** Sources are compile-time, in this repo.
- **No visualizer / FFT.** spotatui has it; not needed here.
- **No crossfade between sources.** A half-second gap at a source boundary is acceptable.
- **No autotools / Meson / CMake.** Cargo only.
- **No GUI fallback.** If the terminal can't render, print an error and exit.

Two "not yet" items from v0.1 have since shipped and are no longer non-goals:
synced **lyrics** (`src/lyrics.rs` — lrclib.net + embedded tags) and a
**YouTube** source via `yt-dlp` shell-out (search + play, plus an opt-in
single-track download bound to `d`; no library browse). See the README Legal
section for YouTube terms.

---

## 3. Target environment

- **OS:** Void Linux (primary). Other Linuxes should work, untested.
- **Terminal:** `st` patched with `kitty-graphics-protocol`, or any
  Kitty-graphics-capable terminal (kitty, Ghostty, WezTerm, recent Konsole).
- **Audio:** ALSA (default) or PulseAudio. PipeWire via Pulse compat.
- **Backend daemon:** `mpd` running locally with the user's music library indexed.
- **Spotify:** Premium account plus a Spotify Developer app (client_id) for
  Web API auth.

---

## 4. Architecture

### 4.1 Source plugin model

Every source implements `MusicSource`. The trait is the spine of fuga.

The definition in `src/source/mod.rs` is the source of truth; the core of it:

```rust
#[async_trait]
pub trait MusicSource: Send + Sync {
    fn scheme(&self) -> &'static str;   // "local" | "radio" | "somafm" | "spotify" | "youtube"
    fn display_name(&self) -> &'static str;

    async fn search(&self, query: &str) -> Result<Vec<Item>>;
    async fn browse(&self, path: &str) -> Result<Vec<Entry>>;
    // Paginated browse: forwards each page down `tx` so a large view renders
    // as it streams instead of blocking on the full result.
    async fn browse_streaming(&self, path: &str, tx: Sender<Result<Vec<Entry>>>);
    async fn resolve(&self, uri: &str) -> Result<Playable>;
    async fn play(&self, playable: &Playable) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn pause(&self) -> Result<()>;   // default impl: Err("not supported")
    async fn resume(&self) -> Result<()>;
    async fn playback_status(&self) -> Result<Option<PlaybackStatus>>;
    async fn set_volume(&self, vol: u8) -> Result<()>;
    async fn seek(&self, position: Duration) -> Result<()>;

    // Raw encoded (JPEG/PNG) bytes. Decoding + caching happen in the CALLER
    // (src/art_cache.rs), not the source, so the cache is shared across sources.
    async fn art(&self, uri: &str, size: ArtSize) -> Result<Vec<u8>>;
    async fn is_saved(&self, uri: &str) -> Result<bool>;
    async fn save(&self, uri: &str) -> Result<()>;
    async fn unsave(&self, uri: &str) -> Result<()>;
}

pub enum ArtSize { Thumb, Medium, Full }
```

The trait also carries a set of capability methods that only some sources
implement and that default to a no-op or error: `list_devices` /
`transfer_to_device`, `add_to_playlist` / `remove_from_playlist`,
`relation_uri`, `download`, `embedded_lyrics`, `view_snapshot`,
`rate_limit_remaining`. In practice these are the Spotify (and, for
`download`, YouTube) feature plane wearing the shared-trait costume — a known
fat-union shape rather than a clean minimal spine.

### 4.2 Implementations

(`Item`, `Entry`, `Playable`, `ArtSize` live in `src/types.rs`, not here.)

```
src/source/
├── mod.rs           # MusicSource trait
├── local.rs         # LocalSource    — wraps mpd_client::Client
├── radio.rs         # RadioSource    — generic .pls/.m3u + MPD transport
├── somafm.rs        # SomaFmSource   — channels.json + MPD transport
├── mpd_shared.rs    # shared MPD status/volume helpers (local/radio/somafm/youtube)
├── youtube.rs       # YouTubeSource  — yt-dlp shell-out (search/play/download) + MPD transport
└── spotify/         # SpotifySource — its own subdirectory, the big one
    ├── mod.rs       # the source impl + Spotify view types
    ├── auth.rs      # OAuth PKCE flow + token storage + refresh
    ├── metadata.rs  # Web API / mercury metadata (artist / track / album lists)
    ├── raw.rs       # raw mercury + protobuf endpoints (rootlist decode, etc.)
    ├── player.rs    # embedded librespot player wiring
    ├── cache.rs     # Spotify view snapshot caching
    └── governor.rs  # Web API rate-limit governor (cooldown, real Retry-After)
```

### 4.3 Unified queue

The queue lives in `App` state; no source owns it.

```rust
pub struct Queue {
    items: Vec<QueuedItem>,
    current: Option<usize>,
}

pub struct QueuedItem {
    pub source_scheme: &'static str,
    pub uri: String,
    pub display: ItemDisplay, // title, artist, album, art_uri, duration
}
```

The dispatcher (`src/dispatch.rs`) handles "advance to next":
1. Read `queue.current + 1`.
2. Look up the source by scheme.
3. Call `current_source.stop().await?` if the source is changing.
4. Call `next_source.play(&playable).await?`.
5. Update `current`.

**Critical:** when crossing a source boundary (e.g. Spotify → local FLAC),
the previous source must fully stop before the next begins, or you get
double audio. A half-second silent gap is acceptable; double audio is not.

### 4.4 Audio output topology

- **LocalSource, RadioSource, SomaFmSource:** delegate audio to MPD. Set
  MPD output to ALSA or PulseAudio in `mpd.conf`. fuga does not touch
  audio for these sources.
- **SpotifySource:** embedded `librespot` with `rodio` or `cpal` backend
  writing directly to ALSA/Pulse. fuga owns the audio stream.

When MPD is active, Spotify is silent (paused/stopped). When Spotify is
active, MPD is `stop`ped (not paused — we don't want it to auto-resume).
The dispatcher coordinates the handoff.

### 4.5 Event loop

A single tokio runtime, multi-threaded by default. Main loop in `src/app.rs`:

```rust
loop {
    tokio::select! {
        ev = terminal_events.next() => handle_terminal(ev?, &mut app).await?,
        ev = mpd_events.next()      => handle_mpd_event(ev?, &mut app).await?,
        ev = spotify_events.next()  => handle_spotify_event(ev?, &mut app).await?,
        _ = tick_interval.tick()    => app.tick().await?,  // 250ms — for progress bar
        _ = shutdown.recv()         => break,
    }
    if app.dirty { app.render(&mut terminal).await?; app.dirty = false; }
}
```

`mpd_events` is a stream from `mpd_client::Client::idle()`. `spotify_events`
is a stream from librespot player events plus a periodic Web API poll for
"playback state changed externally" (someone hit pause on their phone —
Spotify Connect is bidirectional).

The UI renders only when `dirty` is set. Event handlers set the flag.
There is no poll-render at 60 fps.

---

## 5. Source-by-source detail

### 5.1 LocalSource

Wraps `mpd_client::Client`. Connection target comes from config (default
`localhost:6600`).

- `search`: `mpd_client::commands::Find` with `Filter::tag(Tag::Title, query)` etc.
- `browse`: `mpd_client::commands::List` for tags, `lsinfo` for directories.
- `resolve`: returns `Playable::Url(uri)` where `uri` is the MPD library URI.
- `play`: `clear` queue → `add uri` → `play 0`.
- `art`: try `albumart` first, fall back to `readpicture` (embedded), fall
  back to scanning the file's directory for `cover.{jpg,png}`. Cached by song URI.

Idle subsystems used: `Player`, `Queue`, `Mixer`, `Database`.

### 5.2 RadioSource

User-supplied URLs (config `[[radio]]` blocks) plus the `:add <url>`
runtime command.

- `search`: substring match on station name in user config.
- `browse`: lists user-defined stations.
- `resolve`: URLs ending in `.pls` or `.m3u(8)` are fetched, parsed, and
  the first stream URL is extracted. `m3u8-rs` for m3u/m3u8; `.pls` is
  hand-rolled (~30 lines, INI-style).
- `play`: send the resolved stream URL to MPD via `addid` + `play`.
- `art`: optional per-station `art_url` in config. Otherwise none.

ICY metadata is read via MPD's `currentsong` `Title:` field — for radio,
MPD surfaces the live ICY `StreamTitle` there.

### 5.3 SomaFmSource

Pulls `https://api.somafm.com/channels.json` once at startup, caches at
`$XDG_CACHE_HOME/fuga/somafm.json` for 6 hours.

Channel JSON schema (pin once; the upstream API has no published schema):
```json
{
  "channels": [{
    "id": "groovesalad",
    "title": "Groove Salad",
    "description": "...",
    "dj": "Rusty Hodge",
    "genre": "ambient|downtempo|electronica",
    "image": "https://api.somafm.com/img/groovesalad120.png",
    "largeimage": "...",
    "xlimage": "...",
    "playlists": [
      {"url": "https://...groovesalad.pls", "format": "mp3", "quality": "highest"},
      {"url": "...", "format": "aac", "quality": "highest"}
    ],
    "lastPlaying": "Artist - Track",
    "listeners": "1234"
  }]
}
```

- `search`: substring match on title/genre/description.
- `browse`: returns all channels as Entries.
- `resolve`: picks the highest-quality MP3 playlist URL, fetches the .pls,
  extracts the stream URL.
- `play`: hands the stream URL to MPD.
- `art`: downloads `xlimage` (or `largeimage` for thumbs), cached by channel id.

### 5.4 SpotifySource — the heavy one

Uses `rspotify` for Web API and embedded `librespot` for playback. A single
struct owns both (plus the rate-limit governor and view caches — see the
file tree above). Auth is **PKCE**: a public `client_id` only, no client
secret. Sketch of the config:

```rust
pub struct SpotifyConfig {
    pub client_id: String,          // PKCE public client id — no secret
    pub device_name: String,        // shows up as the Spotify Connect device name
    pub bitrate: librespot_playback::config::Bitrate,
    pub volume_normalisation: bool,
    pub cache_dir: PathBuf,         // librespot's audio cache, separate from art cache
}
```

#### 5.4.1 OAuth PKCE flow (`auth.rs`)

1. Generate `code_verifier` (43–128 chars, URL-safe random).
2. Compute `code_challenge = base64url(sha256(code_verifier))`.
3. Open browser to `https://accounts.spotify.com/authorize?...&code_challenge_method=S256&code_challenge=...`.
4. Spawn a localhost HTTP listener on the redirect port (default 8888) —
   ~50 lines of `tokio::net::TcpListener` plus a one-shot GET parser. No
   axum/warp needed.
5. Receive `?code=...`, POST to `https://accounts.spotify.com/api/token`
   with `code_verifier`, store access + refresh tokens at
   `$XDG_DATA_HOME/fuga/spotify_tokens.json` (mode 0600).
6. Refresh proactively when the token has less than 5 minutes left.

Reuse `rspotify::AuthCodeSpotify` if the PKCE flow fits; if it requires a
callback closure that doesn't compose with the event loop, write the flow
directly with `reqwest` + `serde_json`.

#### 5.4.2 Web API surface (`metadata.rs` / `raw.rs` / `governor.rs`)

Wrap `rspotify` calls with:
- **Rate-limit backoff:** on 429, read `Retry-After`, sleep, retry once.
- **Token refresh on 401:** refresh, retry once, then propagate.
- **Pagination:** collect all pages for library calls; lazy-paginate search results.

Endpoints used:
- `search` (track, album, artist, playlist)
- `current_user_saved_albums`, `current_user_saved_tracks`, `current_user_followed_artists`
- `current_user_playlists` + `playlist_items`
- `album_tracks`, `artist_albums`, `artist_top_tracks`
- `current_playback`, `transfer_playback`, `start_playback`, `pause_playback`,
  `next_track`, `previous_track`, `seek_track`, `volume`

For algorithmic playlists (Discover Weekly), Spotify removed the
`/v1/recommendations` endpoint and stopped surfacing those playlists in
the public Web API. fuga falls back to a mercury rootlist walk via
librespot's `SpClient::get_rootlist` to recover them.

#### 5.4.3 Embedded librespot (`player.rs`)

```rust
use librespot_core::{cache::Cache, config::SessionConfig, session::Session};
use librespot_playback::{
    audio_backend, config::{AudioFormat, PlayerConfig, Bitrate},
    mixer::{self, MixerConfig},
    player::Player,
};

pub async fn create_player(config: &SpotifyConfig, credentials: librespot_core::authentication::Credentials)
    -> anyhow::Result<(Session, Player)>
{
    let session_config = SessionConfig::default();
    let cache = Some(Cache::new(Some(&config.cache_dir), None, Some(&config.cache_dir), None)?);
    let session = Session::new(session_config, cache);
    session.connect(credentials, true).await?;

    let player_config = PlayerConfig {
        bitrate: config.bitrate,
        normalisation: config.volume_normalisation,
        ..Default::default()
    };

    let backend = audio_backend::find(None).expect("no audio backend");
    let mixer = mixer::find(None).unwrap()(MixerConfig::default());
    let (player, _events_rx) = Player::new(
        player_config, session.clone(), mixer.get_soft_volume(),
        move || backend(None, AudioFormat::default()),
    );

    Ok((session, player))
}
```

Play a track: `player.lock().await.load(SpotifyId::from_uri("spotify:track:...")?, true, 0)`.

Stop: `player.lock().await.stop()`.

The player events receiver (`PlayerEvent::EndOfTrack`, `PlayerEvent::Stopped`,
etc.) feeds the `spotify_events` stream in the main loop. `EndOfTrack`
triggers the dispatcher to advance the queue.

**Pitfall:** librespot `Session::connect` blocks several seconds on first
auth. Do it as a background task and show "Connecting to Spotify..." in
the UI; do not block startup.

**Pitfall:** librespot has its own audio output. If MPD's output and
librespot's output target the same ALSA device they fight. Use
PulseAudio/PipeWire to share safely, or different ALSA devices.

#### 5.4.4 Spotify-specific views

The Spotify tab is deeper than other sources. Sub-views:
- Search (across tracks/albums/artists/playlists)
- Saved Albums
- Saved Tracks ("Liked Songs")
- Followed Artists
- Playlists (own + followed)
- Recently Played
- New Releases

Each sub-view is a `View` impl with its own keybindings.

---

## 6. UI

### 6.1 Stack

- `ratatui` for layout and widgets
- `ratatui-image` (Kitty backend) for inline thumbs and now-playing art
- `crossterm` as the ratatui backend
- A custom widget for source-list with embedded image cells

### 6.2 Tab layout

Tabs are per **source mode** (`t` cycles modes), not merged across sources —
each mode carries its own tab set, column layout and accent. The active
mode's tabs are configurable in `[ui] tabs`; a tab whose backing source
isn't enabled hides automatically.

### 6.3 Inline thumbnails

**Mechanism:** Kitty graphics protocol Unicode placeholders. Each unique
cover is transmitted once with `a=t,U=1,i=<id>,f=100,t=d`, then row
rendering emits Unicode `U+10EEEE` with diacritics referencing the image
id and row/col within the image.

`ratatui-image` configured for the Kitty backend handles this. Each list
row contains a small `StatefulImage` widget (2 cells wide × 2 tall) plus
text columns.

**Cache architecture:** see `src/art_cache.rs`. Decoded images live in an
LRU in RAM; bytes are cached on disk under `$XDG_CACHE_HOME/fuga/art/`.
In-flight fetches are deduplicated and concurrency is bounded by a
semaphore so the Spotify CDN isn't hammered.

**Fetch policy on scroll:**
- Visible rows + 10-row look-ahead in the scroll direction → enqueue art fetches.
- Rows scrolled out of view → drop the future (in-flight stays warm,
  decoded LRU evicts naturally).
- Semaphore caps concurrent fetches at 8.

**Thumb size:** 2 cells wide × 2 cells tall by default. Configurable via
`[ui] thumb_cells`. Bigger is slower scroll; smaller is unrecognizable.

**Toggle:** `T` cycles thumb modes. On a non-Kitty terminal, fuga
auto-detects at startup (sends a Kitty query escape and waits for
response) and falls back to halfblocks.

### 6.4 Now-playing art

Larger bitmap, ratatui-image classic-placement Kitty mode. Re-renders only
on track change (debounced 100 ms for rapid skipping).

### 6.5 Keybindings

Vim-ish, ncspot-influenced. Compile-time defaults live in `src/keys.rs`;
runtime overrides go in `~/.config/fuga/keys.toml`. See the README for
the current default keymap.

### 6.6 Command bar (`:`)

Vim-style commands:
- `:add <uri>` — add to queue
- `:play <uri>` — clear queue and play
- `:goto <n>` — jump to queue index
- `:vol <0..100>` — set master volume
- `:q` — quit

---

## 7. Configuration

`~/.config/fuga/config.toml`; see `examples/config.toml` for the full
shipped example.

Token storage at `$XDG_DATA_HOME/fuga/spotify_tokens.json`, mode 0600.
Token values are never logged.

---

## 8. Build, dependencies, packaging

See `Cargo.toml` for the dependency manifest. Build:

```sh
cargo build --release           # production
cargo run -- --debug            # dev
cargo test                      # unit tests
cargo clippy -- -D warnings     # lint
cargo fmt                       # format
```

Major-version bumps for `librespot` and `rspotify` are not blind — both
have breaking-change histories. Pin via `Cargo.lock`.

### Void Linux packaging

A template is available; see the README for current packaging status.
Document the st-graphics patch dependency in the README; it is not
enforceable via xbps.

### st-graphics patch (terminal dependency)

Users running `st` need the kitty-graphics-protocol patch from
`sergei-grechanik/st-graphics`. README has install steps.

---

## 9. Conventions

### 9.1 Error handling

- **Everywhere:** `anyhow::Result<T>` for prose-y error chains. (An earlier
  plan split library code onto `thiserror` typed enums; it was dropped — no
  caller needed to match on error variants, so `thiserror` came out as an
  unused dependency.)
- **Never `unwrap()` in async tasks or hot paths.** OK in tests and
  `main()` for unrecoverable startup failures.
- **Log errors at `tracing::error!` before propagating** in handlers the
  user won't see directly.
- **User-facing errors go to a status-bar toast**, not panics. Panics in
  spawned tasks must be caught (`tokio::task::JoinError`) and surfaced.

### 9.2 Async

- All I/O async. No `std::fs`, no `reqwest::blocking`.
- Long-running tasks (librespot session, MPD idle loop) are
  `tokio::spawn`ed; `JoinHandle`s live in `App` for shutdown.
- Cancellation: a single `shutdown` `CancellationToken` lives in `App` and is
  `select!`ed in the main loop. Individual background tasks are not separately
  cancellable — teardown is `std::process::exit(0)` (librespot's `Player::Drop`
  blocks, so a cleanly joined shutdown isn't worth the hang). Streaming-browse
  tasks self-cancel by checking an epoch/generation instead.
- Don't hold a `Mutex` (sync or async) across `.await` unless it's a
  `tokio::sync::Mutex` and you've thought about deadlock.

### 9.3 Logging

- `tracing` everywhere. `tracing_subscriber::EnvFilter` from `RUST_LOG`.
- Default level: `info` for fuga, `warn` for deps.
- Log file: `$XDG_CACHE_HOME/fuga/fuga.log` (rotated at 10 MB). Never log
  to stdout in TUI mode — it corrupts the screen.
- Spans: one per Spotify Web API request, one per MPD command batch, one
  per art fetch.

### 9.4 Module style

- One `mod.rs` per directory. Re-export public types from `mod.rs`.
- Public surface stays small: `App`, `Config`, the `MusicSource` trait,
  a handful of core types. Everything else is `pub(crate)` or private.
- Tests live next to code: `#[cfg(test)] mod tests { ... }` at the bottom
  of each file.

### 9.5 Performance

- Don't re-decode images on every render. `ArtCache` returns `Arc<DynamicImage>`.
- Don't re-fetch Spotify search on every keystroke. Debounce 200 ms.
- Don't re-render the full UI on every event. Set the `dirty` flag;
  render once at the end of the event-loop iteration if dirty.
- Profile with `cargo flamegraph` if scroll is laggy. Common culprits:
  synchronous image decode on the UI thread, unbounded fetch parallelism
  saturating the network.

---

## 10. Roadmap

### Shipped (through v0.4)
- Local + Radio + SomaFM + Spotify + YouTube sources, end-to-end.
- Inline Kitty thumbnails with halfblocks fallback; per-source columns,
  column headers and accent palettes; sidebar art mode.
- Synced lyrics (lrclib.net + embedded tags).
- MPRIS bridge (Linux) + a macOS media-key bridge; runs on Linux and macOS.
- Unix-socket control plane (`fuga play`, `fuga next`, …).
- Lifecycle hooks (`on_track_change`, `on_source_switch`, `on_startup`).
- Hardening: fuzz targets, a PTY soak/perf harness, queue proptests,
  cargo-deny supply-chain config, idle-CPU regression guards.

### Planned
- Platform expansion — the current north star: FreeBSD → NetBSD → OpenBSD →
  Windows (halfblocks-only thumbs on Windows).
- Same-source crossfade.
- Additional radio directory source (radio-browser.info).

---

## 11. Common pitfalls / decisions log

**Why Rust over C:** librespot is Rust (no clean C embed path), rspotify
handles 2k+ LOC of Spotify API plumbing, and `ratatui-image` solves
inline thumbs idiomatically. C is workable for the original scope (MPD +
radio); it stops being workable once Spotify Web API + embedded librespot
+ LRU image cache + scroll-aware fetch parallelism are in scope.

**Why a source-plugin trait over a thin TUI on Mopidy:** the Mopidy MPD
frontend doesn't implement `albumart`/`readpicture`, so you'd need its
JSON-RPC API anyway; mopidy-spotify is alpha and flaky; replacing Mopidy
in Rust is cleaner than depending on it.

**Why librespot embedded over a spotifyd subprocess:** spotifyd is fine,
but it adds a process boundary, requires Spotify Connect
"transfer playback" round-trips for every play action, and has its own
auth/cache state to manage. Embedded gives tighter control and matches
what spotatui/ncspot do.

**Why MPD as transport for local + radio + somafm:** MPD's curl and
ffmpeg input plugins handle every codec and stream type out of the box;
reimplementing audio decode for these source types in Rust would
duplicate well-solved work.

**Why a unified queue in fuga rather than the MPD queue:** MPD doesn't
know about Spotify URIs. The dispatcher has to own the schedule.

**Why no crossfade in v1:** crossfade across source boundaries means
decoding two streams simultaneously and mixing — currently MPD owns one
audio path and librespot owns another; mixing requires either both
writing to a software mixer (PulseAudio module) or a custom audio
router. Out of scope for v1.

---

## 12. References / source material

Code to read for inspiration:

- **ncspot** (github.com/hrkfdn/ncspot) — Cursive-based, librespot
  embedded. Reference for Spotify session/player wiring and UX feel.
- **rmpc** (github.com/mierak/rmpc) — Rust MPD client with
  Kitty/Sixel/iTerm2/Ueberzugpp image protocol detection. Reference for
  terminal probing and config polish.
- **spotatui** (github.com/LargeModGames/spotatui) — Spotify TUI with
  native streaming and synced lyrics. Reference for Spotify feature breadth.
- **yazi** (github.com/sxyazi/yazi) — File manager with inline Kitty
  Unicode placeholder thumbs. Reference for inline-thumb scroll model
  and ArtCache architecture.
- **st-graphics** (github.com/sergei-grechanik/st-graphics) — Terminal
  patch fuga depends on. Read its README for tmux passthrough setup.
- **ratatui-image** (github.com/ratatui/ratatui-image) — Image widget
  crate. Read examples for stateful image rendering.
- **librespot** (github.com/librespot-org/librespot) — Spotify protocol.
- **rspotify** (github.com/ramsayleung/rspotify) — Web API.

External specs:
- Kitty graphics protocol — sw.kovidgoyal.net/kitty/graphics-protocol/
- MPD protocol — mpd.readthedocs.io/en/latest/protocol.html
- SomaFM channels JSON — `https://api.somafm.com/channels.json`
- Spotify Web API — developer.spotify.com/documentation/web-api

---

## 13. When stuck

- **Audio fights between MPD and librespot:** check that both aren't
  holding the same ALSA hw device. Use `pulse` or `pipewire` output for
  both, or different ALSA devices.
- **Spotify auth fails:** delete `$XDG_DATA_HOME/fuga/spotify_tokens.json`
  and re-auth. Confirm the Developer app redirect URI matches
  `redirect_port` in config.
- **Inline thumbs invisible:** verify the terminal supports Kitty
  graphics with `printf '\e_Gi=31337,s=1,v=1,a=q,t=d,f=24;AAAA\e\\'` and
  check for an OK response. Under tmux, confirm `allow-passthrough on`
  and tmux ≥ 3.4. Under SSH, install the st terminfo on the remote.
- **librespot session disconnects after a few minutes:** known librespot
  behavior on idle. Reconnect logic lives in
  `SpotifySource::ensure_session()`. Don't rely on the session being
  alive across Web API calls; check before use.
- **MPD idle hangs:** `mpd_client::Client::idle()` returns a stream; if
  it stops yielding the connection died. Detect with a heartbeat ping
  every 30 s and reconnect on failure.
- **Crash in art rendering:** fall back to a text-only row, log the
  error, keep going. Never let art failure crash the UI.
