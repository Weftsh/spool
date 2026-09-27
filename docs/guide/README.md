---
layout: ../../layouts/Docs.astro
title: Documentation
description: Weft documentation for people and for agents — quickstarts for mirrors and REST repos, TypeScript and Python SDKs, code review with OWNERS, workflows, runners, billing, and the REST API as OpenAPI and llms.txt.
---

# Weft documentation

Everything here is real git. **Mirror** stops your CI waiting on clones.
**Repos** gives you a repository per user or per agent session over REST.
**Review** and **Workflows** land and check changes on either. Public
repositories are free forever.

## Start here

Three steps, in order. The first two take a minute each.

1. **Create an account.** Free, no card. Your namespace holds public
   repositories; a team gets an organization.
   [Sign up](/login?mode=signup)
2. **Make an organization and a token.** An organization is where a
   team or a private repository lives; a public mirror or repository
   can live in your own namespace. A token is what your CI
   and scripts sign in with. [Authentication](/docs/authentication/)
3. **Pick a quickstart.** Point CI at a mirror in five minutes, or make
   your first commit over REST. Both end with a clone.
   [Mirror in 5 minutes](/docs/quickstart-mirror/) ·
   [First commit over REST](/docs/quickstart-repos/) ·
   the SDK for [TypeScript](/docs/sdk/) or [Python](/docs/sdk-python/)

## Understand the system

- [How serving works](/docs/how-serving-works/): why empty-disk nodes are fast
- [Changes, OWNERS and landing](/docs/code-review/): per-commit review and the fast-forward land queue
- [The freshness contract](/docs/freshness-contract/): never a silent stale miss
- [Organizations and billing](/docs/billing/): what a seat is, and what a failed payment does not do

## Operate

- [Webhooks](/docs/webhooks/) — inbound origin events, outbound push events
- [Workflows](/docs/workflows/) — CI from a `.weft/*.yml` file, on our runners or yours
- [Running a self-hosted runner](/docs/self-hosted-runners/) — the operator's side: the binary, a systemd unit, and how to isolate it
- [Weft runners for GitHub Actions](/docs/github-runners/) — keep your workflows on GitHub, `runs-on: weft`, and the jobs run on our fleet from the same pool of minutes
- [CI integration](/docs/ci-integration/) — bring your own CI: sign a verdict onto the Checks tab
- [Static sites](/docs/static-sites/) — commit a `.weft/site.yml` and a directory in the repository is served as a website
- [Packages](/docs/packages/) — a private registry for your own packages, with the commit that built each version
- [Package policy](/docs/package-policy/) — what may enter your builds from a public registry: licences, a waiting period, and names that are yours
- [Export & escape hatch](/docs/export/) — bundles, org-wide
- [Metrics & usage](/docs/metrics/) — p50/p99, CSV, Prometheus
- [Service limits](/docs/service-limits/) — the honest v1 envelope

## For agents

Machine-consumable surfaces, kept current with the docs build:

- [TypeScript SDK](/docs/sdk/) — `@weftsh/sdk`: a repository per session,
  commits, reads, undo and repo-scoped git remotes in a few lines
- [Python SDK](/docs/sdk-python/) — `weftsh`: the same, sync or asyncio

- [`/llms.txt`](/llms.txt) — index of this documentation
- [`/llms-full.txt`](/llms-full.txt) — the full documentation as one file
- [`/openapi.json`](/openapi.json) — the REST API, OpenAPI 3.1
