#!/bin/sh
# qa.sh — hardening / QA runner, layered on top of the `make check` gate.
#
# `make check` stays the canonical commit/push gate (fmt, clippy, test, file-size +
# debt caps, coverage ratchet). This script adds the heavier hardening tools on top:
# dependency audit, fuzzing, mutation testing, memory/UB checks, soak, and binary-size
# analysis. Run subcommands on demand; CI runs `qa.sh all` (the cheap, deterministic set).
#
# Quick start:
#   sh scripts/qa.sh install     # one-time, per machine
#   sh scripts/qa.sh all         # lint + test + audit (CI-safe)
#   sh scripts/qa.sh             # show this help
#
# Some subcommands need a nightly toolchain (rustup): fuzz, safety. They detect a
# missing toolchain and tell you what to install rather than failing cryptically.

set -eu

# Run from the crate root regardless of where invoked.
ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$ROOT"

FUZZ_SECS="${FUZZ_SECS:-60}"      # per-target fuzz budget (qa.sh fuzz)
SOAK_SECS="${SOAK_SECS:-120}"     # soak duration (qa.sh stress)

have() { command -v "$1" >/dev/null 2>&1; }

# Require a tool or explain how to get it; returns non-zero (caller decides).
need() {
	if have "$1"; then return 0; fi
	echo "  ! missing: $1 — run: sh scripts/qa.sh install" >&2
	return 1
}

# Require rustup+nightly for the nightly-only tools; guide if absent.
need_nightly() {
	if ! have rustup; then
		echo "  ! no rustup on this box — '$1' needs a nightly toolchain." >&2
		echo "    install rustup, then: rustup toolchain install nightly" >&2
		return 1
	fi
	return 0
}

say() { printf '\n=== %s ===\n' "$1"; }

cmd_install() {
	say "install (global cargo tools)"
	# Stable-toolchain binaries.
	for t in cargo-nextest cargo-deny cargo-machete cargo-audit \
	         cargo-bloat cargo-llvm-lines cargo-mutants cargo-modules; do
		bin="${t#cargo-}"
		if have "$t" || cargo "$bin" --version >/dev/null 2>&1; then
			echo "  ok   $t"
		else
			echo "  --   installing $t"
			cargo install --locked "$t" || echo "  ! $t failed (skipping)"
		fi
	done
	echo
	echo "  Nightly-only tools (need rustup): cargo-fuzz, cargo-careful, miri, sanitizers."
	echo "  With rustup present:"
	echo "    rustup toolchain install nightly"
	echo "    rustup component add miri rust-src --toolchain nightly"
	echo "    cargo install --locked cargo-fuzz cargo-careful"
}

cmd_lint() {
	say "lint (fmt + clippy pedantic + machete)"
	cargo fmt --check
	cargo clippy --all-targets -- -D warnings
	if need cargo-machete; then cargo machete; fi
}

cmd_test() {
	say "test"
	if have cargo-nextest; then
		cargo nextest run
	else
		echo "  (cargo-nextest not installed; using cargo test)"
		cargo test
	fi
}

cmd_audit() {
	say "audit (cargo-audit + cargo-deny)"
	if need cargo-audit; then cargo audit; fi
	if need cargo-deny; then
		[ -f deny.toml ] || { echo "  --   no deny.toml; running cargo deny init"; cargo deny init; }
		cargo deny check
	fi
}

cmd_fuzz() {
	say "fuzz (cargo-fuzz, ${FUZZ_SECS}s/target)"
	need_nightly fuzz || return 0
	need cargo-fuzz || return 0
	if [ ! -d fuzz ]; then
		echo "  ! no fuzz/ crate yet. fuga is a bin-only crate, so fuzzing needs:"
		echo "      1) a src/lib.rs exposing the parse fns (config/somafm/radio/youtube/image)"
		echo "      2) cargo fuzz init, then cargo fuzz add <target>"
		echo "    See the project hardening plan (plan.md) before adding these."
		return 0
	fi
	for t in $(cargo +nightly fuzz list 2>/dev/null); do
		echo "  -- fuzzing $t"
		cargo +nightly fuzz run "$t" -- -max_total_time="$FUZZ_SECS"
	done
}

cmd_mutants() {
	say "mutants (cargo-mutants — grades the test suite)"
	need cargo-mutants || return 0
	cargo mutants
}

