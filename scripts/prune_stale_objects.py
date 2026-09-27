#!/usr/bin/env python3
"""Remove test binaries from a cargo target tree that no target owns any more.

`cargo llvm-cov` builds every executable in `target/llvm-cov-target/debug/deps`
into the report, and it decides which ones belong to the workspace by
*package* name. Cargo never deletes an artifact it stops producing, so
renaming a `[[bin]]` — `stratum-runner` became `weft-runner` — leaves the
old target's test binary in `deps`, still matching its package, carrying
the *old* source's line map, and with no profraw behind it because it was
never run. The report then says a doc comment on line 588 of `spec.rs` is
an executable line nobody executed, and the gate fails on a machine that
has seen both names while CI — whose cache never met the old one — is
green. One sha, two verdicts, and the difference is this file.

Usage:
    prune_stale_objects.py <deps-dir> <target-name>...

Deletes every executable in <deps-dir> whose stem — the name before the
trailing `-<metadata hash>` — is not one of the given target names (with
`-` folded to `_`, as cargo spells file names). Everything else is left
alone: `.d`, `.rlib`, `.rmeta`, incremental dirs, and any file whose name
does not have cargo's `<stem>-<16 hex>` shape. Prints what it removed.
"""

import os
import re
import stat
import sys

ARTIFACT = re.compile(r"^(?P<stem>[A-Za-z0-9_]+)-(?P<hash>[0-9a-f]{16})$")


def stale_executables(deps, targets):
    """Executables in `deps` whose stem names no current target."""
    wanted = {t.replace("-", "_") for t in targets}
    out = []
    for name in sorted(os.listdir(deps)):
        m = ARTIFACT.match(name)
        if not m or m.group("stem") in wanted:
            continue
        path = os.path.join(deps, name)
        st = os.lstat(path)
        if not stat.S_ISREG(st.st_mode) or not st.st_mode & stat.S_IXUSR:
            continue
        out.append(path)
    return out


def main(argv):
    if len(argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    deps, targets = argv[1], argv[2:]
    if not os.path.isdir(deps):
        return 0
    for path in stale_executables(deps, targets):
        os.remove(path)
        print(f"pruned stale test binary {path}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
