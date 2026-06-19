#!/bin/sh
# record-demo.sh - hands-free demo recording of fuga in the CURRENT terminal.
#
# Runs fuga in the same terminal that invoked this script. A background
# driver feeds keystrokes via xdotool while ffmpeg screen-captures the
# terminal window, then converts to optimized GIF.
#
# Output: docs/demo.gif (~35s, target <5MB)
#
# Deps: fuga (on PATH or built), xdotool, ffmpeg, awk
# X11 only. Must be run from inside the terminal you want recorded
# (uses $WINDOWID, which st/xterm/urxvt/foot-x11 all set).

set -eu

cd "$(dirname "$0")/.."
REPO_ROOT="$(pwd)"

# ---- config ----
FPS_RECORD=30
FPS_OUT=18
# SCALE_W=0 means output at native capture resolution (sharper text).
# Set to a positive width (e.g. 1000) to downscale.
SCALE_W=0
DURATION_S=55
FUGA_BIN="${FUGA_BIN:-fuga}"
OUT_MP4="$REPO_ROOT/docs/demo.mp4"
OUT_GIF="$REPO_ROOT/docs/demo.gif"
PALETTE="$(mktemp --suffix=.png)"

# ---- preflight ----
for cmd in xdotool ffmpeg awk; do
    command -v "$cmd" >/dev/null 2>&1 || { echo "missing dep: $cmd" >&2; exit 1; }
done
command -v "$FUGA_BIN" >/dev/null 2>&1 || {
    if [ -x "$REPO_ROOT/target/release/fuga" ]; then
        FUGA_BIN="$REPO_ROOT/target/release/fuga"
    elif [ -x "$REPO_ROOT/fuga-v0.1.0-linux-x86_64" ]; then
        FUGA_BIN="$REPO_ROOT/fuga-v0.1.0-linux-x86_64"
    else
        echo "fuga binary not found. set FUGA_BIN or run 'cargo build --release'" >&2
        exit 1
    fi
}
[ "${DISPLAY:-}" ] || { echo "DISPLAY unset. X11 only." >&2; exit 1; }
[ "${WINDOWID:-}" ] || {
    echo "WINDOWID unset. Run this from inside an st/xterm/urxvt/foot-x11 window." >&2
    exit 1
}

WIN_ID="$WINDOWID"

# ---- cleanup ----
FFMPEG_PID=""
DRIVER_PID=""
cleanup() {
    [ -n "$DRIVER_PID" ] && kill "$DRIVER_PID" 2>/dev/null || true
    [ -n "$FFMPEG_PID" ] && kill -INT "$FFMPEG_PID" 2>/dev/null || true
    rm -f "$PALETTE"
}
trap cleanup EXIT INT TERM

# ---- focus the recorded window ----
# A backgrounded launch (spawned from another terminal) often opens unfocused,
# so the keystroke driver's events would land in the wrong window. Grab focus
# and park the pointer over the window (covers focus-follows-mouse WMs like
# dwm). Window *size* is left to the WM/terminal — under a tiling WM, resize
# the tile (or go fullscreen/monocle) before recording for a larger capture.
xdotool windowactivate --sync "$WIN_ID" 2>/dev/null || true
xdotool mousemove --window "$WIN_ID" 20 20 2>/dev/null || true
sleep 0.4

# ---- window geometry ----
eval "$(xdotool getwindowgeometry --shell "$WIN_ID")"
# Inset by st's 2px internal border so the recording is content-only.
BORDER=2
X=$(( X + BORDER ))
Y=$(( Y + BORDER ))
WIDTH=$(( WIDTH - 2 * BORDER ))
HEIGHT=$(( HEIGHT - 2 * BORDER ))
WIDTH=$(( WIDTH - WIDTH % 2 ))
HEIGHT=$(( HEIGHT - HEIGHT % 2 ))
echo "==> recording window $WIN_ID: ${WIDTH}x${HEIGHT} at +${X},${Y}"

# ---- start ffmpeg in background ----
ffmpeg -y -loglevel error \
    -video_size "${WIDTH}x${HEIGHT}" -framerate "$FPS_RECORD" \
    -f x11grab -i "${DISPLAY}+${X},${Y}" \
    -t "$DURATION_S" \
    -c:v libx264 -preset ultrafast -pix_fmt yuv420p \
    "$OUT_MP4" &
FFMPEG_PID=$!

