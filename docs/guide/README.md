# Spool user guide

Spool is a git forge you run yourself. Every repository is real git,
stored in your own bucket, and private to the organization that owns it.
**Repos** gives you a repository per user or per agent session over REST.
**Mirrors** keep a copy of a GitHub repository next to your CI. **Review**
and **Workflows** check and land changes on either.

Examples on these pages use `https://spool.example.com` for the server.
Put your own server's URL in its place.

## Start here

1. **Get an account on your server.** Whoever runs the server creates the
   first one from the command line. After that, an organization's admins
   invite people in by email, and the operator can add them from the
   command line too. There is no signing yourself up.
   [Authentication](authentication.md)
2. **Join an organization and mint a token.** An organization is where
   your team's repositories live. A token is what your CI and scripts
   sign in with, and it acts in one organization.
   [Authentication](authentication.md)
3. **Pick a quickstart.** Point CI at a mirror, or make your first
   commit over REST. Both end with a clone.
   [Mirror a GitHub repository](quickstart-mirror.md) ·
   [First commit over REST](quickstart-repos.md) ·
   the SDK for [TypeScript](sdk.md) or [Python](sdk-python.md)

## Understand the system

- [How serving works](how-serving-works.md): why empty-disk nodes are fast
- [Changes, OWNERS and landing](code-review.md): per-commit review and the fast-forward land queue
- [Changesets](changesets.md): one review over changes in several repositories, landed together
- [Forks](forks.md): contributing to a repository you can read but not push to
- [The freshness contract](freshness-contract.md): never a silent stale miss

## Operate

- [Settings](settings.md) — where each setting lives and who may change it
- [Webhooks](webhooks.md) — inbound origin events, outbound push events
- [Workflows](workflows.md) — CI from a `.weft/*.yml` file, run on machines your organization registers
- [Running a self-hosted runner](self-hosted-runners.md) — the operator's side: the binary, a systemd unit, and how to isolate it
- [CI integration](ci-integration.md) — bring your own CI: sign a verdict onto the Checks tab
- [Checking out from a mirror in GitHub Actions](actions-checkout.md)
- [Importing issues from GitHub](import-github.md)
- [Git over SSH](ssh.md)
- [Search](search.md) — find a repository in the organizations you belong to
- [Audit and undo](audit-and-undo.md) — the trail, and putting a branch back
- [Export](export.md) — bundles, one repository or the whole organization
- [Metrics and usage](metrics.md) — p50/p99, CSV, Prometheus
- [CDN offload](cdn-offload.md) — serving clone packs from a CDN
- [Service limits](service-limits.md) — the v1 envelope
- [The maintainer firewall](maintainer-firewall.md) — a design, not yet built

## For agents and scripts

- [TypeScript SDK](sdk.md) — `@weftsh/sdk`: a repository per session,
  commits, reads, undo and repo-scoped git remotes in a few lines
- [Python SDK](sdk-python.md) — `weftsh`: the same, sync or asyncio
- [The REST API](../openapi.json) — OpenAPI 3.1. Your server also serves
  it at `/openapi.json`.
