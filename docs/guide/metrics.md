# Metrics & usage

## Per-repo serving metrics

```bash
curl -H "Authorization: Bearer $TOKEN" \
  "https://api.weft.sh/v1/orgs/acme/repos/widget/metrics?from=$FROM_MS&to=$TO_MS"
```

Returns, per kind:

| Kind | Meaning |
|------|---------|
| `clone` | full clones served — count, bytes, p50/p99 latency |
| `fetch` | incremental fetches — count, bytes, p50/p99 |
| `push` | accepted writes and their latency |
| `api` | REST and advertisement requests absorbed |
| `freshness` | webhook-receipt → servable lag on mirrors |

Percentiles come from log-scale latency histograms recorded per minute, so
p99 is a real tail measurement, not an average in disguise. Add
`&format=csv` for the spreadsheet-ready version — the artifact your renewal
conversation wants.

The metrics response also carries the mirror's current sync state
(`last_sync_at`, `sync_error`), so one call answers "is it healthy and how
fast is it".

## Org usage

```bash
curl -H "Authorization: Bearer $TOKEN" https://api.weft.sh/v1/orgs/acme/usage
```

Daily rows of `active_repos` (repos that did any work), `total_repos`,
`requests`, `bytes_out`, `hosted_minutes`, `private_bytes_out` and
`private_bytes_stored`, plus your plan. `bytes_out` is everything served,
public included; only `private_bytes_out`, `hosted_minutes` and
`private_bytes_stored` reach the bill, and they do so through the
organization's [pool](/docs/billing/#what-a-seat-brings): the three
`meters` on the billing view are these rows summed over the billing
period. `private_bytes_stored` is the day's average, not its peak.
**Dormant repos never appear in `active_repos`, and dormant repos are
free** — what they store is the one thing a dormant private repository
still counts for.

## Prometheus

`GET /metrics` exposes process-level counters
(`stratum_requests_total`, `stratum_bytes_out_total`) for your own
monitoring stack, alongside `GET /healthz` for liveness checks.
