# Forwarded-push answers

What `git push --porcelain --atomic --no-verify --force-with-lease=…`
prints when a mirror's push is forwarded to real GitHub — the exact
command `crates/stratum-server/src/mirror/forward.rs::push_to_origin`
runs — for every answer `classify` reads. Replayed by
`mirror_push_fixtures_classify_like_the_fake` in `forward.rs`, which
also holds the e2e suite's `pre-receive` hook (`tests/mirror_e2e.rs`)
to the recorded remote wording.

**Provenance: derived from the docs, not yet observed.** `provenance.json`
says `"observed": false`. Five things here are beliefs — the porcelain
reason for a protected branch, the `GH006` remote line, what the
sibling of a refused command reports under `--atomic`, the transport's
403 for an installation without `Contents: write`, and `stale info` for
a lease the origin does not hold. `scripts/manual-mirror-push.sh
fixtures` overwrites every file with what real GitHub printed and sets
`observed: true`; until it has been run, the fixture test pins the
classifier to a belief, and says so.

One file pair per case, `<case>.stdout` (the porcelain) and
`<case>.stderr` (the remote and the transport), with the commands that
were sent in `provenance.json`. Tokens are redacted by the script.
