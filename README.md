# fuga

```text
░█▀▀░█░█░█▀▀░█▀█
░█▀▀░█░█░█░█░█▀█
░▀░░░▀▀▀░▀▀▀░▀░▀
```

Terminal-native music library aggregator. One TUI, one queue, many sources:
local files (via MPD), internet radio, SomaFM, Spotify, YouTube. Inline
album-art thumbnails on every row when run in a Kitty-graphics-capable
terminal.

![fuga in patched st showing Liked Songs](docs/screenshot.png)

## Features

- Inline album-art thumbs on every list row (Kitty Unicode placeholders, with
  a halfblocks fallback for non-Kitty terminals)
- Five sources, one unified queue: local files (MPD), Spotify, YouTube,
  SomaFM, and user-defined internet radio
- Vim-style keybinds, mouse support, MPRIS bridge, lifecycle hooks, and a
  unix-socket IPC control plane (`fuga play <uri>`, `fuga next`, …)
- Source-plugin trait — adding a new backend is one file implementing
  `MusicSource`
- Embedded `librespot` for Spotify (no `spotifyd` subprocess) and `yt-dlp`
  shell-out for YouTube (fuga itself never talks to Google)
- Per-source theme palette and a configurable tab bar that merges content
  across sources
- See [docs/architecture.md](docs/architecture.md) for the source-plugin
  design and audio-routing notes

---

## Status

v0.1.0 — first public release. Local, radio, SomaFM, and Spotify all work
end-to-end (browse, play, queue, search, control). Audio dispatcher gates
source switches so two backends never play at once.

## Requirements

