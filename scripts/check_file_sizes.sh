#!/bin/sh
# Fail if any tracked source file exceeds the line cap. Large files blow an agent's
# context window, so keep functionality split across small files. Grandfathered paths
# live in .gate-allow (one path per line) — flagged each run as refactor debt, never
# blocking. Language-agnostic: caller passes the cap and the file globs.
# Usage: check_file_sizes.sh <cap> <glob> [<glob> ...]
cap="$1"
shift || true
[ -n "$cap" ] && [ "$#" -ge 1 ] || { echo "usage: check_file_sizes.sh <cap> <glob>..." >&2; exit 2; }
allowfile=".gate-allow"

fail=0
for f in $(git ls-files "$@"); do
  n=$(wc -l < "$f")
  if [ -f "$allowfile" ] && grep -qxF "$f" "$allowfile"; then
    [ "$n" -gt "$cap" ] && echo "sizes: GRANDFATHERED $f ($n lines > $cap) — refactor debt"
    continue
  fi
  if [ "$n" -gt "$cap" ]; then
    echo "sizes: BLOCKED — $f is $n lines (cap $cap). Split it before committing."
    fail=1
  fi
done

[ "$fail" -eq 0 ] && echo "sizes: all non-grandfathered files <= $cap lines"
exit "$fail"
