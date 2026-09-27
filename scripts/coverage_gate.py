#!/usr/bin/env python3
"""Coverage gate: every product line is either executed by the test suite
or explicitly justified in coverage-ledger.toml — where a justification
means one of: unreachable by construction, not deterministically
triggerable from outside the process, or an equivalence-class member
whose behavior is pinned by sibling tests. Both directions are
enforced — an unledgered uncovered line fails the build, and a ledger
entry whose lines have become covered (or moved) fails as stale, so the
ledger can only shrink truthfully.

Usage: coverage_gate.py <lcov-file> <ledger-toml>
"""

import re
import sys
import tomllib

# Test infrastructure is not product code; its own error paths (download
# fallbacks, harness edge cases) are not part of the product contract.
EXEMPT_FILE_PATTERNS = [re.compile(r"crates/stratum-testkit/")]


def parse_lcov(path):
    uncovered, totals = {}, {}
    current = None
    for raw in open(path):
        line = raw.strip()
        if line.startswith("SF:"):
            sf = line[3:]
            m = re.search(r"crates/.*$", sf)
            current = m.group(0) if m else sf
            uncovered.setdefault(current, set())
            totals.setdefault(current, [0, 0])
        elif line.startswith("DA:") and current is not None:
            n, h = line[3:].split(",")[:2]
            totals[current][0] += 1
            if int(h) == 0:
                uncovered[current].add(int(n))
            else:
                totals[current][1] += 1
    return uncovered, totals


def parse_ranges(spec):
    out = []
    for part in str(spec).split(","):
        part = part.strip()
        if "-" in part:
            a, b = part.split("-")
            out.append((int(a), int(b)))
        else:
            out.append((int(part), int(part)))
    return out


def main():
    lcov_path, ledger_path = sys.argv[1], sys.argv[2]
    uncovered, totals = parse_lcov(lcov_path)
    # A ledger that will not parse is a gate that reports a tomllib
    # traceback instead of a finding — which is how one editing mistake
    # in a 2,000-line file cost a whole 20-minute gate run to diagnose.
    # Say where, and say it in the gate's own voice.
    try:
        ledger = tomllib.load(open(ledger_path, "rb"))
    except tomllib.TOMLDecodeError as e:
        print(f"COVERAGE GATE FAILED:\n- {ledger_path} is not valid TOML: {e}")
        print(
            "  (a stray or missing [[exempt]] header is the usual cause — "
            "each entry needs its own)"
        )
        sys.exit(1)

    ledgered = {}
    problems = []
    for n, entry in enumerate(ledger.get("exempt", []), start=1):
        # A structurally broken entry used to surface as a bare KeyError
        # traceback, which says nothing about which entry or what is
        # wrong with it — and costs a whole gate run to find out. The
        # usual cause is a doubled or orphaned `[[exempt]]` header, so
        # name that.
        if "file" not in entry or "lines" not in entry:
            problems.append(
                f"ledger entry #{n} is missing `file` or `lines` ({entry!r}) — "
                "a doubled or orphaned [[exempt]] header is the usual cause"
            )
            continue
        f, spec = entry["file"], entry["lines"]
        if not entry.get("reason", "").strip():
            problems.append(f"ledger entry {f}:{spec} has no reason")
        flaky = bool(entry.get("flaky", False))
        ledgered.setdefault(f, []).append((parse_ranges(spec), spec, flaky))

    # Direction 1: every uncovered line is ledgered.
    unexplained = []
    for f, lines in sorted(uncovered.items()):
        if any(p.search(f) for p in EXEMPT_FILE_PATTERNS):
            continue
        ranges = [r for rs, _, _ in ledgered.get(f, []) for r in rs]
        for n in sorted(lines):
            if not any(a <= n <= b for a, b in ranges):
                unexplained.append(f"{f}:{n}")
    if unexplained:
        # The count is in the header and always has been. The listing is
        # capped at 60 so a bad run does not bury the rest of the
        # report — but say so where the listing ends, not only where it
        # starts. A reader who pipes this through grep sees the tail
        # without the header, concludes the list is the total, fixes
        # sixty lines and finds sixty more. That happened, three times,
        # to somebody who then blamed the script.
        shown = unexplained[:60]
        more = len(unexplained) - len(shown)
        listing = "\n  ".join(shown)
        if more:
            listing += f"\n  … and {more} more (listing capped at 60)"
        problems.append(
            f"{len(unexplained)} uncovered line(s) not in the ledger:\n  " + listing
        )

    # Direction 2: every non-flaky ledger range still holds an uncovered
    # line. Entries marked flaky = true cover lines whose execution varies
    # with environment (git/toolchain-dependent delta shapes, llvm line
    # attribution): they exempt when uncovered and are never stale.
    for f, entries in sorted(ledgered.items()):
        actual = uncovered.get(f, set())
        for rs, spec, flaky in entries:
            if flaky:
                continue
            if not any(a <= n <= b for a, b in rs for n in actual):
                problems.append(f"stale ledger entry {f}:{spec} — lines are covered now, prune it")

    prod_total = prod_cov = prod_ledgered = 0
    for f, (t, c) in totals.items():
        if any(p.search(f) for p in EXEMPT_FILE_PATTERNS):
            continue
        prod_total += t
        prod_cov += c
        ranges = [r for rs, _, _ in ledgered.get(f, []) for r in rs]
        prod_ledgered += sum(1 for n in uncovered.get(f, ()) if any(a <= n <= b for a, b in ranges))

    denom = prod_total - prod_ledgered
    pct = 100.0 * prod_cov / denom if denom else 100.0
    print(f"product lines: {prod_total}  covered: {prod_cov}  ledgered: {prod_ledgered}")
    print(f"effective coverage (covered / coverable): {pct:.2f}%")

    if problems:
        print("\nCOVERAGE GATE FAILED:")
        for p in problems:
            print(f"- {p}")
        sys.exit(1)
    if pct < 100.0:
        print("\nCOVERAGE GATE FAILED: effective coverage below 100%")
        sys.exit(1)
    print("coverage gate: OK — 100% of coverable lines executed, ledger exact")


if __name__ == "__main__":
    main()
