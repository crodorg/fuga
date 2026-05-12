#!/bin/sh
# Build fuga in release mode and install the binary + example config.
# Default prefix is ~/.local; override with PREFIX env var.

set -eu

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/fuga"

cd "$(dirname "$0")/.."

echo "==> cargo build --release"
cargo build --release

echo "==> install binary -> $BIN_DIR/fuga"
mkdir -p "$BIN_DIR"
install -m755 target/release/fuga "$BIN_DIR/fuga"

if [ ! -f "$CONFIG_DIR/config.toml" ]; then
    echo "==> install default config -> $CONFIG_DIR/config.toml"
    mkdir -p "$CONFIG_DIR"
    install -m644 examples/config.toml "$CONFIG_DIR/config.toml"
else
    echo "==> $CONFIG_DIR/config.toml already exists, leaving it alone"
fi

cat <<EOF

Done.

Next steps:
  1. Make sure mpd is running: \`mpc status\`
  2. (Optional) Configure Spotify in $CONFIG_DIR/config.toml,
     then run: fuga --spotify-auth
  3. Run: fuga

If $BIN_DIR is not in \$PATH, add it:
  export PATH="$BIN_DIR:\$PATH"
EOF
