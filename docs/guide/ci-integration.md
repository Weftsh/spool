# CI integration

This page is about **bringing your own CI**: whatever already builds your
code, wherever it runs, signing a verdict back to us. That is one of two
ways to get a check onto a commit here.

The other is [Workflows](workflows.md) — hosted CI, a
`.weft/*.yml` file in the repository, run on our runners, with logs
you can read while the job is still going and `push`/`change` triggers
that start it. If you have no CI yet, start there; it is fewer moving
parts than anything on this page.

The two coexist, and neither replaces the other. Both write into the
same `check_runs` table under the same names, so a hosted `ci / test`
and a Buildkite `ci/tests` sit in one list on the Checks tab, are
required by name the same way, and gate the land queue identically. A
repository can run both. Note that a hosted workflow starts from a
**push** — over HTTPS, over SSH, or through `POST …/commits` — so
commits that arrive in a mirror by syncing from its origin do not start
one; a mirrored repository's verdicts come from the paths on this page.

What Weft does with either is **hold the verdict and act on it**. A
check named `ci/tests` sitting at `failing` blocks the land queue, shows
in review beside the human approvals, and colours the badge in your
README. The rest of this page is how a verdict from *your* CI gets here.

## Which of the three paths you are on

Find yourself here first. Two of the three are short, and most readers of
this page need one paragraph of it.

### 1. Mirrored from GitHub — you do nothing

Its Actions runs are polled in through the same App installation the
commits already come through, and they appear on the repository's
**Checks** tab without a secret, a snippet or a change to your workflow.
Creating the mirror is what starts it: the first sync asks GitHub for
the run history, and every sync after it asks again, so a run that
finishes after the last look is picked up by the next one. **Check
GitHub again** on the Checks tab asks right now instead of waiting for
the next sync.

The one requirement is a permission: the installation needs
**`actions: read`**, which installations created before checks existed do
not have. Until it is approved, the Checks tab says so in as many words —
"we cannot read this project's checks" — rather than showing an empty
list that reads as a project without CI. The button on that page sends
you to GitHub to approve it, and the runs already in GitHub's history
come with it.

That is the whole of it. The rest of this page is for the other two.

### 2. Hosted here, CI somewhere else — four steps, and none of them is optional

This is the case with the most moving parts, because **nothing on Weft
starts a build of yours**. Hosted [workflows](workflows.md) are the
one exception and they run here rather than on your CI; for everything
else, a repository created here is not connected to anything that runs
your code, so the loop has to be closed at both ends: something has
to tell your CI there is work, and something has to bring the verdict
back. Each half is useless without the other, and each is a different
page of these docs — which is why they are listed here in order rather
than left to be assembled.

1. **Subscribe a webhook, so a push reaches your CI.** `POST …/webhooks`,
   or the repository's **Settings → Push webhooks** panel. Deliveries
   fire on `push`, `change.landed` and `change.ejected`, signed with a
   delivery secret shown once. This is the trigger; without it your CI
   never learns that anything happened. See
   [Webhooks](webhooks.md), and read *What a delivery does and does
   not tell you* below before you write the receiver.
2. **Give the runner a credential to clone with.** A token with
   `repo:read` ([Authentication](authentication.md)) or a deploy-style
   SSH key ([SSH](ssh.md)). Your CI fetches from us the same way a
   person does.
3. **Post the verdict back.** The intake secret and the request in the
   rest of this page.
4. **Read it on the Checks tab**, on the change under review, and in the
   badge.

#### What a delivery does and does not tell you

Worth an afternoon to whoever writes the receiver. The envelope is
`{ event, repo_id, payload }`, and the `payload` differs by how the push
arrived:

| The push came in by | `payload` carries |
|---|---|
| `git push` over HTTPS | `{ "via": "git" }` |
| `git push` over SSH | `{ "via": "ssh" }` |
| `POST …/commits` | `{ "via": "api", "commit": …, "branch": … }` |

So an ordinary `git push` — which is how nearly every push arrives —
announces **that** the repository moved and not **what** moved. There is
no branch and no commit in it. A receiver cannot filter by branch at the
hook, and has to fetch and compare to find out what changed. Build for
that rather than discovering it: a workflow keyed on `payload.branch` will
work perfectly in a test against `POST …/commits` and never fire in
production.

