# Webhooks

## Inbound: keeping mirrors fresh

Point your origin's push webhook at Weft:

```
POST https://api.weft.sh/webhooks/github     (GitHub App deliveries)
POST https://api.weft.sh/webhooks/generic    (anything else)
```

Deliveries must carry `X-Hub-Signature-256: sha256=<hmac>` computed over the
raw body with your webhook secret; unsigned or mis-signed deliveries are
rejected with `401`. A verified push event fans out background syncs to
every mirror of that origin, and the receipt-to-servable lag is recorded as
the `freshness` metric.

Generic-provider payloads identify the origin by URL:

```json
{ "full_name": "https://git.example.com/acme/widget.git" }
```

## Outbound: push events from your repos

Subscribe a URL to a repo:

```bash
curl -X POST https://api.weft.sh/v1/orgs/acme/repos/session-8412/webhooks \
  -H "Authorization: Bearer $TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "url": "https://app.example.com/hooks/stratum" }'
```

The response includes the delivery `secret` — shown once. Every delivery is
signed the same way (`X-Weft-Signature-256: sha256=<hmac over the body>`);
verify before trusting.

You can do the same from the repository's **Settings → Push webhooks**
panel, which is also where the CI intake secret lives — the two are the
two halves of one job. See [CI integration](ci-integration.md) for the
whole loop.

### The events

| Event | Fires when |
|---|---|
| `push` | Anything moves a ref: a `git push` over HTTPS or SSH, or `POST …/commits` |
| `change.landed` | A change lands, including one that landed by inclusion when a stack's top landed |
| `change.ejected` | The lander refused a change and took it out of the queue |

Every delivery is `{ event, repo_id, payload }`:

```json
{
  "event": "push",
  "repo_id": "01jm…",
  "payload": { "via": "api", "commit": "3f2a…", "branch": "main" }
}
```

**The `push` payload depends on how the push arrived, and the common case
is the thin one.** A `git push` carries `{ "via": "git" }` and an SSH push
carries `{ "via": "ssh" }` — no branch, no commit. Only `POST …/commits`
carries `{ "via": "api", "commit": …, "branch": … }`, as above. So a
receiver that reads `payload.branch` works against the API path and
silently never fires for real pushes; treat a `push` as "something moved,
go and look" and fetch to find out what.

`change.landed` carries `{ change, commit, branch, patchset }` (or
`included_in` in place of `patchset` when it landed by inclusion), and
`change.ejected` carries `{ change, verdict }`.

Failed deliveries retry three times with backoff; delivery outcomes are
recorded and visible to support. List subscriptions with `GET …/webhooks`,
remove them with `DELETE …/webhooks/{id}`.
