#!/usr/bin/env bash
# The write-through mirror contract, against real GitHub. A manual gate.
#
# What this proves that CI cannot
# ------------------------------
# A push to a mirror is forwarded to its origin by
# `crates/stratum-server/src/mirror/forward.rs`: `git push --porcelain
# --atomic --no-verify --force-with-lease=<ref>:<old> origin <new>:<ref>`
# from the seed clone, under the installation token the sync fetches
# with, and `classify` reads what came back. Every automated test of it
# pushes to a bare repository on disk over `file://`, whose refusals are
# a `pre-receive` hook we wrote to sound like GitHub. Four things are
# therefore beliefs, marked BELIEF in the fixtures and the hook, and a
# suite built on a belief that is wrong is green precisely where the
# product is broken:
#
#   1. a protected branch is refused with the porcelain reason
#      `protected branch hook declined`, and GitHub's explanation rides
#      on `remote:` lines (`GH006: Protected branch update failed …`);
#   2. under `--atomic`, the commands GitHub did not object to report
#      `atomic transaction failed`, so the report can say which was
#      which. This said `atomic push failure` until the first real run
#      on 2026-09-16, and `classify` knew only that and `atomic push
#      failed` — neither of which the receiving end ever sends. The cost
#      was not cosmetic: every innocent sibling of a refused ref was
#      reported as refused for its own reason, carrying the blocked
#      branch's `GH006` text, so the pusher went looking for a fault in
#      a branch that was fine;
#   3. an installation without `Contents: write` is refused at the
#      transport: `remote: Write access to repository not granted.` and
#      `The requested URL returned error: 403`, with no porcelain line;
#   4. a lease that matches the origin's value permits a non-fast-forward
#      (`+` in the porcelain), and a lease that does not is `stale info`.
#
# This script sends the **same push `forward.rs` sends** — same flags,
# same refspecs, same lease — under the App's own JWT and the
# installation tokens minted from it and nothing else, and asserts that
# GitHub answers with the shapes `classify` reads. What it observes is
# written into `crates/stratum-testkit/fixtures/mirror-push/` by
# `fixtures`, and `mirror_push_fixtures_classify_like_the_fake` in
# `forward.rs` goes red the moment the recorded wire and the classifier
# — or the e2e hook — disagree.
#
#   scripts/manual-mirror-push.sh perms      # GET /app/installations/{id}: contents: write (and the denied one: not)
#   scripts/manual-mirror-push.sh push       # push a new branch with the dispatcher's flags → `*`; delete it → `-`
#   scripts/manual-mirror-push.sh stale      # the same push with a wrong lease → `! … (stale info)` (belief 4)
#   scripts/manual-mirror-push.sh nonff      # a non-fast-forward under a matching lease → `+` (belief 4)
#   scripts/manual-mirror-push.sh protected  # a push to STRATUM_GITHUB_PROTECTED_BRANCH → the reason and the remote lines (beliefs 1, 2)
#   scripts/manual-mirror-push.sh denied     # the same push under an installation WITHOUT contents: write → 403 (belief 3)
#   scripts/manual-mirror-push.sh fixtures   # the observed stdout/stderr → crates/stratum-testkit/fixtures/mirror-push
#   scripts/manual-mirror-push.sh all        # every step above, in order
#
# What it needs
# -------------
#   STRATUM_GITHUB_APP_ID                  the App the deployment authenticates as
#   STRATUM_GITHUB_APP_KEY_PEM             its private key: a path to the PEM, or the PEM itself
#   STRATUM_GITHUB_INSTALLATION_ID         an installation of that App that holds Contents: write
#   STRATUM_GITHUB_MIRROR_REPO             owner/name of a repository it covers; branches named
#                                          weft-manual-<timestamp> are pushed to it and deleted again
#   STRATUM_GITHUB_PROTECTED_BRANCH        (optional) a branch of that repository protected against
#                                          direct pushes; without it `protected` is a NOTE
#   STRATUM_GITHUB_DENIED_INSTALLATION_ID  (optional) an installation of the same App that genuinely
#                                          LACKS Contents: write; without it `denied` is a NOTE
#   STRATUM_GITHUB_DENIED_REPO             (optional) owner/name it covers
#
# Use the App and the installations you deploy with. `denied` cannot
# fail under an installation that holds the permission — the script
# checks first and refuses to claim it. Nothing here uses a personal
# token or `gh auth`.
#
# What it costs to run
# --------------------
# Two or three short-lived branches on the repository, created and
# deleted; the protected branch is never moved (that is what is being
# checked). API budget in the tens of requests.
#
# What it cannot prove
# --------------------
#   * Belief 2 without a branch that is actually protected: `--atomic`
#     needs a refusal to show what the sibling command reports.
#   * Belief 3 without STRATUM_GITHUB_DENIED_INSTALLATION_ID.
#   * Anything about the mirror itself: this talks to GitHub the way
#     the server does, not to the server. The e2e suite covers the rest.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

if [ $# -lt 1 ]; then
  sed -n '2,72p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi

[ -n "${STRATUM_GITHUB_APP_ID:-}" ] || { echo "STRATUM_GITHUB_APP_ID is not set (the App the deployment authenticates as)" >&2; exit 2; }
[ -n "${STRATUM_GITHUB_APP_KEY_PEM:-}" ] || { echo "STRATUM_GITHUB_APP_KEY_PEM is not set (a path to the App's private key, or the PEM itself)" >&2; exit 2; }
case "$STRATUM_GITHUB_APP_KEY_PEM" in
  *"-----BEGIN"*) ;;
  *) [ -r "$STRATUM_GITHUB_APP_KEY_PEM" ] || { echo "cannot read $STRATUM_GITHUB_APP_KEY_PEM" >&2; exit 2; } ;;
esac
[ -n "${STRATUM_GITHUB_INSTALLATION_ID:-}" ] || { echo "STRATUM_GITHUB_INSTALLATION_ID is not set (an installation holding Contents: write)" >&2; exit 2; }
[ -n "${STRATUM_GITHUB_MIRROR_REPO:-}" ] || { echo "STRATUM_GITHUB_MIRROR_REPO is not set (owner/name that installation covers)" >&2; exit 2; }
for tok in GITHUB_TOKEN GH_TOKEN; do
  if [ -n "${!tok:-}" ]; then
    echo "refusing: $tok is set. This script authenticates as the App and nothing else;" >&2
    echo "unset it so a step cannot pass under a person's permissions." >&2
    exit 2
  fi
done
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
command -v openssl >/dev/null || { echo "openssl is required (to sign the App JWT)" >&2; exit 2; }
command -v git >/dev/null || { echo "git is required" >&2; exit 2; }

exec python3 "$here/manual-mirror-push/contract.py" "$@"
