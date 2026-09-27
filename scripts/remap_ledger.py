#!/usr/bin/env python3
"""Move coverage-ledger entries to where their lines went.

The ledger pins each exemption to a file and a line number. Editing a
file shifts every entry below the edit, and the gate then reports the
same exemption twice — once as a stale entry (its old line is covered
now) and once as an unledgered uncovered line (its new one). A large
increment produces dozens of those, and re-deriving them by hand invites
the worst outcome: pasting a plausible reason next to a line nobody
looked at.

So this walks the diff instead. Given the last commit whose coverage gate
was green, it maps old line numbers to new ones through the unchanged
regions of each file and rewrites the ledger in place. Entries whose
source line was deleted or rewritten are dropped and named, because those
are exactly the ones that need a human decision.

    python3 scripts/remap_ledger.py <base-commit>
    python3 scripts/remap_ledger.py <base-commit> --dry-run

Afterwards, re-run the gate. What it still reports is genuinely new, and
that is the list worth thinking about.
"""

import argparse
import difflib
import pathlib
import re
import subprocess
import sys

ENTRY = re.compile(r'file\s*=\s*"([^"]+)"')
# The gate accepts a comma-separated list of lines and ranges
# ("903,906,909", "796-801", "1150"); so must this, or an entry in that
# form is silently left where it was and the gate reports it as stale
# *and* reports its new line as unledgered — the exact double finding
# this script exists to prevent. That happened: nine entries in the
# ledger were comma lists, and every remap had walked past them.
LINES = re.compile(r'lines\s*=\s*"([0-9,\- ]+)"')


def parse_spec(spec: str) -> list[tuple[int, int]]:
    out = []
    for part in spec.split(","):
        part = part.strip()
        if "-" in part:
            a, b = part.split("-")
            out.append((int(a), int(b)))
        else:
            out.append((int(part), int(part)))
    return out


def render_spec(ranges: list[tuple[int, int]]) -> str:
    return ",".join(f"{a}" if a == b else f"{a}-{b}" for a, b in ranges)


def old_text(base: str, path: str) -> list[str] | None:
    r = subprocess.run(
        ["git", "show", f"{base}:{path}"], capture_output=True, text=True
    )
    return r.stdout.splitlines() if r.returncode == 0 else None


class Mapper:
    """old line number -> new line number, per file, cached."""

    def __init__(self, base: str):
        self.base = base
        self._cache: dict[str, dict[int, int] | str | None] = {}

    def _map_for(self, path: str):
        if path in self._cache:
            return self._cache[path]
        old = old_text(self.base, path)
        p = pathlib.Path(path)
        new = p.read_text().splitlines() if p.exists() else None
        if new is None:
            m = None  # the file is gone
        elif old is None:
            # The file did not exist at the base commit, so any entry for
            # it was written against the current tree and is already at
            # its final line. Dropping it would report the whole file's
            # exemptions as deleted when nothing has moved.
            m = "identity"
        elif old == new:
            m = "identity"
        else:
            m = {}
            # autojunk would treat common lines like "    }" as noise in a
            # large file and produce a wrong alignment. Off.
            sm = difflib.SequenceMatcher(a=old, b=new, autojunk=False)
            for tag, i1, i2, j1, _ in sm.get_opcodes():
                if tag == "equal":
                    for k in range(i2 - i1):
                        m[i1 + k + 1] = j1 + k + 1
        self._cache[path] = m
        return m

    def line(self, path: str, n: int) -> int | None:
        m = self._map_for(path)
        if m is None:
            return None
        if m == "identity":
            return n
        return m.get(n)


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("base", help="commit whose coverage gate was last green")
    ap.add_argument("--ledger", default="coverage-ledger.toml")
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    if subprocess.run(["git", "cat-file", "-e", f"{args.base}^{{commit}}"]).returncode:
        print(f"no such commit: {args.base}", file=sys.stderr)
        return 2

    ledger = pathlib.Path(args.ledger)
    text = ledger.read_text()
    mapper = Mapper(args.base)

    # Split on the entry header rather than parsing TOML, so comments,
    # ordering and formatting survive untouched.
    blocks = text.split("[[exempt]]")
    out = [blocks[0]]
    moved = kept = 0
    dropped: list[str] = []

    for b in blocks[1:]:
        fm, lm = ENTRY.search(b), LINES.search(b)
        if not fm or not lm:
            out.append(b)
            continue
        path, spec = fm.group(1), lm.group(1)
        old_ranges = parse_spec(spec)
        new_ranges = []
        for lo, hi in old_ranges:
            nlo, nhi = mapper.line(path, lo), mapper.line(path, hi)
            if nlo is None or nhi is None or nhi < nlo:
                new_ranges = None
                break
            new_ranges.append((nlo, nhi))
        if new_ranges is None:
            # One piece of a list gone means the whole entry needs a
            # person: the pieces share one reason, and half a reason
            # next to the surviving lines is not one anybody checked.
            dropped.append(f"{path}:{spec}")
            continue
        if new_ranges != old_ranges:
            moved += 1
        else:
            kept += 1
        out.append(b[: lm.start()] + f'lines = "{render_spec(new_ranges)}"' + b[lm.end() :])

    result = "[[exempt]]".join(out)
    if args.dry_run:
        print("(dry run — nothing written)")
    else:
        ledger.write_text(result)

    print(f"moved {moved}, unchanged {kept}, dropped {len(dropped)}")
    for d in dropped:
        print(f"  dropped: {d} — its source line was deleted or rewritten")
    if dropped:
        print("\nThose lines need looking at, not re-adding from memory.")
    print("\nNow re-run the gate; what it reports is genuinely new.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
