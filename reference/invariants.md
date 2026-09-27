# Correctness invariants

These are the load-bearing properties. Every one was earned — most by a
concrete failure during the research (the ledger in `docs/HANDOFF.md`
maps invariants to the experiments that forced them). A future
implementation can change any format or component freely **as long as
these still hold**; violating one silently produces corrupt clones,
torn reads, or wrong refs, usually far from the code that broke it.

Each entry: the invariant, who relies on it, and what breaks.

## Stream composition

**I1 — A served stream is `header + verbatim byte ranges + trailer`.**
The clone/fetch path never parses or re-encodes pack entries; it only
chooses ranges. Everything below exists to make that safe. Relied on
by: the entire latency result (zero request-time delta work).

**I2 — Cold-segment deltas never cross a segment boundary.**
Cold segments use OFS deltas only, and every base lies in the same
segment (H1, amended to path-major order). Breaks: an OFS distance
pointing outside the streamed range → client `index-pack` fails, or
worse, lands on a wrong entry in a differently-composed stream.
Enforced at ingest (`pack_segment_full` packs each segment
independently); checked by the fsck gate.

**I3 — Prefix closure: every REF_DELTA base precedes its delta in the
global stream order.** Global order = cold segments, then hot
emissions in spine order, then `extra_emission`, then WAL entries in
push order. Hot emissions are packed with an explicit seen-set +
`^oid` exclusions so they delta only against already-emitted objects.
Breaks: `index-pack` "REF_DELTA base not found" on clones. This is why
per-ref emissions must NOT be interleaved into the spine (proven to
break suffix completeness — robustness review) and why the extra
emission is a single closure at the tail.

**I4 — Suffix completeness: for a client whose haves include spine
commit *i* (or WAL tip *n*), the suffix from `spine[i+1]` (or
`wal[n+1..]`) plus `extra_emission` plus the WAL contains every object
the client is missing, and only bases it already has or that precede in
the suffix.** This is what makes fetch = "cut the stream at the ACK".
Relied on by: single-round-trip incremental fetch, the H3 result.
Checked by: e2e fetch scenarios at every k, side-branch and WAL-tip
cases.

**I5 — No duplicate objects anywhere in one manifest's stream.**
`index-pack` rejects a pack that contains the same OID twice
("already resolved" / duplicate-base failures — this bit us via
rev-walk over-inclusion of re-introduced objects). Enforced at ingest
by the global seen-set with an entry-count assert. `api::commits` skips
objects the layout holds before packing and `mirror::sync` packs only
what its walk found new, so neither can produce a duplicate at all.
`proto::receive` is the one that receives somebody else's pack, and it
**drops** duplicates when the push only *creates* refs and still
**refuses** otherwise.

This said "rejected, not deduped" until 2026-08-31, and the push path
rejected unconditionally. The reasoning was about a *racing* client, but
the check does not know about races — it fires whenever an object is
already present, which includes the entirely sequential case of pushing
a branch, deleting it and pushing it again (deleting a ref does not
delete its objects), and creating a branch at a commit the server
already has. Both were refused with "already present (concurrent
push?) — fetch and retry", advice that cannot work: the object is
unreferenced or already the client's, so no fetch brings it.

**Why creates only.** The refusal was quietly doing a second job. A
stale client force-pushing over somebody else's work re-sends live
history it cannot exclude — it does not hold the tip the server
advertised, so it cannot use it as a negative base — and being refused
for duplicates is what stopped it. The ref precondition does *not* catch
that, despite the note under the connectivity walk in `receive.rs`
claiming every push here is force-with-lease: for a plain `--force` git
fills `old` from the server's own advertisement, so `curr == u.old`
always holds. A create cannot take anybody's work, so dropping there is
safe by construction; an update keeps the refusal until that gap is
closed on purpose.

That gap is a real one and is written down rather than closed here:
**a plain `git push --force` is not force-with-lease**, and the only
thing standing between a stale client and somebody else's push is the
duplicate refusal it happens to trip. Closing it properly means either
requiring `--force-with-lease` semantics on the wire or restoring an
ancestry check for updates — a decision about push semantics, not a
side effect of a dedup fix.

**I6 — Entry counts in the manifest are exact.** The synthesized pack
header carries the summed count; `index-pack` verifies it. Any
component that adds/removes entries must fix the counts (the
`extra_emission`-in-last-hot-segment accounting in `build_locator.py`
exists for exactly this).

## Pointers, epochs, atomicity

**I7 — Data first, pointer last.** No object referenced by a
`manifest.json` or `locator.hdr` may be PUT after the pointer that
references it. Relied on by: readers with no locking whatsoever.
Breaks: 404s mid-clone on a live swap.