- Linux (developed on Void; other distros should work)
- Rust 1.75+ to build
- `mpd` running on `localhost:6600` with your library indexed
- A Kitty-graphics-capable terminal for inline thumbs:
  - `kitty`, `ghostty`, `wezterm`, `konsole` (recent), or
  - `st` patched with [kitty-graphics-protocol](https://github.com/sergei-grechanik/st-graphics)
  - Anything else falls back to `halfblocks` (low-resolution but renders)
  - Sixel terminals (`xterm -ti vt340`, `mlterm`, `foot`) supported via
    `thumb_mode = "sixel"` for now-playing art only — row thumbs are
    disabled because sixel cells don't anchor to scrolling rows.
    **WIP:** sixel image overflows one row above its panel border in xterm;
    use kitty mode where possible.
- Spotify Premium + a developer app (`client_id`) — only if you want Spotify

## Build

```sh
cargo build --release
install -Dm755 target/release/fuga ~/.local/bin/fuga
```

Or use the install script: `./scripts/install.sh`.

## Configure

```sh
mkdir -p ~/.config/fuga
cp examples/config.toml ~/.config/fuga/config.toml
$EDITOR ~/.config/fuga/config.toml
```

Defaults work for local-only use as long as MPD is on `localhost:6600`.

### Tab bar

The top tab bar is rmpc-style: a list of category tabs that **merge content
across sources**. Configure which tabs show via `[ui] tabs`; if you omit it
fuga derives a sensible default from the sources you have enabled.

```toml
[ui]
tabs = ["queue", "albums", "artists", "playlists", "stations", "spotify", "search"]
tab_alignment = "center"          # center | left | right
multi_source_layout = "grouped"   # how merged source lists render
radio_split = false               # true → separate Radio + SomaFM tabs
```

Recognized tab ids: `queue`, `albums`, `artists`, `playlists`, `stations`,
`radio`, `somafm`, `spotify`, `search`. Tabs whose backing sources aren't
enabled hide automatically.

### Spotify setup

1. Create an app at <https://developer.spotify.com/dashboard>. Add
   `http://127.0.0.1:8888/callback` to the app's redirect URIs.
2. Set `[spotify] enabled = true` and `client_id = "..."` in your config.
3. Run `fuga --spotify-auth` once. A browser opens; approve. Token persists
   at `~/.local/share/fuga/spotify_tokens.json` (mode 0600).
4. Run `fuga` normally.

If Spotify and MPD compete for the same ALSA device, route both through
PulseAudio or PipeWire (both expose a `pulse` device that mixes for you).

## Keys (defaults)

| Key            | Action                                        |
|----------------|-----------------------------------------------|
| `q`            | Quit                                          |
| `j` / `k`      | Down / up                                     |
| `C-d` / `C-u`  | Page down / up                                |
| `g g`          | Top                                           |
| `G`            | Bottom                                        |
| `Tab` / `S-Tab`| Cycle tabs                                    |
| `1`–`9`        | Jump to tab N (in configured order)           |
| `Enter`        | Activate (descend / play)                     |
| `a`            | Enqueue (add to queue without playing)        |
| `Esc` / `h`    | Back one level                                |
| `Space`        | Play / pause                                  |
| `n` / `p`      | Next / previous track                         |
| `s`            | Stop                                          |
| `+` / `-`      | Volume up / down                              |
| `z`            | Toggle shuffle                                |
| `x`            | Cycle repeat (off → all → track)              |
| `o`            | Sort modal                                    |
| `d`            | Spotify Connect device picker                 |
| `T`            | Cycle thumbnail mode                          |
| `r`            | Refresh current view                          |
| `/`            | Focus search input                            |
| `:`            | Focus command bar                             |
| `?`            | Toggle help overlay                           |
| `L`            | Like / unlike current track (Spotify)         |

All keys are user-configurable; see `examples/config.toml`.

**Mouse**:
- Click a tab label → switch tab
- Click a row → play / descend
- Scroll wheel anywhere → cursor up/down
- Scroll wheel over the volume cell → volume up/down
- Click on the progress bar → seek to that fraction
- Right-click on the progress bar → pause/resume
- Album-art panel swallows clicks (rows behind it don't fire)

## Command bar

Type `:` then:

| Command       | Effect                                             |
|---------------|----------------------------------------------------|
| `:q` / `:quit`| Quit                                               |
| `:add <uri>`  | Append `<uri>` to the queue (scheme picks source)  |
| `:play <uri>` | Push `<uri>`, play immediately                     |
| `:goto <n>`   | Jump to queue index `n`                            |
| `:vol <0..100>`| Set master volume                                 |

URI scheme determines the source: `local:`, `radio:`, `somafm:`, `spotify:`,
`youtube:`.

## CLI subcommands

A running fuga listens on a unix socket (`$XDG_RUNTIME_DIR/fuga.sock`).
From another shell:

```sh
fuga play spotify:track:11dFghVXANMlKmJXsNCbNl
fuga next
fuga prev
fuga pause
fuga stop
fuga vol 60
fuga status
```

`fuga status` prints `title | artist | mm:ss/mm:ss | source` so it composes
into status bars and waybar modules.

## MPRIS

Linux media keys and system mixers (KDE Plasma, GNOME, `playerctl`) drive
fuga via the MPRIS D-Bus bridge automatically — no setup. Volume changes
from outside fuga (e.g. KDE's mixer) sync back to the bottom bar.

## Hooks

Set shell commands in `[hooks]` to run on lifecycle events. They receive
state via `FUGA_*` env vars:

```toml
[hooks]
on_track_change  = "notify-send 'Now playing' \"$FUGA_TITLE — $FUGA_ARTIST\""
on_source_switch = "logger fuga: $FUGA_SOURCE_FROM -> $FUGA_SOURCE_TO"
on_startup       = "echo started >> ~/.cache/fuga/runs.log"
```

## Search

Press `/`, type, Enter. Query fans out across every registered source in
parallel; results are grouped by source. `j`/`k` to navigate, Enter to play.

## Troubleshooting

- **No audio after switching to Spotify** — check `~/.cache/fuga/fuga.log`
  for `librespot stop timed out`. If MPD and librespot share `default` ALSA
  device they fight; use PulseAudio/PipeWire or different ALSA devices in
  `mpd.conf` / your PA config.
- **Inline thumbs invisible** — your terminal probably isn't Kitty-capable.
  Press `T` to cycle to halfblocks (works anywhere). Verify your terminal
  with: `printf '\e_Gi=31337,s=1,v=1,a=q,t=d,f=24;AAAA\e\\'`.
- **Under tmux** — set `set -g allow-passthrough on` in `tmux.conf` and use
  tmux ≥ 3.4. Otherwise the graphics protocol is silently dropped.
- **Spotify auth fails** — delete `~/.local/share/fuga/spotify_tokens.json`
  and re-run `fuga --spotify-auth`. Confirm the redirect URI in your Spotify
  developer dashboard matches `redirect_port` in your config.
- **MPD connection error on startup** — `mpc status` to verify MPD is
  running, then `mpc update` to make sure it sees your library.
- **Phone doesn't see fuga as a Spotify Connect device** — you need a
  `client_id` configured (Spotify Web API only registers Connect devices
  through an authenticated session). Re-run `fuga --spotify-auth` if it's
  been a while since the token was issued.

## Logs

`~/.cache/fuga/fuga.log`. `fuga --debug` raises log level. The TUI never
writes to stdout (would corrupt the screen).

## Development

```sh
cargo run -- --debug              # dev
cargo test                        # unit tests
cargo clippy -- -D warnings       # lint
cargo fmt                         # format
```

See [docs/architecture.md](docs/architecture.md) for architecture, the
source-plugin trait, audio routing notes, and the phased roadmap.

## License

MIT. See [`LICENSE`](LICENSE).

## Legal / acknowledgments

**Authorship.** fuga is a hobby project. Large parts of the code were
written with AI assistance, reviewed and integrated by the author. Bug
reports and pull requests are welcome; clean rewrites of any module are
welcome too.

**Spotify.** fuga uses [librespot](https://github.com/librespot-org/librespot)
to stream from Spotify. librespot is an open-source project not approved
or endorsed by Spotify; using it outside personal/educational contexts
may violate the
[Spotify Terms of Service](https://www.spotify.com/legal/end-user-agreement/).
A Spotify Premium account is required. fuga is intended for personal use
and is not affiliated with Spotify AB.

**YouTube.** fuga shells out to a separately-installed
[`yt-dlp`](https://github.com/yt-dlp/yt-dlp) binary to search, stream, and
optionally download tracks from YouTube. fuga itself sends no traffic to
Google/YouTube; it only invokes the local `yt-dlp` binary. Users are
responsible for compliance with the
[YouTube Terms of Service](https://www.youtube.com/static?template=terms),
including any rules around downloading audio. Downloaded files land in
your MPD music directory (or `~/Downloads` as a fallback) and are intended
for personal use. fuga is not affiliated with Google LLC or YouTube.
