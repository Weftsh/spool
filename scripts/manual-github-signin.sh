#!/usr/bin/env bash
# The GitHub sign-in contract, against the real API. A manual gate.
#
# What this proves that CI cannot
# ------------------------------
# Signing up with GitHub skips our own confirmation mail. The entire
# justification for that is one field in one response we have never
# seen: `verified`, on the `primary` entry of `GET /user/emails`. Every
# automated test of it runs against `stratum-testkit`'s `fake_github` —
# the `/user` and `/user/emails` arms, and `fake_user` beside them —
# which we wrote from the documentation and from memory.
#
# That is the exact shape of failure this repository has already paid
# for twice. `get_page` classified a primary rate-limit refusal as a
# permission denial because the fake always attached `Retry-After`, and
# the test named for the case could not fail. A forwarded mirror push
# under `--atomic` reported every innocent sibling of a refused branch
# as refused for its own reason, because the hermetic origin's hook
# could not produce the phrase GitHub really sends
# (`atomic transaction failed`); the assertion was loose enough to pass
# against a phrase nothing emits. A fake encodes what we believe the
# provider does, and a suite built on a fake that is wrong is green
# precisely where the product is broken.
#
# So the beliefs this gate exists to check, all of them marked BELIEF in
# `fake_github.rs`:
#
#   1. `GET /user/emails` answers a **flat array**, each entry carrying
#      `email`, `primary` and `verified` as separate booleans — not a
#      nested object, and not `primary` implying `verified`.
#   2. Exactly one entry is `primary`.
#   3. An App that was never granted the `Email addresses` account
#      permission is answered **403**, not an empty array. An empty
#      array would let a client that ignores the status pass its tests
#      and then read "no proved address" for every person alive.
#   4. `GET /user` carries a numeric `id` that is stable across a login
#      rename — the only field an account here may be keyed on.
#   5. A bad or spent `code` is a **200 carrying an `error` field**, not
#      a 4xx. A client checking only the status would carry an empty
#      token into the next call and read the refusal somewhere it cannot
#      explain it.
#
#   scripts/manual-github-signin.sh authorize   # the real round trip; reads /user and /user/emails
#   scripts/manual-github-signin.sh refused     # a made-up code → 200 with an error field (belief 5)
#   scripts/manual-github-signin.sh noperm      # an App WITHOUT Email addresses → 403 (belief 3)
#   scripts/manual-github-signin.sh fixtures    # the observed bodies → crates/stratum-testkit/fixtures/github-signin
#   scripts/manual-github-signin.sh all
#
# What it needs
# -------------
#   STRATUM_GITHUB_CLIENT_ID         the OAuth client of the App you DEPLOY with
#   STRATUM_GITHUB_CLIENT_SECRET     its secret
#   STRATUM_GITHUB_API_BASE          (optional) default https://api.github.com
#   STRATUM_GITHUB_OAUTH_BASE        (optional) default https://github.com
#   STRATUM_GITHUB_SIGNIN_PORT       (optional) default 8765 — the local callback this script listens on
#   STRATUM_GITHUB_SIGNIN_NOEMAIL_CLIENT_ID      (optional) an App that LACKS the Email addresses permission
#   STRATUM_GITHUB_SIGNIN_NOEMAIL_CLIENT_SECRET  (optional) its secret — without both, `noperm` is a NOTE
#
# One-time setup on the App
# -------------------------
# `authorize` catches the redirect itself, so the App must list
#
#   http://127.0.0.1:8765/callback
#
# as an additional **callback URL** (GitHub Apps allow several). That is
# alongside, not instead of, the deployment's own
# `<STRATUM_PUBLIC_URL>/v1/auth/github/callback`. The App must also hold
# the **`Email addresses` account permission, read-only**, which is the
# permission the product needs and the thing belief 3 is about — run
# this before deciding sign-in is broken, because without it every real
# sign-in lands on `github=noemail` with a sentence that reads like the
# person's GitHub account is at fault.
#
# Run it under the client you DEPLOY with, never a throwaway App: half
# of what is being checked is that the permissions that App actually
# holds admit every call the server makes, and a fresh App with
# everything ticked can never fail that.
#
# If the authorization page refuses
# ---------------------------------
# GitHub validates `redirect_uri` **after** you log in, and compares it
# as an exact string. A registered callback of `127.0.0.1:8765/callback`
# (no scheme) or `http://localhost:8765/callback` (different host) does
# not match `http://127.0.0.1:8765/callback`, and what you get is a page
# whose body reads "Be careful!" — GitHub's warning styling — whose
# *title* is the actual message: **Invalid Redirect URI**. It has no
# authorize button, which reads like a broken page rather than a
# refusal, and it cost an hour of a real session to spot.
#
# The fix, and the form that cannot mismatch:
#
#   STRATUM_GITHUB_SIGNIN_REDIRECT= scripts/manual-github-signin.sh all
#
# Empty omits the parameter entirely, and GitHub then redirects to the
# App's own registered callback. That only works if the registered
# callback points at this script's listener (port 8765 by default);
# otherwise set STRATUM_GITHUB_SIGNIN_REDIRECT to whatever is
# registered, character for character.
#
# STRATUM_GITHUB_SIGNIN_TIMEOUT (default 900s) is how long the callback
# is held open. The default was five minutes until a real run timed out
# while the person was still reading GitHub's warning page.
#
# What it cannot prove
# --------------------
# A person has to be at a browser to approve the authorization, which is
# the part that cannot be automated — a token this script minted through
# an API would be this script's grant, not a person's. `noperm` cannot
# be claimed under a client that holds the permission and says so rather
# than passing quietly, the same way `manual-mirror-push.sh denied`
# refuses to claim its case. Nothing here spends a rate-limit budget
# worth worrying about.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"

if [ $# -lt 1 ]; then
  sed -n '2,112p' "$0" | sed 's/^# \{0,1\}//'
  exit 2
fi

[ -n "${STRATUM_GITHUB_CLIENT_ID:-}" ] || { echo "STRATUM_GITHUB_CLIENT_ID is not set (the OAuth client of the App you deploy with)" >&2; exit 2; }
[ -n "${STRATUM_GITHUB_CLIENT_SECRET:-}" ] || { echo "STRATUM_GITHUB_CLIENT_SECRET is not set" >&2; exit 2; }

# The App's own OAuth client and nothing else. A personal token in the
# environment would let a read quietly succeed under permissions the App
# does not hold, which is the opposite of what this gate is for.
for tok in GITHUB_TOKEN GH_TOKEN; do
  if [ -n "${!tok:-}" ]; then
    echo "refusing: $tok is set. This script authenticates as a person through the App's" >&2
    echo "OAuth client and nothing else; unset it so a step cannot pass under a token" >&2
    echo "that holds permissions the App does not." >&2
    exit 2
  fi
done
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

exec python3 "$here/manual-github-signin/contract.py" "$@"
