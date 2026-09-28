# On-storage formats

Normative spec for every byte Spool writes to the object store. The
code is the implementation; this is the contract. If code and this
document disagree, one of them has a bug — fix whichever is wrong and
say so in the commit. The implementation is
`crates/stratum-store/src/{manifest,plane}.rs`,
`crates/stratum-proto/src/{receive,serve}.rs` and `crates/stratum-engine`.
The `SLH4` locator header was added for zero-copy forks; `SLH3` and
`SLH2` read exactly as before.

All multi-byte integers are **big-endian**. All OIDs are **SHA-1
(20 bytes binary / 40 hex)** — every fixed-width field below bakes this
in, which is why a SHA-256 repository is recognised and refused rather
than served.

## Key layout

```
<repo>/<layout>/manifest.json                     ← mutable pointer (CAS'd by pushes)
<repo>/<layout>/locator.hdr                       ← mutable pointer (swapped by ingest)
<repo>/<layout>/<epoch>/cold-%04d.seg             ← immutable
<repo>/<layout>/<epoch>/hot-%04d.seg              ← immutable
<repo>/<layout>/<epoch>/snapshot.seg              ← immutable
<repo>/<layout>/<epoch>/locator.bin               ← immutable
<repo>/<layout>/<epoch>/chains.bin                ← immutable
<repo>/<layout>/<epoch>/wal/<digest>.seg          ← immutable (content-addressed)
<repo>/<layout>/<epoch>/wal/<digest>.oids         ← immutable (content-addressed)
<repo>/<layout>/<epoch>/refs/page-<sha256>.txt    ← immutable (content-addressed)
<repo>/<layout>/<epoch>/locator-g%04d.bin         ← immutable (per generation)
<repo>/<layout>/<epoch>/chains-g%04d.bin          ← immutable (per generation)
<repo>/<layout>/<epoch>/snapshot-g%04d.seg        ← immutable (per generation)
```

- `<layout>` is `tiered` (the production shape) or `flat`.
- `<epoch>` is an opaque string minted at ingest
  (`YYYYMMDDTHHMMSS-<pid>`); nothing parses it, it only namespaces.
  Everything under an epoch prefix is **immutable once the pointer that
  references it is swapped** — re-ingest writes a whole new epoch.
- Exactly two mutable objects exist per layout: `manifest.json` and
  `locator.hdr`. Upload order is always **data first, pointer last**.
- WAL `<digest>` = SHA-1 hex of the `.seg` payload, so retried PUTs are
  idempotent and racing identical PUTs are harmless.

## manifest.json (schema 3)

JSON, serde-mapped in `crates/stratum-store/src/manifest.rs`. The
single source of truth for refs and stream composition; the read path
loads it once per request, the write path CASes it.

