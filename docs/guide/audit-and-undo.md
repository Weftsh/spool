# Audit & undo

Agent platforms need two answers on demand: *"what did the agent change and
when?"* and *"put it back."* Weft treats both as first-class API surface.

## The audit trail

Every write — API commit, git push, repo create/delete, ref change, token
mint — records who acted, a timestamp, and, for commits, **your context
blob**. "Who" is the person when there is one (`user:01hx…`), whichever
credential they reached for, and the token when it acts for nobody
(`token:01hx…`):

```json
{ "context": { "agent_run": "r-42", "prompt": "p-991", "user": "u-7" } }
```

Query it per repo, per person, per action, or by time:

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "https://api.weft.sh/v1/orgs/acme/audit?repo=session-8412&limit=100"
```

| Parameter | What it narrows to |
| --- | --- |
| `repo` | One repo, by name |
| `user` | Everything one person did, by user id |
| `principal` | One exact actor string — `user:…`, `token:…`, `system:…` |
| `action` | One action, e.g. `token.mint` |
| `since` / `until` | A time window, epoch milliseconds |
| `order` | `asc` (default, oldest first) or `desc` (newest first) |
| `after` / `before` | The pagination cursor for that order |
| `limit` | Up to 1000; 100 by default |
| `format=csv` | The same rows as `text/csv` |

Paging follows the order you asked for. Reading forwards, the response
carries `next_after`; reading backwards it carries `next_before`. Feed the
cursor back on the next request until a page comes back short:

```bash
# Newest first, then the page before it.
curl -H "Authorization: Bearer $TOKEN" \
  "https://api.weft.sh/v1/orgs/acme/audit?order=desc&limit=100"
curl -H "Authorization: Bearer $TOKEN" \
  "https://api.weft.sh/v1/orgs/acme/audit?order=desc&limit=100&before=<next_before>"
```

`format=csv` returns the same rows for a spreadsheet or a ticket. Every
field is quoted and inner quotes are doubled, so a context blob's commas
stay inside their cell:

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "https://api.weft.sh/v1/orgs/acme/audit?format=csv&since=1735689600000" \
  -o activity.csv
```

**A credential scoped to one repo sees only that repo's trail** — filter or
no filter, JSON or CSV. That is deliberate: a per-repo CI token satisfies
org-level `org:read` so it can read its own repo's metadata, and without
this it would also be reading the org's credential inventory. Asking it for
a different repo answers `404`, the same way a foreign org does.

The log is append-only at the application layer, and batches ship
write-once to object storage — the durable record can't be rewritten, by us
or by you.

In the dashboard the same trail is **Settings → Activity**: newest first,
the same filters, and the CSV export behind one button.

## Undo primitives

**Reset** moves a branch pointer — three commits forward, one request back:

```bash
POST /v1/orgs/acme/repos/session-8412/reset
{ "branch": "main", "to": "<good-commit>", "expected_head": "<bad-commit>" }
```

`expected_head` makes it race-safe: if someone else moved the branch first,
you get `409` with the current tip. After a reset, the abandoned commits
remain **reachable by SHA until garbage collection** — auditors can still
fetch exactly what the agent did.

**Revert** appends instead of rewinding — a new commit whose tree undoes the
branch head, preserving history:

```bash
POST /v1/orgs/acme/repos/session-8412/revert
{ "branch": "main" }
```

**Branch** checkpoints cheaply before risky work:

```bash
POST /v1/orgs/acme/repos/session-8412/branches
{ "name": "checkpoint-12", "from": "main" }
```

Tags work the same way (`POST /tags`, `DELETE /tags/{name}`).

## The wire stays strict

Over git, pushes remain fast-forward-and-create-only — forge semantics for
humans and tools. The REST API is the authority that may move refs backwards
and delete branches. That asymmetry is deliberate: your product owns undo;
git clients can't accidentally invoke it.