`change.landed` and `change.ejected` are the ones to hang a post-landing
step off; they carry the change key, the commit and the target branch.

### 3. Neither of those

Anything that can make an HTTPS request and compute an HMAC can report a
check: GitLab CI, Buildkite, CircleCI, Jenkins, a nightly cron on a
machine under someone's desk. If something else already triggers your
builds, you need only step 3 above. It also works for a mirrored
repository whose real CI is somewhere other than Actions; the paths are
not exclusive.

## Where the verdicts end up

Every path lands in the same three places, which is the point of having
one vocabulary for them:

- the repository's **Checks** tab, newest first, filterable by state;
- the **change** under review, beside the human approvals, where a
  `failing` check blocks the land queue and a **required** one must go
  green before the change may land — see
  [Making a check required](#making-a-check-required);
- the **badge** in your README.

## The shape of it

1. Mint a **per-repo intake secret**, once, and put it in your CI's
   secret store. You can do this from the repository's
   **Settings → CI checks** panel, or with the request below.
2. Your CI posts a small signed JSON body to
   `/v1/orgs/{org}/repos/{repo}/ci/checks` when a build finishes.
3. `GET /v1/orgs/{org}/repos/{repo}/badge.svg` renders the result.

The secret can do exactly one thing: write a check on the latest patchset
of a change. It cannot read your code, approve anything, land anything,
or push. That is the whole reason it exists — the alternative
is pasting a `repo:write` token into a third-party runner, and a token
that can report a build should not also be a token that can delete the
repository.

### What the intake does not do for you, said now rather than later

Worth knowing before you wire anything up, because it is a plan you make
once. None of this is a limit of the product as a whole — hosted
[workflows](workflows.md) do run code, keep logs and start
themselves — it is what the *intake on this page* is and is not:

- **It does not run your code.** A verdict arrives; nothing executes
  here. That is the point of a credential that can only write a check.
- **It does not start your build.** We announce a push over a
  [webhook](webhooks.md) and your CI acts on it. There is no nightly
  timer here to hang one off, for hosted workflows either.
- **It does not hold your logs.** We store the `url` you send and link to
  it; the log lives on your CI and stays there. (A hosted workflow's log
  *is* held here, and streamed.)
- **It has no re-run button.** Re-run it where it ran, and post the new
  verdict.

[What this deliberately is not](#what-this-deliberately-is-not), further
down, has the full table and the reasoning.

## 1. Mint the secret

In the dashboard: the repository's **Settings** tab, **CI checks** panel.
It shows whether a secret is configured and when it last moved, mints one
when you press the button, and offers a confirmed revoke. Only somebody
who may administer the repository sees it at all.

Or over the wire, which is what a setup script wants:

```bash
curl -sS -X POST "$WEFT_URL/v1/orgs/$ORG/repos/$REPO/ci/secret" \
  -H "Authorization: Bearer $TOKEN"
```

```json
{ "secret": "…", "rotated_at": 1717171717171 }
```

**Shown once.** There is no endpoint that reads it back; rotating is the
only way to get a new one, and rotating immediately invalidates whatever
the previous holder had. `GET` on the same path reports whether one is
configured and when it last moved, never the value. `DELETE` revokes it.

Store it as `WEFT_CI_SECRET` in your CI provider's secret store.

## 2. Post the verdict

The request, in shell. Every provider snippet below is this, wrapped in
that provider's YAML:

```bash
BODY=$(printf '{"commit":"%s","name":"ci/tests","state":"%s","ref":"%s","event":"%s","actor":"%s","external_id":"%s","run_number":%s,"url":"%s","summary":"%s","sent_at":%s000}' \
  "$COMMIT" "$STATE" "$REF" "$EVENT" "$ACTOR" "$RUN_ID" "$RUN_NUMBER" "$RUN_URL" "$SUMMARY" "$(date +%s)")
SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$WEFT_CI_SECRET" | sed 's/^.*= //')
curl -sS --fail-with-body -X POST "$WEFT_URL/v1/orgs/$ORG/repos/$REPO/ci/checks" \
  -H "Content-Type: application/json" \
  -H "X-Weft-Signature-256: sha256=$SIG" \
  --data-binary "$BODY"
```

The signature is HMAC-SHA256 over the **raw request body** with the intake
secret, hex-encoded, prefixed `sha256=`. It is the same scheme Weft's
outbound webhooks use, so if you have already written a verifier for
those, you have already written this.

### Two shapes, and `change` is what picks between them

One route, two kinds of report, and which one you are making depends on
whether the body names a `change`:

- **Commit-scoped** — `change` absent. The verdict is about the commit
  alone. **Send this one.** It works on every event including a plain
  push, it fills the repository's **Checks** tab and the badge, and —
  because a change's checks are the union of its patchset's rows and the
  runs reported against that patchset's commit — it also appears on the
  review page and satisfies the land gate. One request covers everything.
- **Change-scoped** — `change` present. The verdict attaches to that
  change's **latest patchset** rather than to a sha. The narrower tool,
  and worth it only when you want a verdict that is a statement about
  *the revision under review*: it cannot be inherited by a rewritten
  commit, and when both shapes report one name the patchset row wins.

If you send both, send them as two requests. They are answered separately
on purpose: a `201` naming a patchset says nothing about whether the run
row was written, so a reporter that wants both can retry whichever
failed.

> A change key is `I` followed by hex, taken from the commit's
> `Change-Id: I…` trailer — and if a commit has no such trailer, Weft
> mints an oid-derived key that your CI cannot compute. It is **never**
> the branch name and never the pull request title; anything else is
> answered `404 no such change`. This is the main reason to prefer the
> commit-scoped shape: it needs no key at all.

### The fields

Shared by both shapes:

| Field | Required | Bound | What it is |
|---|---|---|---|
| `commit` | yes | 40 hex | The commit your run actually built |
| `name` | yes | 100 bytes | The check's name, e.g. `ci/tests` |
| `state` | yes | — | Per shape — see below |
| `sent_at` | yes | — | Your clock, unix **milliseconds** |
| `url` | no | 1000 bytes | Where the run's log lives (http/https) |
| `summary` | no | 2000 bytes | One sentence for a human |

Change-scoped only:

| Field | Required | Bound | What it is |
|---|---|---|---|
| `change` | — | 72 bytes | The change key, e.g. `I1a2d0001`. Its presence is what makes the report change-scoped |

Commit-scoped only — all optional, and all of them are things the Checks
tab shows or filters on, so a report that omits them lands as a row with
blanks where a reader expects a branch and a duration:

| Field | Bound | What it is |
|---|---|---|
| `external_id` | — | **Your** id for the run. Send it. Without one the row is keyed on `(commit, name)`, so a genuine re-run of one workflow overwrites the first instead of updating the right row |
| `ref` | — | The branch or tag it ran for. Named `ref` because that is what git calls it and what your CI's environment already exports |
| `run_number` | — | The counter a person recognises in your CI's own UI. Not an identity here; `external_id` is |
| `event` | — | What triggered it — `push`, `pull_request`, `schedule` |
| `actor` | — | Who it ran for |
| `started_at`, `completed_at` | — | Unix milliseconds. Omit rather than sending `0`, which renders as January 1970 — a confident wrong answer |

Every bound is a refusal, not a truncation: a summary one byte too long
is answered `400`, because a summary silently cut in half is a summary
that lies about what happened.

`name` is the identity of the check. Posting `ci/tests` again replaces the
previous `ci/tests` on that patchset — a check is current state, not a
journal. Post the start of the run and then its real state, and the page
follows along.

### `state`, which differs between the two shapes

This is the one place the shapes genuinely disagree, and it catches
people:

| Shape | Accepted |
|---|---|
| Change-scoped | `pending`, `passing`, `failing` |
| Commit-scoped | `queued`, `running`, `passing`, `failing`, `cancelled`, `skipped` |

Note that **`pending` is change-scoped only** and **`queued`/`running`
are commit-scoped only**. The land gate has three states because a
reviewer only needs to know "not yet, yes, or no"; a repository's history
of runs carries the provider's own words, including the two that mean a
verdict was never reached.

An unrecognised word is refused by name — never read as a default,
because a silent green ships code nobody checked and a silent red blocks
code that is fine. The refusal names the shape it judged you under and
what the other shape would have taken, so if you sent `pending` on a
push you are told where `pending` is legal rather than left to conclude
we have no such state.

### Why `commit` is required

A new patchset starts with no checks at all, and this is the field that
keeps it that way. If your run started on patchset 3 and the author
pushed patchset 4 while it was going, the verdict that arrives is about
code that is no longer under review; Weft answers `409` and names both
commits rather than marking the new patchset green on the strength of a
build of the old one.

For the same reason `sent_at` must be within five minutes of our clock,
and an identical body is accepted only once. A captured request is worth
nothing five minutes later.

### What comes back

| Status | Meaning |
|---|---|
| `201` | Change-scoped only: first report under this name on this patchset |
| `200` | Change-scoped: an existing check of that name was updated. Commit-scoped: every report, first or not — the write is an upsert, re-reporting a run is the ordinary case, and the route does not claim `Created` for what may be an update |
| `400` | A field is malformed or over its bound; the body names it |
| `404` | No such repository, no secret configured, or the signature did not match |
| `409` | Stale commit, replayed body, skewed clock, or an abandoned change |
| `413` | The body is over 16 KiB |

The `404` is deliberately one answer for three different failures. Telling
them apart would let anyone learn whether a private repository exists by
reading a status code, which is the thing every other route here is
careful not to do. If you are debugging a `404`, check all three.

## Provider snippets

All four are the same request with that provider's variable names
substituted, and all four are **commit-scoped**: no change key, works on
a push and on a review alike, and it lands on the Checks tab, the badge
and the review page at once.

The optional fields are not decoration. `ref`, `event` and `actor` are
what the Checks tab filters on, `started_at`/`completed_at` are what its
duration column reads, and `external_id` is what makes a re-run *update*
its row instead of overwriting a different one — without it the row is
keyed on `(commit, name)`.

### GitHub Actions

```yaml
- name: Report to Weft
  if: always()
  env:
    WEFT_URL: https://api.weft.sh
    ORG: acme
    REPO: app
    WEFT_CI_SECRET: ${{ secrets.WEFT_CI_SECRET }}
    COMMIT: ${{ github.sha }}
    STATE: ${{ job.status == 'success' && 'passing' || 'failing' }}
    REF: ${{ github.ref_name }}
    EVENT: ${{ github.event_name }}
    ACTOR: ${{ github.actor }}
    RUN_ID: ${{ github.run_id }}
    RUN_NUMBER: ${{ github.run_number }}
    RUN_URL: ${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}
    SUMMARY: GitHub Actions run ${{ github.run_number }}
  run: |
    BODY=$(printf '{"commit":"%s","name":"ci/tests","state":"%s","ref":"%s","event":"%s","actor":"%s","external_id":"%s","run_number":%s,"url":"%s","summary":"%s","sent_at":%s000}' \
      "$COMMIT" "$STATE" "$REF" "$EVENT" "$ACTOR" "$RUN_ID" "$RUN_NUMBER" "$RUN_URL" "$SUMMARY" "$(date +%s)")
    SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$WEFT_CI_SECRET" | sed 's/^.*= //')
    curl -sS --fail-with-body -X POST "$WEFT_URL/v1/orgs/$ORG/repos/$REPO/ci/checks" \
      -H "Content-Type: application/json" \
      -H "X-Weft-Signature-256: sha256=$SIG" \
      --data-binary "$BODY"
```

### CircleCI

```yaml
- run:
    name: Report to Weft
    when: always
    environment:
      WEFT_URL: https://api.weft.sh
      ORG: acme
      REPO: app
    command: |
      STATE=$([ "$CIRCLE_JOB_STATUS" = "success" ] && echo passing || echo failing)
      COMMIT="$CIRCLE_SHA1"
      REF="$CIRCLE_BRANCH"
      EVENT=push
      ACTOR="$CIRCLE_USERNAME"
      RUN_ID="$CIRCLE_WORKFLOW_JOB_ID"
      RUN_NUMBER="$CIRCLE_BUILD_NUM"
      RUN_URL="$CIRCLE_BUILD_URL"
      SUMMARY="CircleCI build $CIRCLE_BUILD_NUM"
      BODY=$(printf '{"commit":"%s","name":"ci/tests","state":"%s","ref":"%s","event":"%s","actor":"%s","external_id":"%s","run_number":%s,"url":"%s","summary":"%s","sent_at":%s000}' \
        "$COMMIT" "$STATE" "$REF" "$EVENT" "$ACTOR" "$RUN_ID" "$RUN_NUMBER" "$RUN_URL" "$SUMMARY" "$(date +%s)")
      SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$WEFT_CI_SECRET" | sed 's/^.*= //')
      curl -sS --fail-with-body -X POST "$WEFT_URL/v1/orgs/$ORG/repos/$REPO/ci/checks" \
        -H "Content-Type: application/json" \
        -H "X-Weft-Signature-256: sha256=$SIG" \
        --data-binary "$BODY"
```

### Buildkite

```yaml
steps:
  - label: "Report to Weft"
    depends_on: tests
    allow_dependency_failure: true
    env:
      WEFT_URL: https://api.weft.sh
      ORG: acme
      REPO: app
    command: |
      STATE=$(buildkite-agent step get outcome --step tests | grep -q passed && echo passing || echo failing)
      COMMIT="$BUILDKITE_COMMIT"
      REF="$BUILDKITE_BRANCH"
      EVENT="$BUILDKITE_SOURCE"
      ACTOR="$BUILDKITE_BUILD_CREATOR"
      RUN_ID="$BUILDKITE_BUILD_ID"
      RUN_NUMBER="$BUILDKITE_BUILD_NUMBER"
      RUN_URL="$BUILDKITE_BUILD_URL"
      SUMMARY="Buildkite build $BUILDKITE_BUILD_NUMBER"
      BODY=$(printf '{"commit":"%s","name":"ci/tests","state":"%s","ref":"%s","event":"%s","actor":"%s","external_id":"%s","run_number":%s,"url":"%s","summary":"%s","sent_at":%s000}' \
        "$COMMIT" "$STATE" "$REF" "$EVENT" "$ACTOR" "$RUN_ID" "$RUN_NUMBER" "$RUN_URL" "$SUMMARY" "$(date +%s)")
      SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$WEFT_CI_SECRET" | sed 's/^.*= //')
      curl -sS --fail-with-body -X POST "$WEFT_URL/v1/orgs/$ORG/repos/$REPO/ci/checks" \
        -H "Content-Type: application/json" \
        -H "X-Weft-Signature-256: sha256=$SIG" \
        --data-binary "$BODY"
```

### GitLab CI

```yaml
report-to-stratum:
  stage: .post
  when: always
  variables:
    WEFT_URL: https://api.weft.sh
    ORG: acme
    REPO: app
  script:
    - |
      STATE=$([ "$CI_JOB_STATUS" = "success" ] && echo passing || echo failing)
      COMMIT="$CI_COMMIT_SHA"
      REF="$CI_COMMIT_REF_NAME"
      EVENT="$CI_PIPELINE_SOURCE"
      ACTOR="$GITLAB_USER_LOGIN"
      RUN_ID="$CI_PIPELINE_ID"
      RUN_NUMBER="$CI_PIPELINE_IID"
      RUN_URL="$CI_PIPELINE_URL"
      SUMMARY="GitLab pipeline $CI_PIPELINE_IID"
      BODY=$(printf '{"commit":"%s","name":"ci/tests","state":"%s","ref":"%s","event":"%s","actor":"%s","external_id":"%s","run_number":%s,"url":"%s","summary":"%s","sent_at":%s000}' \
        "$COMMIT" "$STATE" "$REF" "$EVENT" "$ACTOR" "$RUN_ID" "$RUN_NUMBER" "$RUN_URL" "$SUMMARY" "$(date +%s)")
      SIG=$(printf '%s' "$BODY" | openssl dgst -sha256 -hmac "$WEFT_CI_SECRET" | sed 's/^.*= //')
      curl -sS --fail-with-body -X POST "$WEFT_URL/v1/orgs/$ORG/repos/$REPO/ci/checks" \
        -H "Content-Type: application/json" \
        -H "X-Weft-Signature-256: sha256=$SIG" \
        --data-binary "$BODY"
```

### Reporting the start of a run too

Optional, and worth it on a slow suite so the review page says "running"
rather than nothing. Send the same body with `"state":"running"` before
the tests, and the same `external_id`, which is what makes the second
report update the first rather than adding a row beside it.

Note the vocabulary: commit-scoped uses `queued` and `running`, where a
change-scoped report would use `pending`. Sending `pending` here is
answered `400`, and the refusal says where `pending` is legal.

### Reporting against the change instead

Only if you want a verdict pinned to the patchset rather than the sha.
Replace the `"ref"`/`"event"`/`"actor"`/`"external_id"`/`"run_number"`
fields with `"change":"$CHANGE"`, use `pending`/`passing`/`failing`, and
derive the key from the commit's own trailer:

```bash
CHANGE=$(git log -1 --format=%B "$COMMIT" | sed -n 's/^Change-Id: *//p' | tail -1)
```

If that comes back empty the commit has no trailer, Weft minted a key
your CI cannot compute, and the commit-scoped report above is the one to
send — which is why it is the default here.

## Making a check required

Everything above gets a verdict onto the page. This is the step that
makes the verdict *matter*, and it is a different rule from the one most
readers assume.

By default a change lands as soon as it is approved. A `failing` check
blocks it — that much is automatic, and needs no configuration. But a
check that has **not reported at all** blocks nothing, because as far as
the gate is concerned there is nothing to fail. "CI must not have failed
yet" and "CI must pass" are different promises, and only the second one
keeps a broken build off your trunk.

Naming a check as required is what turns the first into the second. It
needs a **protected branch** first — on a branch anyone can push to, the
land queue is not the only road in and a requirement is bypassable, so
the request is refused rather than giving you a fence with no field
behind it:

```
409  branch "main" is not protected: protect it first, or a required
     check is one anyone can push past
```

Then:

```sh
curl -sX POST "$STRATUM/v1/orgs/acme/repos/app/required-checks/main" \
  -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' \
  -d '{"name": "ci/tests"}'
```

Or, without the shell: **Settings → Branch policy**, under the protected
branch — which is why the list lives there and not in a picker of its own
— where the field suggests the check names this repository has actually
reported so a typo is harder to make.

The branch rides in the path as a trailing catch-all, so a branch name
with slashes in it (`release/2.0`) goes in whole and unescaped. The check
name rides in the body, because it usually has a slash in it too
(`ci/tests`) and cannot be a path segment. `GET` the same path to list
them; `DELETE …?name=ci/tests` to stop requiring one.

The name is matched against the check's `name`, which is the same
namespace both writers use — the intake and the Actions poller — so
`ci/tests` from your pipeline and `ci/tests` polled from GitHub are the
same requirement.

### Waiting is not blocking

A required check that has not reached a verdict does **not** eject the
change. It holds it in the land queue:

| Required check | The queue |
|---|---|
| `passing` | lands |
| `failing`, `cancelled` | ejects, naming the check |
| `skipped` | ejects — "must pass" does not admit "did not run" |
| `pending`, `queued`, `running` | waits |
| has never reported | waits |

That last row is the ordinary case, not the exotic one: push, press
Land, CI has not started yet. Ejecting there would make you press Land a
second time once the build went green, which is most of what the queue
exists to abolish. The change page says what it is holding for while it
waits.

The hold is bounded. After `STRATUM_LAND_WAIT_SECS` — **30 minutes** by
default, and worth raising for a test matrix that takes two hours — the
queue gives up and ejects with the reason:

```
ejected: waited 30m0s for ci/tests, which never reported
```

which is also how you find out you required `ci/tets`. A required name
nothing ever reports is legal and silent: it holds every change on that
branch until the budget runs out.

## 3. The badge

```markdown
[![build](https://api.weft.sh/v1/orgs/acme/repos/app/badge.svg)](https://api.weft.sh/dashboard/acme/app/changes)
```

Add `?branch=release-2` for a branch other than the default.

Wrap it in a link, as above. A badge nobody can click is a dead end: the
reader has just been told the build is red and has nowhere to go. Both
GitHub's own badge generator and shields.io hand you linked Markdown for
the same reason.

It is drawn to the same geometry as every other badge in your README —
20px tall, 3px corners, 11px Verdana, 5px of padding either side of each
word — so `build | passing` comes out 88 pixels wide, which is exactly
what shields.io serves. It will not be the odd one out in the row.

The SVG is rendered by Weft. There is no shields.io in the path and no
outbound request of any kind — a badge is fetched by every reader of your
README, and that is not a log to hand to a third party.

It reports the checks on the **most recent change that landed on the
branch**, which is the last thing to reach that branch through review:

| Colour | Meaning |
|---|---|
| green `passing` | Every check on that change passed |
| red `failing` | At least one failed |
| amber `pending` | Something is still running |
| grey `no status` | Nothing has landed on that branch, or nothing reported |

An open change never colours the branch's badge — it is not on the branch
yet, and a red review making trunk look broken is the expensive direction
to be wrong in. A branch with nothing landed is honestly grey, never
green.

**Post-landing runs work, and they are why the badge can go red at all.**
Landing already refuses a change with a failing check, so a badge that
only ever looked at what passed on the way in could never turn red. Point
your nightly or post-landing suite at the landed change — naming its landed
commit, which is the branch tip — and trunk breaking after the fact shows
up where readers see it.

A public repository's badge is readable by anyone, which is the point. A
**private** repository's badge is masked exactly as the repository is: a
stranger gets the same answer they would get for a repository that does
not exist. There is deliberately no badge that says "private" — that
badge would confirm the repository exists.

The badge carries `Cache-Control: max-age=60`. A badge that caches for a
day is a badge that lies.

## What this deliberately is not

Through this intake, Weft holds a verdict and not a *run*. Your build
happened somewhere we cannot see, and the fields in the request are
everything we will ever know about it. If you are coming from GitHub
Actions, here is what stays on your CI's side, so you can plan around it
rather than discover it — with, for contrast, what a hosted
[workflow](workflows.md) has, since that one *does* run here:

| On GitHub, a failing check offers | Through this intake | With a hosted workflow |
|---|---|---|
| Re-run job / re-run failed jobs | Nothing. Re-run it where it ran, and post the new verdict. | Not yet. Push again, or cancel and push again. |
| Full logs, searchable, per step | Nothing. `url` points at your CI's log page. | The whole log, `text/plain` or streamed live as it is written. |
| Inline annotations on the diff | Nothing. A `summary` sentence, kept in the audit trail. | Nothing. |
| Build artifacts | Nothing. | Nothing. |
| A job-summary panel the run writes | Nothing. | Nothing. |
| The workflow graph, per-job timing | Nothing. | The run's jobs, their `needs`, and each job's start and finish, over the API. |

Everything in the middle column needs us to be running your code, and
through this path we are not. What this path gives you instead is the
part your CI cannot do for itself: the verdict gating the land queue,
sitting beside the human approvals, on a credential that cannot touch
your repository.

One consequence worth stating plainly: **the `url` field is the whole
escape hatch.** It is the only route from a red check back to the thing
that went wrong, so a delivery without one leaves a reviewer with the
word "failing" and nowhere to go. Set it. (A hosted workflow's rows fill
it in for themselves: they link to the run's page here, which is where
its log lives.)

## Verifying deliveries the other way

If you want Weft to tell *your* CI when something happens, rather than
the other way round, that is [webhooks](webhooks.md) — outbound push
events, signed with the same `X-Weft-Signature-256` scheme. The two
directions use one signing scheme on purpose.

Checks feed the land queue: see
[Changes, OWNERS &amp; landing](code-review.md) for how a failing check
interacts with human approvals.
