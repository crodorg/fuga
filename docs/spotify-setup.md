# fuga + Spotify setup

`cat docs/spotify-setup.md` (or just read it on GitHub).

This walks you from zero to a working Spotify backend inside fuga.
You need a **Spotify Premium** account — the Connect / Web Playback
APIs fuga uses are Premium-only.

## 1. Register a Spotify Developer app

Spotify gates Web API access behind a developer-app `client_id`. The
app is just an identity, not a paid product — anyone with a Spotify
account can register one.

1. Open <https://developer.spotify.com/dashboard> in a browser.
2. Log in with the same Spotify account you want fuga to play from.
3. Click **Create app**. Name + description don't matter — pick
   anything fuga-ish (e.g. `fuga (local)`).
4. Under **Redirect URIs**, add exactly:

   ```
   http://127.0.0.1:8888/callback
   ```

   (If you customized `[spotify] redirect_port` in your fuga config,
   substitute that port number.) The URI **must** match what fuga
   passes to the OAuth endpoint or Spotify refuses the redirect.
5. Under **APIs used**, tick **Web API** (and **Web Playback SDK**
   if it's offered — neither is strictly required but enabling them
   surfaces the right docs).
6. Save. On the app's settings page, click **View client ID** and
   copy the long alphanumeric string.

## 2. Configure fuga

Open `~/.config/fuga/config.toml` (create it if missing — fuga
ships `examples/config.toml` as a template). Set:

```toml
[spotify]
enabled = true
client_id = "the-id-you-just-copied"
```

Optional knobs:

- `quality = "lossless"` (default) — fuga negotiates the highest
  stream tier your account supports.
- `device_name = "fuga"` — what other Spotify Connect clients see
  fuga as. Pick any string.
- `redirect_port = 8888` — only change if 8888 is taken on your
  machine; remember to update the dashboard's redirect URI to
  match.

## 3. Run the one-shot auth flow

```sh
fuga --spotify-auth
```

This authorizes **two** things, in order: your app for the Web API
(browsing, search, playlists) and the audio session itself. They are
separate because Spotify's playback handshake only accepts credentials
issued to its own desktop client — a third-party app id can browse, but
it can no longer stream.

What happens:

1. fuga prints an authorize URL and opens it in your default browser
   (`xdg-open` on Linux, `open` on macOS).
2. Spotify shows the consent screen listing the scopes fuga needs
   (library read/modify, playback control, playlist read/modify,
   recently-played, top tracks). Click **Agree**.
3. Spotify redirects to `http://127.0.0.1:8888/callback?code=...`.
   fuga's local listener catches it, exchanges the code for an
   access + refresh token, writes the pair to
   `~/.local/share/fuga/spotify_tokens.json` (mode 0600).
4. The browser tab shows "fuga: auth complete; close this tab."
5. A **second** authorize page opens, this time for the playback
   session (redirect `http://127.0.0.1:8898/login`). Approve it too.
   fuga stores the reusable session credential it returns in
   `~/.local/share/fuga/librespot/credentials.json` (mode 0600), then
   exits.

You only do this once. From then on `fuga` (no flag) auto-loads the
cached token and refreshes it as needed; the playback credential does
not expire.

## 4. Verify

Launch fuga, then either:

- Press `t` until the source indicator says `spotify`, or
- Set `[ui.tabs]` with `spotify = [...]` as the first key in your
  config so fuga boots into Spotify mode (see README).

The Library tab should populate within ~1 second (cold cache may
take longer; you'll see the loading dots on the right edge of the
view header).

## Troubleshooting

**"INVALID_CLIENT: Invalid redirect URI"** during the browser
consent step. The dashboard's redirect URI doesn't exactly match
what fuga sent. Verify you added `http://127.0.0.1:8888/callback`
(http, NOT https; 127.0.0.1, NOT localhost) and saved the change.

**"address already in use" when fuga's local listener binds.**
Another process holds port 8888. Either kill it (`lsof -i :8888`)
or change `[spotify] redirect_port` AND the dashboard's redirect
URI to a free port. Both must match.

**`fuga` launches but tabs say "Spotify not authed".** Token cache
missing or unreadable. Re-run `fuga --spotify-auth` to recreate it.
The cache lives at `$XDG_DATA_HOME/fuga/spotify_tokens.json`
(usually `~/.local/share/fuga/`).

**"Spotify playback not authed" / "Spotify playback auth rejected —
run `fuga --spotify-auth`".** Browsing works but tracks won't load:
the playback credential (`$XDG_DATA_HOME/fuga/librespot/credentials.json`)
is missing, or Spotify rejected it because access was revoked. Re-run
`fuga --spotify-auth` and approve the second browser prompt.

**Playback transfer fails / "no active device".** Spotify Connect
requires an active device. With Premium you can transfer from the
official client or use `d` inside fuga to list devices.

**`bitrate` warning at startup** — older configs used numeric
`bitrate = 320`. Fuga still parses it but the canonical key is
`quality`. Switch to `quality = "lossless"` / `"high"` / `"normal"`
/ `"low"`.

## Scopes fuga requests

For transparency, every scope is consented to during step 3:

| Scope                            | Why fuga needs it                |
|----------------------------------|----------------------------------|
| `streaming`                      | Play audio via Spotify Connect   |
| `user-library-read`              | Saved Albums / Liked Songs tabs  |
| `user-library-modify`            | `F` to like/unlike               |
| `user-read-playback-state`       | Knowing what's playing           |
| `user-modify-playback-state`     | Play/pause/seek/transfer         |
| `user-read-currently-playing`    | Now-playing display              |
| `user-read-recently-played`      | "Recently Played" view           |
| `user-top-read`                  | "Top Tracks" / "Top Artists"     |
| `user-follow-read`               | "Followed Artists"               |
| `playlist-read-private`          | Your private playlists           |
| `playlist-read-collaborative`    | Collab playlists you're in       |
| `playlist-modify-public`         | Add-to-playlist (`m` action)     |
| `playlist-modify-private`        | Same, for private playlists      |

Revoking any of these at <https://www.spotify.com/account/apps>
breaks the corresponding feature; fuga handles 403s by toasting an
error rather than crashing.
