# Search

One endpoint finds a repository across every organization you belong to:
by name, namespace, description or topic.

```bash
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  "https://spool.example.com/v1/search/repos?q=widget"
```

```json
{ "repos": [
    { "id": "01HX…", "org_id": "01HW…", "org": "acme", "name": "widget",
      "description": "the fast one", "kind": "native",
      "created_at": 1766000000000 } ],
  "next": null }
```

Search needs a signed-in person — a dashboard session or a personal
token — or an organization token. A request with no credential is a
`401`. A token bound to one repository is a `403`: it was minted to reach
that repository, and a search is not that repository.

## What matches

A case-insensitive substring of the **repo name**, its **namespace**, its
**description**, or any of its **topics**. Nothing else: not file paths,
not file contents.

An empty `q` matches everything you can see.

## Narrowing to one topic

`q` is fuzzy and inclusive — somebody typing `kubernetes` means "anything
to do with kubernetes" and does not know or care which field carries the
word, so a repository *tagged* `kubernetes` and one that merely mentions
it in its description both come back.

`topic` is the other half, and is exact:

```bash
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  "https://spool.example.com/v1/search/repos?topic=kubernetes"
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
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  "https://spool.example.com/v1/search/topics?limit=24"
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

Your query is text, never syntax — `%` matches a literal percent sign and
`_` a literal underscore, so a search for `100%` finds the repo called
`100%` rather than every repo there is. Queries longer than 128
characters are refused with a `400` rather than quietly truncated:
answering a different question than the one asked is worse than saying
no.

## What you are allowed to find

| You are | You see |
| --- | --- |
| no credential | nothing: `401` |
| signed in, or a personal token | repositories in every organization you belong to, and in your own namespace |
| an organization token | that organization's repositories |
| a token bound to one repository | nothing: `403` |

This is the same rule every other route enforces, and it is deliberately
*not* per-repo grants: a grant without membership does not make a
repository show up in search. You can still open a repository you hold a
grant on by its name.

**A description is as private as its repository.** Text you write about a
repository is never matched for anyone who cannot already see it.

## Paging

`limit` defaults to 25 and caps at 100. When there is another page the
response carries a `next` cursor:

```bash
curl -H "Authorization: Bearer $WEFT_TOKEN" \
  "https://spool.example.com/v1/search/repos?q=&limit=50&after=acme/widget/01HX…"
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
curl -X PATCH https://spool.example.com/v1/orgs/acme/repos/widget \
  -H "Authorization: Bearer $WEFT_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{ "description": "the fast one" }'
```

- Up to 512 characters, one line — no newlines, tabs or control
  characters, because a description is rendered inside a table cell.
- `"description": null` or `""` clears it. Leaving the field out leaves
  it alone, so an edit to one field never silently changes the other.
- `repo:write` is enough.

## There is no publishing a repository

Every repository is private to its organization, and there is no switch
that changes that. A create or a `PATCH` that sends `"public": true` is
refused with `400`:

```
this server has no public repositories: every repository is private to its organization — omit "public" or set it to false
```

`"public": false`, or leaving the field out, is accepted. The refusal is
there so that a script written for Weft's hosted service finds out,
rather than being quietly handed a private repository it thought it had
published.

## Absent and unreadable answer the same

With no credential, every repository answers **401**, whether it exists
or not:

```console
$ curl -si https://spool.example.com/v1/orgs/acme/repos/payments | head -1
HTTP/1.1 401 Unauthorized
$ curl -si https://spool.example.com/v1/orgs/acme/repos/no-such-repo | head -1
HTTP/1.1 401 Unauthorized
```

Once you *have* presented a credential, a repository you cannot read and
one that does not exist both answer **404**. Having authenticated tells
you nothing about repositories you cannot reach, so a token from another
organization and a real absence are indistinguishable. Two different
answers would be an enumeration oracle: ask for a name, read the status
code, and you have learned whether somebody else's repository exists.
The git wire answers the same way.

Namespace names are not masked. They are unique on the server and
claimed first-come, so signup already answers "does `acme` exist?" to
anyone who asks; pretending otherwise here would be theatre.
