#!/bin/sh
# record-demo.sh - hands-free demo recording of fuga in the CURRENT terminal.
#
# Runs fuga in the same terminal that invoked this script. A background
# driver feeds keystrokes via xdotool while ffmpeg screen-captures the
# terminal window, then converts to optimized GIF.
#
# Output: docs/demo.gif (~15s, target <5MB)
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
DURATION_S=44
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
    sleep 2.5   # wait for fuga splash / MPD connect / Spotify session

    # Mode switch keybind: 't' cycles through sources.
    # Assumed cycle order: Local -> Spotify -> YouTube -> SomaFM -> Radio -> wrap.
    # If real order differs, adjust 't' counts below.

    # --- 1. Local (default) ---
    xdotool key --delay 150 j
    xdotool key --delay 150 j
    xdotool key --delay 150 j
    xdotool key --delay 150 j
    sleep 1.2

    # --- 2. Spotify ---
    xdotool key t
    sleep 1.2
    xdotool key --delay 150 j
    xdotool key --delay 150 j
    xdotool key --delay 150 j
    sleep 1.0

    # --- 3. YouTube ---
    xdotool key t
    sleep 1.2
    xdotool key --delay 150 j
    xdotool key --delay 150 j
    sleep 1.0

    # --- 4. SomaFM ---
    xdotool key t
    sleep 1.2
    xdotool key --delay 150 j
    xdotool key --delay 150 j
    xdotool key --delay 150 j
    sleep 1.2

    # --- 5. Back to Spotify (ring is 4 sources, wraps SomaFM -> Local) ---
    xdotool key t          # SomaFM -> Local
    sleep 0.6
    xdotool key t          # Local -> Spotify
    sleep 1.2

    # --- 6. Spotify: jump to Followed Artists ---
    # Spotify sub-view shortcut: `g` then letter (gs=saved, gp=playlists, ga=artists).
    xdotool key g; sleep 0.15; xdotool key a
    sleep 1.8

    # Scroll down a couple of artists.
    xdotool key --delay 200 j
    xdotool key --delay 200 j
    sleep 0.8

    # Enter the artist.
    xdotool key Return
    sleep 1.5

    # Select the top section (Top Tracks).
    xdotool key Return
    sleep 1.2

    # Pick a track and play it.
    xdotool key --delay 200 j
    sleep 0.4
    xdotool key Return
    sleep 3.0              # linger on now-playing while track starts

    # --- 7. Device selector ---
    xdotool key d
    sleep 2.5
    xdotool key Escape
    sleep 0.8

    # --- 8. Expand album art ---
    xdotool key V               # shift+v expands album art
    sleep 4.5              # linger with big art on screen

    # Close the big-art view.
    xdotool key V
    sleep 0.8

    # Toggle thumb mode once to demo no-thumbnail mode.
    xdotool key shift+t
    sleep 2.5              # linger so viewer sees the thumb-less list

    # Stop the recorder BEFORE quitting fuga, so the shell prompt is
    # never captured. SIGINT lets libx264 flush the mp4 trailer cleanly.
    kill -INT "$FFMPEG_PID" 2>/dev/null || true

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
