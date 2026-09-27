---
layout: ../../layouts/Docs.astro
title: "Search and discovery"
description: Find a repository by name, namespace, description or topic — and what a stranger is allowed to find.
---

# Search and discovery

One endpoint answers both "find that repo of ours" and "what is public
here", because they are the same question asked with different
credentials.

```bash
curl "https://api.weft.sh/v1/search/repos?q=widget"
```

```json
{ "repos": [
    { "id": "01HX…", "org": "acme", "name": "widget",
      "description": "the fast one", "public": true,
      "kind": "native", "created_at": 1766000000000 } ],
  "next": null }
```

No credential is needed. With one — a session cookie from the dashboard,
or an org token — the same request also returns repositories in the
namespaces you belong to.

## What matches

A case-insensitive substring of the **repo name**, its **namespace**, its
**description**, or any of its **topics**. Nothing else: not file paths,
not file contents.

An empty `q` matches everything you can see, which is what the
[discovery page](/discover) browses with.

## Narrowing to one topic

`q` is fuzzy and inclusive — somebody typing `kubernetes` means "anything
to do with kubernetes" and does not know or care which field carries the
word, so a repository *tagged* `kubernetes` and one that merely mentions
it in its description both come back.

`topic` is the other half, and is exact:

```bash
curl "https://api.weft.sh/v1/search/repos?topic=kubernetes"
```

Only repositories actually carrying that topic. This is what a topic pill
in a repository's About panel links to, and why it is a separate
parameter rather than a qualifier inside `q`: a pill that also returned
every repository mentioning the word in prose would be useless for the
one job a facet has.

Topics are stored lowercased, so `topic=Rust` and `topic=rust` are the
same request. A topic no repository carries — or a string that could not
be a topic at all, like `not a topic` — comes back as an empty page
rather than a `400`, because a link somebody typed by hand should come
back empty rather than as an error.

The two compose: `?q=operator&topic=rust` is "repositories tagged rust
whose name, namespace, description or topics also mention operator".

## What topics exist

```bash
curl "https://api.weft.sh/v1/search/topics?limit=24"
```

```json
{ "topics": [ { "name": "rust", "repos": 12 },
              { "name": "cli",  "repos": 4 } ] }
```

Most-used first, then alphabetically so the order is stable between
calls rather than reshuffling among equal counts. Scoped the same way
everything else here is: a topic carried only by repositories you cannot
see is not listed, because a list of topic names is an existence oracle
for the work behind them — "we have a `project-atlas` topic" is a
sentence about a private repository.

This is what the [discovery page](/discover) builds its topic chips
from. They were a fixed list of common words until 2026-08-31, which
matched whatever a repository happened to *say* rather than what anybody
had filed under, so most of them found nothing on a real instance while
the topics in genuine use appeared nowhere.

Your query is text, never syntax — `%` matches a literal percent sign and
`_` a literal underscore, so a search for `100%` finds the repo called
`100%` rather than every repo there is. Queries longer than 128
characters are refused with a `400` rather than quietly truncated:
answering a different question than the one asked is worse than saying
no.

## What you are allowed to find

| You are | You see |
| --- | --- |
| anonymous | every public repository |
| signed in | public repositories, plus everything in namespaces you belong to |
| an org token | public repositories, plus that org's |
| a repo-scoped token | public repositories only — it was minted to reach one repo, and a search is not that repo |

This is the same rule every other route enforces, and it is deliberately
*not* per-repo grants: a grant without membership is not access anywhere
else in the product, so it does not widen search either.

**A description is as private as its repository.** Text you write about a
private repo is never matched for anyone who cannot already see it.

## Paging

`limit` defaults to 25 and caps at 100. When there is another page the
response carries a `next` cursor:

```bash
curl "https://api.weft.sh/v1/search/repos?q=&limit=50&after=acme/widget/01HX…"
```

Ordering is `(namespace, name, id)` — deterministic and stable under
inserts, so paging visits each repository exactly once, and never depends
on a relevance score that could change between pages.

The cursor is applied *inside* the visibility filter. Editing one moves
the window within what you could already see; it cannot widen it, and a
cursor that parses as nothing simply starts from the beginning.

## Describing a repository

Descriptions are the only free text search can find a repo by. Set one at
creation, or afterwards:

```bash
curl -X PATCH https://api.weft.sh/v1/orgs/acme/repos/widget \
  -H "Authorization: Bearer $WEFT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "description": "the fast one" }'
```

- Up to 512 characters, one line — no newlines, tabs or control
  characters, because a description is rendered inside a table cell.
- `"description": null` or `""` clears it. Leaving the field out leaves
  it alone, so an edit to one field never silently changes the other.
- `repo:write` is enough.

## Publishing a repository

```bash
curl -X PATCH https://api.weft.sh/v1/orgs/acme/repos/widget \
  -H "Authorization: Bearer $WEFT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "public": true }'
```

Changing visibility needs **`org:admin`**, not `repo:write`: it is the
one edit here with consequences outside the organization — it puts the
code in front of anonymous search — and it is recorded in the
[audit trail](/docs/audit-and-undo/) as `repo.visibility`, which is the
question asked after a leak.

## Absent and private answer the same

Anonymously, a repository that does not exist and one that is private
both answer **401**:

```console
$ curl -si https://api.weft.sh/v1/orgs/acme/repos/payments | head -1
HTTP/1.1 401 Unauthorized
$ curl -si https://api.weft.sh/v1/orgs/acme/repos/no-such-repo | head -1
HTTP/1.1 401 Unauthorized
```

Two different answers would be an enumeration oracle: ask for a name,
read the status code, and you have learned whether that private
repository exists. The git wire has always answered `401` to both, and
this is REST catching up — the two front doors now agree about the same
repository.

Once you *have* presented a credential the answer is **404** for both
instead. Having authenticated tells you nothing about repositories you
cannot reach, so a foreign token and a real absence are indistinguishable
as well.

Namespace names are not masked. They are globally unique and claimed
first-come, so signup already answers "does `acme` exist?" to anyone who
asks; pretending otherwise here would be theatre.
