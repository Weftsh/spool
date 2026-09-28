# Importing issues from GitHub

Moving a project from GitHub to your Spool server is two steps, and the
first is pure git:

1. **Mirror the repository.** Commits, branches and tags come across
   through a [mirror](quickstart-mirror.md), connected through your
   server's GitHub App.
2. **Import its issues** into that mirror's tracker. That is this page.

The import reads from the same upstream the mirror already syncs from,
through the same GitHub App installation. It needs the server to have a
GitHub App configured, and that installation to hold the **`issues:
read`** permission.

## Starting one

```bash
curl -X POST "$WEFT_URL/v1/orgs/acme/repos/widget/import" \
  -H "Authorization: Bearer $ADMIN_TOKEN"
```

```json
{ "status": "queued",
  "detail": "Issues are being imported. Numbers are preserved, so an old #reference keeps meaning what it says." }
```

It needs **`org:admin`**, not `repo:write`: an import writes a project's
whole history into a tracker and cannot be undone by hand. The answer is
`202`, because nothing has been imported yet — a job exists.

Two things are refused before anything starts, because an import that
starts and fails an hour later, in a log, is a much worse answer than one
that never starts and says why:

- **`400`** when the repository has no GitHub origin to import from.
  Connect it as a mirror first, so the import reads the same upstream the
  commits do.
- **`409`** when the repository's tracker already has issues in it. An
  import keeps the numbers it came with, so it can only go into an empty
  tracker: importing next to existing issues would either collide with
  their numbers or renumber the imported ones, and every `#reference` to
  them elsewhere would stop meaning what it says.

## Watching it

```bash
curl "$WEFT_URL/v1/orgs/acme/repos/widget/import" \
  -H "Authorization: Bearer $WEFT_TOKEN"
```

```json
{ "labels": "done", "milestones": "done",
  "issues": "https://api.github.com/repositories/…/issues?page=7…",
  "state": "running", "error": null, "updated_at": 1787428539000 }
```

Progress is reported per phase rather than as one percentage, because
the phases are the thing that resumes and a single number would be
invented. Each phase is `null` before it starts and `"done"` after; while
issues are being walked, `issues` is the URL of the page it will resume
from, so somebody watching a large import can see it move. `state` is the
import job's — `queued`, `running`, `done` or `failed` — and `error` is
the reason, in the words the importer recorded. Reading it needs
`repo:read`.

## What it moves, in what order

1. **Labels**, then **milestones** — issues refer to them, so they have
   to exist first.
2. **Issues, in the order GitHub pages them**, each keeping its
   original number, title, body, open or closed state, timestamps,
   labels and milestone.
3. **Comments** on each issue, in the order they were written, following
   every page of a long conversation rather than the first hundred.

A repository with ten thousand issues is tens of thousands of requests
against a budget of five thousand an hour, so the import is bounded and
resumable: a run walks a fixed number of pages, records where it got to,
and goes back on the queue. A rate limit is not an error; the job waits
as long as GitHub asked and carries on. A server restart loses at most
the page it was on. An interrupted import resumed later updates the rows
it already wrote rather than duplicating them.

## Authors are named, not guessed

An issue or comment keeps its GitHub author as text — `octocat (github)`
— rather than being attributed to an account on your server, however
closely a handle or an address matches. Handles are not identity across
platforms; the person who holds `octocat` here need not be the person who
held it there, and quietly merging the two puts words in somebody's mouth
in a permanent record. A deleted GitHub account arrives with no author.

## Pull requests arrive as issues

GitHub's issues API returns pull requests too, and they are imported as
**issues**, carrying their title, body, conversation, labels and a link to
the original — but not the patchset. An imported pull request is not a
reviewable, landable change here.

That is deliberate. A pull request's head commits live in the
contributor's fork, and for a merged or closed one they frequently no
longer exist anywhere. Reconstructing patchsets from that is guesswork
dressed up as an import, and a review history that is 90% right is worse
than one that is honestly a record. Open pull requests a project still
cares about are re-opened as [changes](code-review.md) here by their
contributors, from a [fork](forks.md).

## When the App cannot read issues

Where the installation does not carry `issues: read`, the import stops
with

```
the GitHub App installation cannot read issues on this repository. It needs the `issues: read` permission — the import has stopped rather than reporting an empty tracker, because those look identical from here
```

in `error`. It does not import an empty list and call it a success — an
import that quietly produces nothing looks exactly like a project that
never had issues. The refusal comes on the first request, before
anything is written, so grant the permission on the installation and
start the import again.

## What it does not do

- **No reactions and no assignees.**
- **No cross-reference rewriting.** A `#123` in an issue body stays as
  written. Because numbers are preserved, it still points at the right
  issue in the same repository.
- **No redirects from old GitHub URLs.** Each imported issue and comment
  keeps its GitHub URL, so a map can be built from it, but the server
  does not serve one.
