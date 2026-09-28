# How serving works

A Spool server's nodes hold no repositories. Everything lives in S3-compatible
object storage as immutable, precomputed artifacts; serving a clone is
choosing byte ranges, not building packs.

## The layout

At ingest, a repository's history is packed offline into:

- **Cold segments** — the bulk of history, ordered *path-major* (each file's
  versions adjacent), cut into ~64 MB segments, each self-contained: deltas
  never cross a segment boundary.
- **Hot emissions** — one thin pack per recent mainline commit
  (~1,024-commit window), pre-deltified against the past. The manifest's
  *spine* records where each commit's bytes start.
- **A snapshot artifact** — the tip commit plus its full tree, self-contained,
  so `--depth 1` CI clones need no graph work at all.
- **A locator** — a sorted table mapping any object id to its exact byte
  range plus a precomputed delta-resolution plan, so single-file reads take
  a handful of parallel range GETs.
- **The manifest** — one small JSON object that is the single source of truth
  for refs and stream composition. It changes only by compare-and-swap.

## Serving

A **clone** is `concat(pack header, segment byte ranges, sha1 trailer)` —
streamed straight from object storage through the node. No delta search, no
pack-objects, no lock: this is why one clone costs a third (up to 1/73rd on
pathological repos) of the server CPU stock git needs, and why any node can
serve any repo.

A **fetch** finds the newest commit you already have on the spine and streams
the byte *suffix* after it — pre-deltified against exactly the objects you
hold. Single-commit fetches are routinely 40× smaller on the wire than what
bitmap-serving git ships.

Every produced clone must pass `git fsck --full --strict`; that gate runs in
the project's CI on every change and has never been waived.

## Writes

A push (or API commit) is verified in quarantine, appended to a write-ahead
log as an immutable object, and committed by one conditional PUT on the
manifest — concurrent writers serialize on the store's compare-and-swap, and
readers always see a complete state. A background compactor folds the log
into fresh segments; epochs left behind are garbage-collected after a grace
window longer than any running clone.

## What this buys you

- **Dormant repos cost object-storage pennies** — no hot replica, ever.
- **Nodes are stateless and interchangeable** — capacity is a scaling group,
  not a data migration.
- **Bursts don't serialize** — immutable data plus per-request state means a
  hundred simultaneous CI clones of one repo behave like one.
