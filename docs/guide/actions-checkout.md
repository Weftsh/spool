# Checking out from a mirror in GitHub Actions

`weftsh/checkout` is a drop-in for `actions/checkout` that fetches the
commit from a [mirror on your Spool server](quickstart-mirror.md) instead
of from github.com. Developers keep pushing to GitHub; only the read path
CI takes moves. If the mirror cannot serve the commit, `actions/checkout`
runs instead and the job says why. Adopting it is one line, not a
decision.

```yaml
- uses: weftsh/checkout@v1
  with:
    api-url: https://spool.example.com  # your Spool server
    repository: acme/widget             # the mirror, org/repo
    token: ${{ secrets.WEFT_TOKEN }}    # repo:read on the mirror
```

The action is open source at
[github.com/weftsh/checkout](https://github.com/weftsh/checkout). It was
written for Weft's hosted service, so its `api-url` defaults to that;
against your own server, always set it.

## What you need

1. A mirror. [Mirror a GitHub repository](quickstart-mirror.md) covers
   connecting your server's GitHub App and creating one.
2. A token with `repo:read`, stored as a repository secret. Every
   repository on your server is private, so there is no checkout without
   one. [Minting one](authentication.md#minting-and-revoking-tokens) is
   a single request or a click in Settings; restrict it to the one mirror
   if this is the only thing the token is for.
3. A route from the job to your server. GitHub's own hosted runners reach
   it only if it is on the internet; a GitHub runner you host inside your
   network reaches it wherever it is.

## What it does

The action probes the mirror, then runs one `git fetch` for the commit
that triggered the workflow. The token travels as an `Authorization`
header, never in the URL, and is not written into the checkout: the
resulting `.git/config` carries no credential, the same as
`actions/checkout` with `persist-credentials: false`. `HEAD` is detached
at the commit. Two remotes are left behind: `weft`, where the objects came
from, and `origin`, the GitHub repository, so a later `git push origin`
goes where a developer's push goes.

Because the fetch goes to a mirror, it is served by the
[freshness contract](freshness-contract.md): a commit the mirror does
not have yet is fetched from your origin before the response, or refused
by name. There is no way to get a stale tree.

## Inputs

| Input | Default | Meaning |
|---|---|---|
| `api-url` | Weft's hosted service | Your Spool server's URL. Set it. |
| `repository` | `<org>/<this repo's name>` | The mirror, as `org/repo`. |
| `org` | | The organization, used when `repository` is not given. |
| `token` | | A token with `repo:read` on the mirror. |
| `ref` | `${{ github.sha }}` | The commit to check out, as a commit id. |
| `fetch-depth` | `1` | `1` for the commit alone, `0` for full history. |
| `path` | | Where to put the repository, relative to the workspace. |
| `fallback` | `true` | Run `actions/checkout` when the mirror cannot serve. |
| `timeout-seconds` | `10` | How long to wait for the mirror's answer to the probe. |

Outputs: `source` (`weft` or `fallback`), `commit`, and `reason` when the
fallback ran.

## When the fallback runs

Each of these is a notice on the job with the reason in it:

- The mirror did not answer within `timeout-seconds`, or the token cannot
  see it. A mirror the token cannot read answers as if it did not exist,
  and so does a request with no token.
- The commit is not on the mirror and a synchronous sync of the origin did
  not surface it, or the sync exceeded the freshness budget. The mirror's
  own sentence is in the notice.
- The commit has been superseded on its branch. A mirror serves the tip of
  every branch and tag, plus the commits its compaction has checkpointed;
  a commit a newer push has moved past is refused by name. A job queued
  behind a burst of pushes, or a re-run of an older commit, takes the
  fallback. This is a limit of the serving engine today, not of the
  action, and it is on the list; it never produces a stale tree.
- A `pull_request` event with the default `ref`. There, `github.sha` is a
  merge commit GitHub makes for the run, which no mirror carries. To check
  the PR head out from the mirror:

  ```yaml
  - uses: weftsh/checkout@v1
    with:
      api-url: https://spool.example.com
      repository: acme/widget
      token: ${{ secrets.WEFT_TOKEN }}
      ref: ${{ github.event.pull_request.head.sha }}
  ```

- A `fetch-depth` other than `0` or `1`, or a `ref` that is a branch name
  rather than a commit id. `actions/checkout` resolves those.

Set `fallback: false` on a workflow whose purpose is to prove the mirror;
then any of the above fails the job with the reason instead.
