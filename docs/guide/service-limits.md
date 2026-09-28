# Service limits

These are the v1 limits, stated plainly. Each traces to a measured or
designed bound in the engine, not to a guess. None of them is a quota:
Spool counts nothing against a plan, and how much you store is limited
by your own bucket.

| Limit | Value | Notes |
|-------|-------|-------|
| Push / request body | 64 MB | oversized pushes get `413`; large-object offload (LFS-class) is on the roadmap |
| Concurrent pushers per repo | ~16 | measured burst ceiling; beyond it pushes are politely rejected with a retry hint, never corrupted |
| Batch create/delete | 1,000 repos per call | split larger sets into several calls |
| Repo listing page | 1,000 | keyset pagination; millions of repositories list fine |
| Commit operations per request | 10,000 | one commit = one atomic ref transaction |
| Object ids | SHA-1 | layouts are format-versioned; SHA-256 repos are recognized and rejected loudly today |
| Shallow clones | `--depth 1` fast path | deeper `--depth N` falls back with an explicit error rather than degrading silently |
| Partial clone filters | not yet advertised | stock git therefore never sends them; full clones serve instead |
| CDN offload | opt-in, current-tip only | needs `fetch.uriprotocols` on the client (git ≥ 2.34) and a pack at the current tip; otherwise the clone is served inline, correct either way |

## Behavioral guarantees behind the numbers

- Anything over a bound fails **loudly and specifically** — a `413`, a `ng`
  report naming the reason, a `404` with an explanation. Silent degradation
  is treated as a bug.
- Just-pushed objects are readable immediately through the API and the
  wire; single-object point reads may take the slower write-log path until
  the background compactor folds them (seconds to minutes).
- Reset/undo never deletes objects; garbage collection honors a grace
  window longer than the longest running clone.
- A CDN-offloaded clone verifies what it downloads. A corrupt or missing
  pack fails the clone outright; it never produces a repository that
  looks complete and is not.