**I8 — Epoch data is immutable.** Once a pointer references an epoch,
nothing under that epoch prefix is overwritten or deleted (until epoch
GC's grace-window sweep). A reader that loaded a manifest may keep
issuing ranged GETs against its keys indefinitely.

**Verification, in three parts, two of them now covered.** The
straddled-pointer half is covered
(`faults_e2e::compaction_lost_race_via_injection` clones, fscks and
point-reads while `locator.hdr` and `manifest.json` disagree). The
**sweep-under-an-in-flight-clone** half is covered by
`chaos_e2e::upstream_may_not_sweep_an_epoch_a_fork_is_cloning_from`,
which zero-copy forks made both possible to write and necessary: a fork
reads upstream's epoch, upstream compacts past it, and a sweep at *zero*
grace must still not take it while a clone of the fork is parked inside
those keys. No grace window can answer that one — a fork may hold an
epoch for months — so the `epoch_refs` reference is what holds it, and
removing the resolver makes that test delete the epoch and fail.

**Still not verified**: the *swap*-under-an-in-flight-clone half, where a
reader streams from a manifest it already loaded while the **same**
repository swaps its pointers underneath. The epoch-swap-under-reader
e2e this line used to cite lives in the research repo and was never
ported, and no suite here runs a reader on one node against a swapper on
another, which is the production topology. Manifest segment keys are stored absolute
so an old manifest survives a swap. *Appending* new keys into a live
epoch is allowed and used — incremental folds add hot segments, locator
generations, ref pages, and WAL objects — because no existing pointer
references them until a pointer swap does; existing objects are never
touched.

**I9 — The manifest is the only ref truth, and it changes only by
CAS.** Push validation reads `(manifest, etag)` once, validates
against exactly that snapshot, and writes with `If-Match`. On 412
everything re-runs. Never patch a manifest you didn't validate
against. Breaks: lost ref updates, refs pointing at objects whose WAL
entries lost a race. With a paged ref store this still holds: page
objects are immutable and content-addressed, so writing a new page
changes nothing until the manifest that points at it lands — the CAS
remains the single commit point for refs, pages, WAL, and tiers alike.

**I10 — WAL objects land before the manifest that references them**
(push-side instance of I7), and WAL keys are content-addressed so
replays/races are idempotent.

## Verification gates

**I11 — Every produced clone passes `git fsck --full --strict` in CI.**
Brief §8: correctness precondition, never relaxed, no exceptions for
"it's just a perf change". This gate caught the shallow-graft breakage
and the walk-divergence bugs; it is the reason the numbers are
trustworthy.

**I12 — Nothing enters the store unverified.** A push is quarantined:
prior WAL materialized, thin bases fetched through the read plane and
hash-verified on write-in, `index-pack --fix-thin` over the result,
then connectivity/fast-forward BFS (descend pushed objects only; every
edge leaving the push must exist in locator ∪ WAL sidecars; each
update's old tip must appear as such a terminal edge — that *is* the
ff proof). `--strict` is deliberately absent (requires a full local
odb; stock git's `receive.fsckObjects` defaults off for the same
reason) — connectivity is our check, not index-pack's.

**I12a — A published package version's bytes never change.** The digest
is verified before the artifact becomes addressable, and republishing a
version is refused rather than honoured: only yank and deprecate exist.
npm, PyPI, Maven and Cargo all assume this and cache accordingly, so a
mutable version is not a policy choice, it is a supply-chain hole — and
it is what the provenance and the licence gate both rest on. The one
deliberate exception is an **OCI tag**, which is a moving pointer by
design; what never changes there is the *manifest*, addressed by its
digest, and both the old and the new remain pullable that way.

**I13 — Bounded hostile input everywhere.** Varints capped, inflate
output bounded by declared size, delta opcodes bounds-checked, pkt-line
≤ 65516, body ≤ 64 MB (an OCI layer is the one exception and is
streamed into fixed-size blocks rather than buffered, so no request's
body is ever resident), BFS ≤ 200k visits / 5k frontier lookups,
ranged GETs must return 206. Any new parser follows suit; over-budget
work is rejected loudly to the fallback path, never best-effort.

## Point-read plane

**I14 — Chain plans are complete at build time.** A locator record's
plan (inline or chains.bin) covers the *entire* delta resolution —
span ranges include every intra-emission OFS base, hops cover every
REF root transitively. The reader applies, it never discovers. Breaks:
the 249-sequential-GET pathology this design replaced, or failed
resolution.

**I15 — `locator.hdr` carries the epoch its data lives in**, and the
reader derives every data key from that one header. A reader never
mixes the manifest's epoch with the locator's: the two pointers may
lag each other during ingest, and each must be internally consistent
on its own.

## Protocol behavior worth pinning (not obvious from git docs)

- Stateless v2 negotiation resends all haves each round; ACK selection
  checks WAL tips **before** spine commits (a WAL tip is newer truth).
- depth-1 is served from the snapshot only while `wal` is empty;
  otherwise fall through to full negotiation (stale-snapshot guard,
  e2e-covered).
- Shallow-ingested corpora must advertise `shallow-info` and
  `fetch=shallow`, and the manifest graft list is part of correctness,
  not metadata (index-pack rejects the clone without it).
- v0/v1 upload-pack clients are cleanly rejected (v2 gate);
  receive-pack speaks v0 by design (git never shipped v2 for push).
