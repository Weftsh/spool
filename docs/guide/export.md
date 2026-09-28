# Export

Everything on your server is ordinary git, and getting it out — for a
backup, an archive, or a move — has first-class API support. Two doors
are always open:

## Door one: it's git

Every repo — including every repo your agents created — is a standard
git remote:

```bash
git clone https://x:$TOKEN@spool.example.com/acme/session-8412.git
```

Whatever you clone is verified history; the project's CI gates every
serving change on `git fsck --full --strict` of the produced clone.

## Door two: bundles

For bulk moves or archives, export server-side bundles:

```bash
# start the export (async)
curl -X POST …/repos/session-8412/export -H "Authorization: Bearer $TOKEN"
# → { "job": "01jm…", "state": "queued" }

# poll, then download
curl …/repos/session-8412/export/01jm…
curl -OJ …/repos/session-8412/export/01jm…/download
```

The artifact is a plain `git bundle` — clone it with nothing but git,
anywhere:

```bash
git clone session-8412.bundle restored/
```

Bundles are built from a freshly materialized, fsck-verified copy of the
layout, so an export is also an integrity check.

## The whole org at once

```bash
curl -X POST https://spool.example.com/v1/orgs/acme/export \
  -H "Authorization: Bearer $ADMIN_TOKEN"
```

starts one export job per active repo and returns all job ids. Point a
script at the job list and you have a complete, standard-format copy of
everything.