| field | type | meaning |
|---|---|---|
| `schema` | u32 | `3` |
| `repo`, `layout` | string | identity; `layout` ∈ {`tiered`,`flat`} |
| `object_format` | string | git object format of every OID in the layout; default `"sha1"`; readers reject formats they don't speak |
| `refs` | [(refname, oid)] | unpaged repos: all advertised refs, push-updated. Paged repos: only serving-critical tips (HEAD's branch) |
| `ref_pages` | [{first, last, key, count, bytes}] | schema 4, sharded ref store: sorted non-overlapping refname ranges, each an immutable content-addressed page of "oid SP refname LF" lines. `ls-refs` loads only prefix-overlapping pages; a push rewrites one page (split at STRATUM_REF_PAGE_MAX, default 1000) and commits it via the manifest CAS. Measured: 100k refs → 34 KB manifest, 0.36 s full advert |
| `head` | string | symref target for HEAD |
| `segments` | [Segment] | flat layout only: `cold-%04d.seg` list |
| `cold_segments` | [Segment] | tiered: path-major full-delta segments, clone prefix |
| `hot_segments` | [HotSegment] | tiered: concatenated thin emissions (`entries` not tracked per segment — per spine entry instead) |
| `spine` | [SpineEntry] | first-parent commits, oldest→newest: `{oid, seg (hot index), off, entries, bytes}`; the ACK/suffix directory |
| `extra_emission` | {entries, bytes}? | schema 3: multi-ref closure emission, always the tail of the **last** hot segment |
| `tail_emissions` | [{seg, off, entries, bytes}] | schema 4: positioned secondary-ref closures — the ingest trailer plus one per incremental fold. Every ACK suffix must deliver each of them (explicitly, when the suffix starts after one). A full compaction resets the list; `tails()` normalizes v3→v4 |
| `snapshot` | Segment? | self-contained depth-1 pack (tip commit + tree closure) |
| `locator` | {key, hdr_key, chains_key, record_bytes, records}? | point-read plane keys |
| `shallow` | [oid] | graft boundary for shallow-ingested corpora (served in `shallow-info`) |
| `epoch` | string | the epoch every relative decision uses |
| `wal` | [WalEntry] | push log, append-only until compaction: `{key, oids_key, entries, bytes, updates: [(refname, old, new)]}` |

Segment = `{key, entries, bytes}` (keys stored absolute, so old-epoch
manifests keep working mid-swap). Stream-plan rules implemented in
`manifest.rs`:

- clone = `cold_segments ++ hot_segments ++ wal[..]` (or `segments ++ wal` flat);
  total entry count = sum of all parts, WAL included.
- fetch-from-spine-commit *i* = hot bytes from `spine[i+1].off`
  (spanning segments as needed) + `extra_emission` + all WAL entries.
- fetch-from-WAL-tip *n* (client's have = `updates.new` of `wal[n]`) =
  `wal[n+1..]` only.
- depth-1 = `snapshot` alone, **only when `wal` is empty**.

## locator.hdr (magic `SLH4`; `SLH3` and `SLH2` still readable)

The point-read plane's atomically-swapped pointer, written by
`crates/stratum-engine/src/locator.rs::build_locator` (and by incremental compaction), parsed by
`crates/stratum-store/src/plane.rs`. All three magics are decoded and
encoded by `plane::parse_header` / `plane::write_header` — one decoder,
shared by the plane loader, the fork writer and epoch GC.

```
offset  size   field
0       4      magic "SLH3"
4       2      u16 epoch length E
6       E      epoch (ASCII); data keys are derived as <layout-root>/<epoch>/…
6+E     4      u32 generation G — data files are locator-g%04d.bin /
               chains-g%04d.bin for G. Incremental compaction writes
               generation G+1 into the same epoch and swaps this hdr;
               older generations stay in place for live pointers until
               epoch GC.
10+E    8      u64 record count
18+E    8      u64 n_cold  (segment indices < n_cold → cold-%04d.seg, else hot-%04d.seg)
26+E    4097×8 u64 byte offsets into the locator file: bucket b covers
               records [offset[b], offset[b+1]); bucket = top 12 OID bits
```

`SLH2` is the generation-less legacy form (files named `locator.bin` /
`chains.bin`, no G field). A reader that sees an unknown magic must fail
loudly, never guess.

### SLH4 — an absolute data prefix

`SLH4` exists so a layout can point at data that is **not underneath
it**. Everything about the plane is otherwise unchanged.

```
offset    size   field
0         4      magic "SLH4"
4         2      u16 epoch length E
6         E      epoch (ASCII) — informational; it does NOT derive data keys
6+E       1      u8 generation present: 0 or 1. Any other value is an error.
7+E       4      u32 generation G — present only when the flag is 1
7+E+g     2      u16 data-prefix length P   (g = 4 when the flag is 1, else 0)
9+E+g     P      data prefix (UTF-8), absolute, no trailing slash.
                 Data keys are <data prefix>/cold-%04d.seg, …/locator-g%04d.bin,
                 …/chains.bin, and so on — the reader's own prefix is not used.
9+E+g+P   8      u64 record count
17+E+g+P  8      u64 n_cold
25+E+g+P  4097×8 u64 bucket offsets, exactly as SLH3
```

Rules, all load-bearing:

- **The generation flag is explicit, unlike SLH2/SLH3 where the magic
  implies it.** A fork of a legacy `SLH2` repository is an `SLH4` header
  with *no* generation, because upstream's data files are still named
  `locator.bin` / `chains.bin`. A reader that inferred a generation from
  the magic would misread the header by four bytes and send every point
  read to a garbage key.
- **P = 0 is an error**, not "use the default": an empty prefix resolves
  every data key to the bucket root.
- **The epoch field is informational under SLH4.** Data keys come from
  the prefix. Epoch GC must therefore only count an `SLH4` epoch as live
  in *this* layout when the prefix names a directory directly under this
  layout; a prefix pointing elsewhere is another repository's epoch, kept
  alive by the control plane's `epoch_refs` reference rather than by this
  pointer.
- Writers emit the **narrowest** magic that carries their content:
  no prefix and no generation is `SLH2`, no prefix with a generation is
  `SLH3`, any prefix is `SLH4`. So decode-then-encode is byte-identical,
  which is what lets a fork copy a header and change exactly one field.

**Why it exists.** A zero-copy fork copies upstream's manifest into its
own prefix and shares upstream's immutable objects. Manifest segment keys
are already absolute, so that half worked — but `Plane::load` derived
`data_prefix` from the *caller's* prefix, so a forked repository cloned
correctly and then 404'd on `/files`, `/tree`, `/diff` and `/log`. The
fix is at the pointer: `plane::rebase_header` re-points a copied header,
and it is the only write a fork's read path needs.

**Ordering.** The `epoch_refs` reference that pins upstream's epoch must
be registered **before** this pointer is published. A crash between the
two leaves storage pinned slightly too long, which a sweeper reconciles;
the reverse order leaves a live fork pointing at collectable data, which
is corruption. Same argument as I7 — data first, pointer last.

## locator.bin — 150-byte records, sorted by OID

`RECORD = ">20sIQIQQH"` + 4 inline hops (`build_locator.py:32`):

```
offset  size  field
0       20    oid (binary)
20      4     u32 segment index (global: cold then hot)
24      8     u64 entry offset within segment
32      4     u32 entry length (this entry's own bytes)
36      8     u64 span start — lowest byte offset this entry's intra-segment
              OFS-delta chain reaches (read [span, off+len) to resolve locally)
44      8     u64 chain_off — byte offset into chains.bin (0 if inline/none)
52      2     u16 chain_cnt — number of hops (0 = self-contained)
54      96    4 × 24-byte inline hop slots (zero-padded); used when
              chain_cnt ≤ 4, so the common chained read needs no
              chains.bin round trip
```

Lookup = one ranged GET of the record's bucket slice + linear scan
(records within a bucket are OID-sorted; buckets average ~records/4096).

## chains.bin / hop encoding

A hop is `">IQIQ"` (24 bytes): `u32 seg`, `u64 range_start`,
`u64 range_len`, `u64 entry_off`. A chain plan lists hops **root
first**; each hop's `[range_start, range_start+range_len)` covers that
emission's local OFS closure, and `entry_off` marks the entry to
resolve within it. Plans are fully transitive at build time (REF roots
resolved recursively), so the reader never re-walks: it fetches all hop
ranges (coalescing gaps ≤ 128 KB, ≤ 8 parallel GETs) and applies the
delta stack root→leaf.

## Segments (`*.seg`) and WAL payloads

Every `.seg` object is a **stripped pack**: a `git pack-objects` v2
pack with the 12-byte `PACK` header and 20-byte SHA-1 trailer removed —
raw entries only. Consequences:

- Serving concatenates: the server writes a fresh 12-byte header with the
  summed entry count, streams the planned payload byte-ranges verbatim,
  and appends a freshly computed SHA-1 trailer
  (`crates/stratum-proto/src/serve.rs::stream_pack`). No pack parsing on the
  clone path.
- Cold segments: OFS deltas only, **strictly intra-segment** (H1
  invariant). Hot emissions and WAL payloads: thin — REF_DELTA bases
  must appear **earlier in the global stream order** (see
  `docs/invariants.md`).
- WAL `.seg` = the pushed pack's stripped payload, byte-for-byte as the
  client sent it (already `--fix-thin`-verified in quarantine).

## WAL `.oids` sidecar

Newline-separated sorted 40-hex OIDs — exactly the objects contained in
the paired `.seg`. Used by push verification (connectivity terminals,
duplicate rejection) without re-parsing packs.

## snapshot.seg

Stripped pack containing the tip commit + its complete tree/blob
closure, no deltas against anything external — a valid depth-1 clone
body on its own. Built at ingest; stale (and therefore unused) whenever
`wal` is non-empty.

## Mutation protocol

- **Ingest / compaction**: write the full new epoch directory, then swap
  `manifest.json`, then `locator.hdr` — each a conditional PUT
  (create-only via `If-None-Match: *` on first ingest, `If-Match` on the
  previously-current etag otherwise; `crates/stratum-store`). The compactor
  CASes against the etag it read *before* cloning, so a concurrent push
  forces a retry rather than being dropped. The two pointers may lag
  each other briefly; each is internally consistent on its own (I15).
- **Push**: PUT `wal/<digest>.seg` + `.oids` (unconditional,
  content-addressed), then `PUT manifest.json` with `If-Match: <etag>`
  read at the start of validation; on 412, re-run the whole validation
  against the fresh manifest, ≤ 3 attempts. First manifest creation
  uses `If-None-Match: *`. On 409 as well as 412 — see below.

  Both semantics are pinned by an executable contract rather than by
  prose: `crates/stratum-store/src/contract.rs` names every store
  semantic the engine depends on, together with the call site that breaks
  without it. It runs against MinIO on every CI run
  (`crates/stratum-testkit/tests/store_contract.rs`) and against a real
  bucket as a manual gate (`scripts/manual-s3.sh check
  --both-addressing-styles`, under the deploy role — see CLAUDE.md).

  This used to read "verified against MinIO; real S3 unverified here —
  top open item", and the open item was hiding a real defect. Real S3
  answers **409 ConditionalRequestConflict** when two conditional writes
  to one key *overlap*, reserving 412 for a precondition that genuinely
  failed; a racing pair can even see 409 first and 412 on the retry.
  MinIO never emits 409, so nothing in the suite had ever produced one,
  and the store client mapped it to a generic error — which meant that on
  the multi-node deployment the design exists to support, the loser of a
  manifest CAS fell out of the retry loop and failed the user's push.
  Both statuses now map to `PutError::Conflict`.