# ---- background keystroke driver ----
# Sends to whatever window is focused; relies on the script invoker keeping
# the recorded window focused (it will be, since fuga runs in foreground here).
(
    sleep 3.0   # wait for fuga splash / MPD connect / Spotify session + browse fetch

    # `t` (cycle_source) steps through *registered* sources only, in canonical
    # order Local -> Spotify -> YouTube -> SomaFM -> Radio -> wrap. We don't
    # count presses to land anywhere: after a quick spin we jump straight to
    # Spotify with the `g`+`s` leader (source_jump:spotify), which is exact no
    # matter how many sources are registered.

    # --- 1. Quick source spin: flip the ring so each source flashes by ---
    sleep 1.0                       # linger on the default source at launch
    xdotool key t; sleep 0.9        # -> next source
    xdotool key t; sleep 0.9
    xdotool key t; sleep 0.9
    xdotool key t; sleep 0.9
    xdotool key t; sleep 0.9        # 5 presses covers the full ring (max 5)

    # --- 2. Land on Spotify for the showcase (deterministic) ---
    xdotool key g; sleep 0.15; xdotool key s
    sleep 2.0                       # Spotify landing page loads, thumbs per row

    # Spotify landing page sections: Discover Weekly / Liked Songs / Recently
    # Played / Top Tracks / Top Artists — every row carries an album-art thumb.
    xdotool key --delay 220 j
    xdotool key --delay 220 j
    xdotool key --delay 220 j
    sleep 1.0
    # Back to the top (Discover Weekly), then down one to Liked Songs so the
    # descent is deterministic.
    xdotool key g; sleep 0.12; xdotool key g    # gg -> top (Discover Weekly)
    sleep 0.4
    xdotool key j                                # -> Liked Songs (row 2)
    sleep 0.6

    # --- 3. Descend into Liked Songs (a wall of album-art rows) ---
    xdotool key Return
    sleep 1.8
    # Move to the 14th track (cursor lands on row 1 on descent) — that song has
    # synced lyrics, unlike the EDM up top. 13 presses = row 14.
    xdotool key --delay 160 j j j j j j j j j j j j j
    sleep 0.8

    # --- 4. Play it: now-playing fills with large art + metadata ---
    xdotool key Return
    sleep 3.0

    # --- 5. Resize the now-playing art panel (e): collapse it into the
    #        bottom bar, then expand back to full size. The expand-back is
    #        the path that used to render blank — now it repaints. ---
    xdotool key e
    sleep 2.0          # collapsed: art shrinks into the bottom bar
    xdotool key e
    sleep 2.5          # expanded: full-size cover repaints

    # --- 6. Synced lyrics: open, then linger while they load and scroll ---
    xdotool key B
    sleep 13.0         # lyrics are slow to load; linger long enough to read them
    xdotool key Escape
    sleep 0.6

    # --- 7. Expand album art to fill the screen (v, lowercase) ---
    xdotool key v
    sleep 4.0
    xdotool key v               # toggle back
    sleep 0.6

    # --- 8. Spotify Connect device picker (d) ---
    xdotool key d
    sleep 2.2
    xdotool key Escape
    sleep 0.6

    # --- 9. Toggle inline thumbnails off, then on (T) ---
    xdotool key T
    sleep 2.0
    xdotool key T
    sleep 1.0

    # Stop the recorder BEFORE quitting fuga, so the shell prompt is never
    # captured, and WAIT for ffmpeg to fully flush the mp4 moov atom before
    # quitting. Quitting fuga can crash a graphics terminal (sixel/Kitty st
    # fork) on teardown; if that happens before ffmpeg has flushed, the mp4
    # trailer is lost and the file is unreadable. Polling kill -0 until
    # ffmpeg exits guarantees a valid mp4 regardless of the terminal dying.
    kill -INT "$FFMPEG_PID" 2>/dev/null || true
    i=0
    while kill -0 "$FFMPEG_PID" 2>/dev/null && [ "$i" -lt 60 ]; do
        sleep 0.2
        i=$((i + 1))
    done

    # Quit fuga.
    xdotool key q
) &
DRIVER_PID=$!

# ---- run fuga in this terminal (foreground) ----
# When the driver sends 'q', fuga quits and control returns here.
"$FUGA_BIN" || true

# Stop ffmpeg the instant fuga exits so the gif doesn't trail empty frames.
# SIGINT lets libx264 flush its trailer cleanly (SIGTERM/KILL corrupts mp4).
if [ -n "$FFMPEG_PID" ]; then
    kill -INT "$FFMPEG_PID" 2>/dev/null || true
    wait "$FFMPEG_PID" 2>/dev/null || true
    FFMPEG_PID=""
fi

# Driver self-terminates after sending 'q'; reap if still around.
wait "$DRIVER_PID" 2>/dev/null || true
DRIVER_PID=""

[ -s "$OUT_MP4" ] || { echo "ffmpeg produced empty mp4" >&2; exit 1; }

# ---- mp4 -> gif ----
if [ "$SCALE_W" -gt 0 ]; then
    SCALE_FILTER="scale=${SCALE_W}:-1:flags=lanczos,"
else
    SCALE_FILTER=""
fi

echo "==> generating palette..."
ffmpeg -y -loglevel error -i "$OUT_MP4" \
    -vf "fps=${FPS_OUT},${SCALE_FILTER}palettegen=max_colors=256:stats_mode=full" \
    "$PALETTE"

echo "==> encoding gif..."
ffmpeg -y -loglevel error -i "$OUT_MP4" -i "$PALETTE" \
    -lavfi "fps=${FPS_OUT},${SCALE_FILTER}format=rgb24[x];[x][1:v]paletteuse=dither=bayer:bayer_scale=5" \
    "$OUT_GIF"

SIZE=$(stat -c%s "$OUT_GIF")
SIZE_MB=$(awk "BEGIN{printf \"%.2f\", $SIZE/1048576}")
echo "==> done: $OUT_GIF (${SIZE_MB} MB)"

if [ "$SIZE" -gt 5242880 ]; then
    echo "    WARNING: >5MB. Lower DURATION_S, FPS_OUT, or SCALE_W." >&2
fi
