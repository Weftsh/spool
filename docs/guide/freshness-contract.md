---
layout: ../../layouts/Docs.astro
title: The freshness contract
description: The exact behavior of a Weft mirror when origin is ahead, slow, or down.
---

# The freshness contract

The contract has one invariant: **never a silent stale miss.** Every case
where the mirror could serve you something old is explicit, on the wire, in a
way your runbook and your vendor-risk audit can quote.

## The cases

**You fetch a commit the mirror has.** It serves immediately from object
storage. Webhooks keep the mirror seconds behind your origin (p50 < 10 s
delivery-to-servable); a 60-second poll backstops webhook loss.

**You fetch a commit the mirror doesn't have yet.** Weft synchronously
fetches from your origin *before responding*, within a bounded budget
(default 8 s, configurable per deployment). If the sync lands your commit,
the response is fresh — CI racing a push just works. Concurrent requests for
the same repo coalesce into one origin fetch.

**The commit doesn't exist upstream either.** `404`, with a body that says a
sync ran and names the origin — never a hang, never a guess.

**The sync exceeds the budget.** `404` explaining the freshness budget was
exceeded and to retry shortly. Your job fails fast with a quotable reason
instead of hanging on a slow origin.

**Your origin is unreachable.** Everything already mirrored keeps serving —
that's the continuity pitch — and every response carries:

```
X-Weft-Staleness: <seconds since last successful sync>
X-Weft-Origin-Error: <what the last sync attempt saw>
```

The headers clear on the first successful sync after recovery.

## Why this makes the mirror safe unconditionally

The failure mode that would burn you is the invisible one: a green build
against code that wasn't the code. Under this contract a response is either
provably current, explicitly stale (headers), or an explicit failure naming
the origin. There is no fourth state.

## Verifying it yourself

Both behaviors are easy to drill:

```bash
# freshness: push to origin, immediately fetch the new SHA via the mirror
git fetch mirror $NEW_SHA        # triggers a synchronous sync

# staleness: check the headers during an origin incident
curl -sI -H "Git-Protocol: version=2" \
  "https://api.weft.sh/acme/widget.git/info/refs?service=git-upload-pack" \
  | grep -i x-weft
```