cmd_safety() {
	say "safety (miri + sanitizers + careful)"
	need_nightly safety || return 0
	echo "  -- miri"
	cargo +nightly miri test || echo "  ! miri found issues (or needs: rustup component add miri)"
	echo "  -- AddressSanitizer"
	RUSTFLAGS="-Zsanitizer=address" cargo +nightly test --target "$(rustc -vV | sed -n 's/host: //p')" \
		|| echo "  ! ASan run failed/flagged"
	echo "  -- ThreadSanitizer"
	RUSTFLAGS="-Zsanitizer=thread" cargo +nightly test --target "$(rustc -vV | sed -n 's/host: //p')" \
		|| echo "  ! TSan run failed/flagged"
	if have cargo-careful; then
		echo "  -- cargo-careful"; cargo +nightly careful test || echo "  ! careful flagged"
	fi
}

cmd_stress() {
	say "stress (soak ${SOAK_SECS}s + RSS watch)"
	cargo build --release
	bin="$ROOT/target/release/fuga"
	echo "  NOTE: fuga probes the terminal at startup (term_probe.rs) and blocks in"
	echo "        n_tty_read with no attached terminal. A meaningful TUI soak needs a"
	echo "        real terminal or a PTY harness (rexpect / ratatui-testlib, wave 4)."
	echo "        SIGWINCH/resize fuzzing: drive it under tmux with a 'resize-pane' loop."
	if ! have script; then
		echo "  ! 'script' (util-linux) not found — cannot allocate a PTY here. Skipping soak."
		return 0
	fi
	echo "  -- best-effort PTY soak (may exit early if services/terminal unavailable)"
	( script -qc "$bin" /dev/null >/dev/null 2>&1 & echo $! >/tmp/fuga-soak.pid ) || true
	pid=$(cat /tmp/fuga-soak.pid 2>/dev/null || echo "")
	[ -n "$pid" ] || { echo "  ! could not start soak"; return 0; }
	i=0; max=0
	while [ "$i" -lt "$SOAK_SECS" ]; do
		if ! kill -0 "$pid" 2>/dev/null; then
			echo "  soak process exited at ${i}s (expected without a real terminal)"; break
		fi
		rss=$(ps -o rss= -p "$pid" 2>/dev/null | tr -d ' ')
		if [ -n "${rss:-}" ] && [ "$rss" -gt "$max" ] 2>/dev/null; then max=$rss; fi
		i=$((i+2)); sleep 2
	done
	kill "$pid" 2>/dev/null || true
	if [ "$max" -gt 0 ] 2>/dev/null; then echo "  peak RSS: $((max/1024)) MiB"; fi
}

cmd_min() {
	say "min (bloat + llvm-lines + modules)"
	if need cargo-bloat; then cargo bloat --release --crates | head -25; fi
	if need cargo-llvm-lines; then cargo llvm-lines | head -25; fi
	if need cargo-modules; then
		cargo modules structure 2>/dev/null || cargo modules generate tree 2>/dev/null \
			|| echo "  ! cargo-modules CLI shape differs; run it manually"
	fi
}

cmd_gate() {
	say "gate (delegating to make check — the canonical project gate)"
	make check
}

cmd_all() {
	# CI-safe set: the project gate + dependency audit. Long/interactive tools
	# (fuzz, mutants, safety, stress) are run on demand, not here.
	cmd_gate
	cmd_audit
}

usage() {
	cat <<'EOF'
qa.sh — hardening / QA runner (layered on `make check`)

  install   cargo install the global tools (once per machine)
  lint      fmt --check + clippy pedantic + cargo-machete
  test      cargo-nextest run (falls back to cargo test)
  audit     cargo-audit + cargo-deny                  [security: supply chain]
  fuzz      run cargo-fuzz targets (FUZZ_SECS=60)     [security: input surface]
  mutants   cargo-mutants — grade the test suite
  safety    miri + sanitizers + cargo-careful         [security: memory/UB]
  stress    soak + RSS watch (SOAK_SECS=120)
  min       cargo-bloat + llvm-lines + modules
  gate      make check (the existing commit/push gate)
  all       CI-safe: gate + audit

  fuzz/safety need rustup+nightly; fuzz also needs a src/lib.rs (fuga is bin-only).
EOF
}

case "${1:-help}" in
	install) cmd_install ;;
	lint)    cmd_lint ;;
	test)    cmd_test ;;
	audit)   cmd_audit ;;
	fuzz)    cmd_fuzz ;;
	mutants) cmd_mutants ;;
	safety)  cmd_safety ;;
	stress)  cmd_stress ;;
	min)     cmd_min ;;
	gate)    cmd_gate ;;
	all)     cmd_all ;;
	help|-h|--help) usage ;;
	*) echo "unknown subcommand: $1" >&2; usage; exit 2 ;;
esac
