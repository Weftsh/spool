# Metrics and usage

## Per-repo serving metrics

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "https://spool.example.com/v1/orgs/acme/repos/widget/metrics?from=$FROM_MS&to=$TO_MS"
```

It needs `repo:read` on the repository. It returns, per kind:

| Kind | Meaning |
|------|---------|
| `clone` | full clones served — count, bytes, p50/p99 latency |
| `fetch` | incremental fetches — count, bytes, p50/p99 |
| `push` | accepted writes and their latency |
| `api` | REST and advertisement requests absorbed |
| `freshness` | webhook-receipt → servable lag on mirrors |

Percentiles come from log-scale latency histograms recorded per minute, so
p99 is a real tail measurement, not an average in disguise. Add
`&format=csv` for the spreadsheet-ready version.

The metrics response also carries the mirror's current sync state
(`last_sync_at`, `sync_error`), so one call answers "is it healthy and how
fast is it".

## Org usage

```bash
curl -H "Authorization: Bearer $TOKEN" https://spool.example.com/v1/orgs/acme/usage
```

```json
{ "days": [ { "day": "2026-09-27", "active_repos": 4, "total_repos": 31,
              "requests": 18230, "bytes_out": 2143290112 } ] }
```

Up to 90 daily rows, newest first: `active_repos` (repositories that did
any work that day), `total_repos`, `requests` and `bytes_out` (everything
served). It needs `org:read`. **Dormant repositories never appear in
`active_repos`** — a repository nobody touched that day did no work.

## Prometheus and health checks

`GET /metrics` exposes process-level counters
(`stratum_requests_total`, `stratum_bytes_out_total`) for your own
monitoring stack. `GET /healthz` answers while the process is up, for
liveness. `GET /readyz` answers `200 ready` only when the node can reach
both PostgreSQL and the object store, and `503` naming the one that
failed otherwise — gate traffic on that one.
