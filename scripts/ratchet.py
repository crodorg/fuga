#!/usr/bin/env python3
"""Coverage ratchet: the floor can only go up. Language-agnostic.

Usage: ratchet.py <floor-file> <pct> [--bump]

Reads the stored floor and the freshly measured percentage. Exits non-zero if the
measurement dropped below the floor. With --bump, raises the floor to the new
measurement (and never lowers it). Stdlib only — no dependencies.
"""

import pathlib
import sys


def main() -> int:
    argv = sys.argv[1:]
    bump = "--bump" in argv
    args = [a for a in argv if a != "--bump"]
    if len(args) != 2:
        print("usage: ratchet.py <floor-file> <pct> [--bump]", file=sys.stderr)
        return 2

    floor_file, pct_str = args
    pct = float(pct_str)
    path = pathlib.Path(floor_file)
    raw = path.read_text().strip() if path.exists() else ""
    floor = float(raw) if raw else 0.0

    # tiny epsilon so 90.00 vs 90.00 floating noise never falsely blocks
    if pct + 1e-9 < floor:
        print(
            f"coverage: BLOCKED — {pct:.2f}% is below the floor of {floor:.2f}%",
            file=sys.stderr,
        )
        return 1

    if bump and pct > floor:
        path.write_text(f"{pct:.2f}\n")
        print(f"coverage: floor ratcheted {floor:.2f}% -> {pct:.2f}%")
        return 0

    print(f"coverage: {pct:.2f}% >= floor {floor:.2f}% OK")
    return 0


if __name__ == "__main__":
    sys.exit(main())
