#!/bin/sh
# Measure line coverage with cargo-tarpaulin and enforce (or --bump) the ratchet floor.
# tarpaulin is used instead of cargo-llvm-cov because the box has no rustup/llvm-tools.
set -e
bump=""
[ "$1" = "--bump" ] && bump="--bump"

if ! command -v cargo-tarpaulin >/dev/null 2>&1; then
  echo "coverage: cargo-tarpaulin not installed (cargo install cargo-tarpaulin)" >&2
  exit 1
fi

# --test-threads=1: some tests mutate process-global env (std::env::set_var), which can
# race under tarpaulin's default parallel scheduling. Serializing keeps the measurement
# stable. (cargo test stays parallel and is stable there.)
out=$(cargo tarpaulin --skip-clean --out Stdout -- --test-threads=1 2>&1) || {
  echo "coverage: tarpaulin failed:" >&2
  printf '%s\n' "$out" | tail -15 >&2
  exit 1
}

# tarpaulin prints a summary line like: "84.21% coverage, 123/146 lines covered"
pct=$(printf '%s\n' "$out" | grep -oE '[0-9]+\.[0-9]+% coverage' | tail -1 | grep -oE '[0-9]+\.[0-9]+')
if [ -z "$pct" ]; then
  echo "coverage: could not parse tarpaulin output:" >&2
  printf '%s\n' "$out" | tail -10 >&2
  exit 1
fi

exec python3 scripts/ratchet.py coverage-floor.txt "$pct" $bump
