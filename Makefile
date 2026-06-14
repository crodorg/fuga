# Agent-environment check gate — Rust profile. Installed by ~/.claude/gate/install.sh.
# The codebase verifies itself so agents (and you) cannot commit code that breaks it.
#   make check       full gate — CI runs exactly this (CI parity)
#   make check-fast  fast deterministic checks — run on every commit (pre-commit hook)
#   make check-cov   coverage ratchet — run on push / CI (slower; needs cargo-tarpaulin)

RS_CAP := 800
GLOBS  := *.rs

.PHONY: check check-fast check-cov fmt clippy test sizes debt cov ratchet-bump

check: check-fast check-cov

check-fast: fmt clippy test sizes debt

check-cov: cov

fmt:
	cargo fmt --check

clippy:
	cargo clippy --all-targets -- -D warnings

test:
	cargo test --quiet

sizes:
	sh scripts/check_file_sizes.sh $(RS_CAP) $(GLOBS)

debt:
	sh scripts/check_debt.sh $(GLOBS)

cov:
	sh scripts/coverage.sh

ratchet-bump:
	sh scripts/coverage.sh --bump
