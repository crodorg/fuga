#!/bin/sh
# Fail if any debt marker is present in tracked source. Standards live in the
# verification, not a doc an agent may ignore: unfinished work cannot be committed.
# Language-agnostic: caller passes the file globs.
# Usage: check_debt.sh <glob> [<glob> ...]
[ "$#" -ge 1 ] || { echo "usage: check_debt.sh <glob>..." >&2; exit 2; }
if git grep -nE '(TODO|FIXME|XXX|HACK)' -- "$@" >/dev/null 2>&1; then
  echo "debt: BLOCKED — debt markers present:"
  git grep -nE '(TODO|FIXME|XXX|HACK)' -- "$@"
  exit 1
fi
echo "debt: clean"
