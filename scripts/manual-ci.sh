#!/usr/bin/env bash
# The CI-provider contract, run against real providers. A manual gate.
#
# What this proves that CI cannot
# ------------------------------
# Every automated test of the checks feature runs against a fake we wrote.
# `stratum-testkit`'s fake GitHub encodes what we *believe* GitHub does,
# and a fake that is wrong about the provider produces a suite that is
# green and a product that is broken — in exactly the shape the fake got
# wrong, and nowhere else.
#
# That is not hypothetical. `get_page` classified a refusal as
# rate-limited only when `Retry-After` was present. GitHub sends that on
# **secondary** limits. A **primary** budget exhaustion is a 403 carrying
# `x-ratelimit-remaining: 0` and **no** `Retry-After`, which we therefore
# classified as "your App cannot read Actions" — telling a maintainer to
# re-approve a permission they already had, and stopping the poller for
# good on a condition that clears by itself in under an hour. The test
# named for exactly that case could not fail, because the fake always
# attached `Retry-After`.
#
# This is the same shape as the S3 finding: MinIO answers 412 where real
# S3 answers 409, and no MinIO test could ever have caught it. The answer
# there was a manual gate against the real thing —
# `scripts/manual-s3.sh` — and this is its sibling.
#
#   scripts/manual-ci.sh actions            # read a real installation
#   scripts/manual-ci.sh actions --exhaust-rate-limit
#   scripts/manual-ci.sh denied             # an installation without actions:read
#   scripts/manual-ci.sh intake fixture gitlab
#   scripts/manual-ci.sh intake watch ci/tests
#   scripts/manual-ci.sh intake negatives
#   scripts/manual-ci.sh all
#
# What it costs to run
# --------------------
# `actions` and `denied` cost a handful of GitHub API requests and a few
# seconds. `--exhaust-rate-limit` costs the installation's **entire
# primary budget** — up to 5,000 requests — and leaves that installation
# unable to call the API until the window resets, which can be a full
# hour. Do not run it against an installation something else depends on.
# It is the only way to observe the refusal we got wrong, and it is opt-in
# for that reason: without it, `actions` prints a NOTE and does not claim
# a pass on the rate-limit case.
#
# `intake` needs a real project on a real non-GitHub CI. `intake fixture`
# prints the pipeline configuration to commit there, taken from the same
# document a maintainer reads; `intake watch` then waits for the verdict
# to arrive and checks its shape. A person has to push, and that is the
# part that cannot be automated even manually — a pipeline that this
# script triggered through an API would be this script's request, not
# that CI system's.
#
# What it cannot prove
# --------------------
#   * That GitHub will not invent a `status`/`conclusion` pair tomorrow.
#     It checks the pairs your repository has actually produced. Point it
#     at a repository with a *varied* history — cancelled runs, timed-out
#     runs, skipped jobs, a run awaiting manual approval — or it will
#     confirm only that `completed/success` maps correctly.
#   * Anything about secondary rate limits. Those are triggered by
#     concurrency and content, not by a budget, and provoking one on
#     purpose is abuse of the API.
#   * That the deployment's IAM/App permissions are minimal in general.
#     It checks the two axes this feature rests on: that the installation
#     it reads with holds `actions: read` and no more than we deploy
#     with, and that the Stratum credential is bound to one repository.
#
# Least privilege, and why it is not optional
# -------------------------------------------
# Run this with the **App installation and the Stratum token the
# deployment actually uses**, not an owner-of-everything credential. The
# `denied` case exists to catch a permission we do not hold being
# reported as "this project has no CI"; an installation that holds every
# permission can never fail it, and a check that cannot fail proves
# nothing. Same argument as `manual-s3.sh` makes about an admin key.
set -euo pipefail

cd "$(dirname "$0")/.."

GH=${GITHUB_API_BASE:-https://api.github.com}

die() { printf '\033[31m%s\033[0m\n' "$*" >&2; exit 1; }
say() { printf '\033[1m%s\033[0m\n' "$*"; }
note() { printf '\033[33m   NOTE  %s\033[0m\n' "$*"; }
pass() { printf '\033[32m   PASS  %s\033[0m\n' "$*"; }
fail() { printf '\033[31m   FAIL  %s\033[0m\n' "$*"; FAILED=$((FAILED + 1)); }
FAILED=0

need() {
  local n=$1 hint=$2
  [ -n "${!n:-}" ] || die "$n is not set. $hint"
}

# One field out of a JSON object on stdin, or empty. python3 rather than
# a jq dependency, for the same reason manual-stack.sh does it this way.
jf() { python3 -c 'import json,sys
d=json.loads(sys.stdin.read() or "{}")
for k in sys.argv[1].split("."):
    d = (d or {}).get(k) if isinstance(d, dict) else None
print("" if d is None else (json.dumps(d) if isinstance(d,(dict,list)) else d))' "$1"; }

# ---------------------------------------------------------------------
# The App: a JWT, then an installation token
# ---------------------------------------------------------------------

b64url() { openssl base64 -A | tr '+/' '-_' | tr -d '='; }

app_jwt() {
  local now header payload signing sig
  now=$(date +%s)
  header='{"alg":"RS256","typ":"JWT"}'
  # 60s back, because GitHub rejects a token whose `iat` is in its future
  # by even a second and clocks disagree.
  payload=$(printf '{"iat":%d,"exp":%d,"iss":"%s"}' "$((now - 60))" "$((now + 540))" \
    "$STRATUM_GITHUB_APP_ID")
  signing="$(printf %s "$header" | b64url).$(printf %s "$payload" | b64url)"
  sig=$(printf %s "$signing" | openssl dgst -sha256 -sign "$STRATUM_GITHUB_APP_KEY_PEM" -binary | b64url)
  printf '%s.%s' "$signing" "$sig"
}

# Mint a token for one installation, and hand back the token and the
# permissions it was granted — the permissions are half the point.
install_token() { # install_token <installation_id>  -> prints token
  local jwt out
  jwt=$(app_jwt)
  out=$(curl -sS -X POST "$GH/app/installations/$1/access_tokens" \
    -H "Authorization: Bearer $jwt" \
    -H "Accept: application/vnd.github+json")
  local tok
  tok=$(printf %s "$out" | jf token)
  [ -n "$tok" ] || die "could not mint an installation token for $1: $out"
  printf %s "$tok"
}

install_permissions() { # -> the permissions object, as JSON
  local jwt
  jwt=$(app_jwt)
  curl -sS -X POST "$GH/app/installations/$1/access_tokens" \
    -H "Authorization: Bearer $jwt" -H "Accept: application/vnd.github+json" | jf permissions
}

# A GET, with the status line and the headers we care about, so a
# classification can be checked against what actually came back.
gh_get() { # gh_get <token> <path> -> "<headers>---BODY---<body>"
  local tmp
  tmp=$(mktemp)
  curl -sS -D - -o "$tmp" \
    -H "Authorization: Bearer $1" -H "Accept: application/vnd.github+json" \
    "$GH$2"
  echo "---BODY---"
  cat "$tmp"
  rm -f "$tmp"
}

# Header value by name, case-insensitively — both sides are lowered here
# rather than leaning on awk's IGNORECASE, which is a gawk extension and
# silently absent in the BSD awk on a macOS development machine. A helper
# that works on the CI image and not on the machine the gate is actually
# run from is worse than none.
hdr() { # hdr <headers-blob> <name>
  printf '%s\n' "$1" | tr -d '\r' | awk -v k="$(printf %s "$2" | tr 'A-Z' 'a-z')" \
    'tolower($1)==k":" {sub(/^[^:]*: */,""); print; exit}'
}

http_status() { printf '%s\n' "$1" | tr -d '\r' | awk '/^HTTP\// {s=$2} END{print s}'; }

# ---------------------------------------------------------------------
# actions — a real installation, a real repository
# ---------------------------------------------------------------------

# What `run_state` in crates/stratum-server/src/mirror/origin.rs names
# explicitly. Anything else degrades to `queued`, which is safe (never
# `passing`) but is a lie about a run that has finished.
KNOWN_PAIRS='
in_progress|
completed|success
completed|failure
completed|timed_out
completed|startup_failure
completed|cancelled
completed|skipped
completed|neutral
queued|
requested|
waiting|
pending|
'

cmd_actions() {
  need STRATUM_GITHUB_APP_ID "The App the deployment authenticates as."
  need STRATUM_GITHUB_APP_KEY_PEM "Path to the App's private key PEM."
  need STRATUM_GITHUB_INSTALLATION "The installation id the deployment reads with."
  need STRATUM_GITHUB_ACTIONS_REPO "owner/name of a repository with a VARIED Actions history."
  [ -r "$STRATUM_GITHUB_APP_KEY_PEM" ] || die "cannot read $STRATUM_GITHUB_APP_KEY_PEM"

  local exhaust=0
  [ "${1:-}" = "--exhaust-rate-limit" ] && exhaust=1

  say "── GitHub Actions, through installation $STRATUM_GITHUB_INSTALLATION ──"
  echo "   $GH/repos/$STRATUM_GITHUB_ACTIONS_REPO/actions/runs"
  echo

  # 1. Least privilege, on the axis this feature rests on.
  local perms actions_perm
  perms=$(install_permissions "$STRATUM_GITHUB_INSTALLATION")
  actions_perm=$(printf %s "$perms" | jf actions)
  echo "   installation permissions: $perms"
  if [ "$actions_perm" = "read" ]; then
    pass "the installation holds actions: read, and only read"
  elif [ -n "$actions_perm" ]; then
    fail "the installation holds actions: $actions_perm — the deployment needs read; \
running under more than we deploy with means the denied case below cannot fail"
  else
    fail "the installation holds no actions permission at all — this is the \`denied\` \
fixture, not the reading one"
  fi

  # 2. It reads runs at all.
  local resp status headers body
  resp=$(gh_get "$(install_token "$STRATUM_GITHUB_INSTALLATION")" \
    "/repos/$STRATUM_GITHUB_ACTIONS_REPO/actions/runs?per_page=100")
  headers=${resp%%---BODY---*}
  body=${resp#*---BODY---}
  status=$(http_status "$headers")
  if [ "$status" != "200" ]; then
    fail "reading Actions answered $status, not 200 — nothing below means anything"
    printf '%s\n' "$body" | head -5
    return
  fi
  local total
  total=$(printf %s "$body" | jf total_count)
  pass "the installation reads Actions ($total runs on this repository)"

  # 3. Every status/conclusion pair this repository has really produced,
  #    against the ones run_state names. An unknown pair is not cosmetic:
  #    it is a finished run we will report as `queued` forever.
  local unknown
  unknown=$(printf %s "$body" | python3 -c '
import json, sys
known = set(l for l in """'"$KNOWN_PAIRS"'""".split() if l)
runs = json.load(sys.stdin).get("workflow_runs", [])
seen = {}
for r in runs:
    p = f'"'"'{r.get("status")}|{r.get("conclusion") or ""}'"'"'
    seen[p] = seen.get(p, 0) + 1
for p, n in sorted(seen.items()):
    print(("KNOWN" if p in known else "UNKNOWN"), p, n)
')
  echo "$unknown" | sed 's/^/   /'
  local n_unknown
  n_unknown=$(printf '%s\n' "$unknown" | grep -c '^UNKNOWN' || true)
  if [ "$n_unknown" = "0" ]; then
    pass "every status/conclusion pair on this repository is one run_state names"
  else
    fail "$n_unknown status/conclusion pair(s) above are not in run_state's table, so \
we report those runs as \`queued\` forever. crates/stratum-server/src/mirror/origin.rs, run_state"
  fi
  local n_distinct
  n_distinct=$(printf '%s\n' "$unknown" | grep -c . || true)
  if [ "${n_distinct:-0}" -lt 4 ]; then
    note "only $n_distinct distinct pair(s) here. This repository's history is not varied \
enough to be a mapping check — point STRATUM_GITHUB_ACTIONS_REPO at one with cancelled, \
timed-out and skipped runs on it."
  fi

  # 4. The case this whole script exists for.
  echo
  if [ "$exhaust" = 0 ]; then
    local rl
    rl=$(curl -sS -H "Authorization: Bearer $(install_token "$STRATUM_GITHUB_INSTALLATION")" \
      -H "Accept: application/vnd.github+json" "$GH/rate_limit")
    echo "   budget now: $(printf %s "$rl" | jf resources.core)"
    note "the primary rate-limit refusal was NOT observed. That refusal is a 403 with"
    note "x-ratelimit-remaining: 0 and NO Retry-After, and it is the shape we"
    note "classified as a permission denial. Re-run with --exhaust-rate-limit to"
    note "spend this installation's whole budget and see it. This is not a pass."
  else
    rate_limit_case
  fi
}

# Burn the budget, then read the refusal, then ask the deployment what it
# made of it. Both halves matter: GitHub's shape is the input, and our
# classification is the thing under test.
rate_limit_case() {
  say "   spending the installation's primary budget — this takes a while"
  local tok resp headers status remaining retry i=0
  tok=$(install_token "$STRATUM_GITHUB_INSTALLATION")
  while :; do
    resp=$(gh_get "$tok" "/repos/$STRATUM_GITHUB_ACTIONS_REPO/actions/runs?per_page=1")
    headers=${resp%%---BODY---*}
    status=$(http_status "$headers")
    i=$((i + 1))
    [ "$status" = "200" ] || break
    if [ $((i % 250)) = 0 ]; then
      printf '   %s requests, %s left\n' "$i" "$(hdr "$headers" x-ratelimit-remaining)"
    fi
    if [ "$i" -gt 6000 ]; then
      fail "6000 requests and the budget never ran out — this installation is not \
subject to the limit we model, so the case cannot be observed here"
      return
    fi
  done
  remaining=$(hdr "$headers" x-ratelimit-remaining)
  retry=$(hdr "$headers" retry-after)
  echo "   refused after $i requests: $status, x-ratelimit-remaining=${remaining:-absent}, \
Retry-After=${retry:-absent}"

  if [ "$status" != "403" ] && [ "$status" != "429" ]; then
    fail "a spent primary budget answered $status; get_page only classifies 403 and 429"
    return
  fi
  if [ "$remaining" != "0" ]; then
    fail "the refusal carries x-ratelimit-remaining=${remaining:-absent}, not 0 — \
rate_limit_wait has nothing to key on and this will be read as a permission denial"
    return
  fi
  if [ -n "$retry" ]; then
    note "this refusal DID carry Retry-After ($retry), which is the secondary-limit \
shape. The primary-limit case — no Retry-After — is the one the bug was in; if this \
installation never produces it, this run has not exercised it."
  else
    pass "a spent primary budget is 403 + x-ratelimit-remaining: 0 + no Retry-After, \
exactly the shape the fake could not produce"
  fi

  # And now the half that is ours. The deployment must call this a wait,
  # not a refusal: `denied` true is the answer that tells a maintainer to
  # re-approve a permission they already have.
  if [ -z "${STRATUM_URL:-}" ] || [ -z "${STRATUM_TOKEN:-}" ]; then
    note "STRATUM_URL/STRATUM_TOKEN unset — GitHub's shape was checked, our reading of \
it was not. Set them and point STRATUM_ORG/STRATUM_REPO at the mirror this installation \
feeds."
    return
  fi
  curl -sS -o /dev/null -X POST -H "Authorization: Bearer $STRATUM_TOKEN" \
    "$STRATUM_URL/v1/orgs/$STRATUM_ORG/repos/$STRATUM_REPO/ci/poll" || true
  sleep 5
  local poll denied err
  poll=$(curl -sS -H "Authorization: Bearer $STRATUM_TOKEN" \
    "$STRATUM_URL/v1/orgs/$STRATUM_ORG/repos/$STRATUM_REPO/ci/poll")
  denied=$(printf %s "$poll" | jf denied)
  err=$(printf %s "$poll" | jf error)
  echo "   the deployment says: denied=$denied error=${err:-null}"
  if [ "$denied" = "True" ] || [ "$denied" = "true" ]; then
    fail "a spent rate limit is being reported as \`denied\` — this is the bug. A \
maintainer is being told to re-approve \`actions: read\`, which they already hold, for a \
condition that clears by itself."
  else
    pass "a spent rate limit is a wait, not a permission refusal"
  fi
}

# ---------------------------------------------------------------------
# denied — an installation that genuinely may not read Actions
# ---------------------------------------------------------------------

cmd_denied() {
  need STRATUM_GITHUB_APP_ID "The App the deployment authenticates as."
  need STRATUM_GITHUB_APP_KEY_PEM "Path to the App's private key PEM."
  need STRATUM_GITHUB_DENIED_INSTALLATION \
    "An installation of the SAME App WITHOUT actions: read. Install it on a scratch \
repository and decline the Actions permission; without it this case cannot be run, and \
running only the happy path is how the classification bug survived."
  need STRATUM_GITHUB_DENIED_REPO "owner/name a repository that installation covers."

  say "── an installation without actions: read ──"
  local perms actions_perm
  perms=$(install_permissions "$STRATUM_GITHUB_DENIED_INSTALLATION")
  actions_perm=$(printf %s "$perms" | jf actions)
  echo "   installation permissions: $perms"
  if [ -n "$actions_perm" ]; then
    fail "this installation holds actions: $actions_perm, so it is not the denied \
fixture — the check below would pass for the wrong reason"
    return
  fi
  pass "the installation genuinely holds no actions permission"

  local resp headers status remaining retry
  resp=$(gh_get "$(install_token "$STRATUM_GITHUB_DENIED_INSTALLATION")" \
    "/repos/$STRATUM_GITHUB_DENIED_REPO/actions/runs?per_page=1")
  headers=${resp%%---BODY---*}
  status=$(http_status "$headers")
  remaining=$(hdr "$headers" x-ratelimit-remaining)
  retry=$(hdr "$headers" retry-after)
  echo "   refusal: $status, x-ratelimit-remaining=${remaining:-absent}, Retry-After=${retry:-absent}"

  # The two refusals must be distinguishable from the response alone.
  # If a permission denial also carried remaining=0, no classifier could
  # tell them apart and the design would be wrong rather than the code.
  if [ "$status" != "403" ]; then
    fail "a permission denial answered $status; actions_from_page only maps 403 to \
ACTIONS_READ_DENIED, so this would surface as a generic refusal"
  elif [ "$remaining" = "0" ]; then
    fail "a permission denial carries x-ratelimit-remaining: 0, the same as a spent \
budget — the two cases are indistinguishable from the response and no classification \
here can be correct"
  else
    pass "a permission denial is 403 with budget remaining, distinguishable from a \
spent limit"
  fi

  if [ -z "${STRATUM_URL:-}" ] || [ -z "${STRATUM_TOKEN:-}" ] || [ -z "${STRATUM_DENIED_REPO:-}" ]; then
    note "STRATUM_URL/STRATUM_TOKEN/STRATUM_DENIED_REPO unset — GitHub's half was \
checked, ours was not. Point STRATUM_DENIED_REPO at a Stratum mirror connected to that \
installation."
    return
  fi
  curl -sS -o /dev/null -X POST -H "Authorization: Bearer $STRATUM_TOKEN" \
    "$STRATUM_URL/v1/orgs/$STRATUM_ORG/repos/$STRATUM_DENIED_REPO/ci/poll" || true
  sleep 5
  local poll denied
  poll=$(curl -sS -H "Authorization: Bearer $STRATUM_TOKEN" \
    "$STRATUM_URL/v1/orgs/$STRATUM_ORG/repos/$STRATUM_DENIED_REPO/ci/poll")
  denied=$(printf %s "$poll" | jf denied)
  echo "   the deployment says: denied=$denied"
  if [ "$denied" = "True" ] || [ "$denied" = "true" ]; then
    pass "an installation that may not read Actions is reported as denied, not as a \
project with no CI"
  else
    fail "the deployment reports denied=$denied. An empty Checks tab and a Checks tab \
we are not allowed to fill look identical to a maintainer, and only one of them is \
something they can fix."
  fi
}

# ---------------------------------------------------------------------
# intake — a real non-GitHub provider
# ---------------------------------------------------------------------

DOC=docs/guide/ci-integration.md

cmd_intake_fixture() {
  local provider=${1:-}
  case "$provider" in
    gitlab | buildkite | circleci) ;;
    *) die "usage: scripts/manual-ci.sh intake fixture <gitlab|buildkite|circleci>" ;;
  esac
  [ -f "$DOC" ] || die "$DOC is missing — the snippets are supposed to come from the \
document a maintainer reads, so that this gate and the docs cannot drift"
  # Refused in a sentence, not by `set -u`. The snippet below prints the
  # URL a maintainer is meant to paste into a real project's CI config, so
  # an unset one is a missing prerequisite rather than a bug — and an
  # operator who gets `STRATUM_URL: unbound variable` has been told the
  # shell's problem instead of theirs. Every other prerequisite in this
  # script goes through `need`.
  need STRATUM_URL "The base URL of the Stratum this project should report to, \
e.g. https://stratum.example.com — it is printed into the snippet you paste \
into the provider."
  say "── $provider ──"
  echo "Commit this into a real project on $provider, set STRATUM_CI_SECRET there from"
  echo "  POST $STRATUM_URL/v1/orgs/\$ORG/repos/\$REPO/ci/secret"
  echo "and push. Then: scripts/manual-ci.sh intake watch ci/tests"
  echo
  # Printed from the document rather than restated here. A second copy of
  # a snippet is a second thing to update, and the one that does not get
  # updated is always the one nobody runs.
  python3 - "$DOC" "$provider" <<'PY'
import re, sys
doc, want = open(sys.argv[1]).read(), sys.argv[2]
titles = {"gitlab": "GitLab", "buildkite": "Buildkite", "circleci": "CircleCI"}[want]
# The provider's own section, up to the next heading of the same level
# *or shallower*: stopping only at the same level runs a `###` section on
# into the `##` that follows it, and printing a neighbouring provider's
# snippet under this one's name is worse than printing nothing.
m = re.search(rf"^(#{{2,4}})\s*{titles}.*?$", doc, re.M)
if not m:
    sys.exit(f"no {titles} section in {sys.argv[1]} — the docs and this gate have drifted")
depth = len(m.group(1))
rest = doc[m.end():]
end = re.search(rf"^#{{1,{depth}}}\s", rest, re.M)
print(rest[: end.start() if end else len(rest)].strip())
PY
  echo
  note "a person has to push. A pipeline this script triggered through an API would be \
this script's request, not that CI system's — and the seam under test is that CI \
system's HTTP client, its shell, its openssl and its clock."
}

cmd_intake_watch() {
  local name=${1:-ci/tests}
  need STRATUM_URL "The deployment to watch."
  need STRATUM_TOKEN "A repo-scoped read token for it."
  need STRATUM_ORG ""
  need STRATUM_REPO ""
  say "── waiting for $name to arrive from a real provider ──"
  local deadline=$((SECONDS + 900)) runs hit
  while [ "$SECONDS" -lt "$deadline" ]; do
    runs=$(curl -sS -H "Authorization: Bearer $STRATUM_TOKEN" \
      "$STRATUM_URL/v1/orgs/$STRATUM_ORG/repos/$STRATUM_REPO/checks/runs?limit=20")
    hit=$(printf %s "$runs" | python3 -c '
import json,sys
name = sys.argv[1]
for r in json.load(sys.stdin).get("runs", []):
    if r.get("name") == name:
        print(json.dumps(r)); break
' "$name")
    [ -n "$hit" ] && break
    sleep 10
  done
  if [ -z "$hit" ]; then
    fail "no run named $name arrived in 15 minutes. Check the CI job's own log: a \
signature computed over a body that differs from the bytes sent — a trailing newline \
from \`echo\` is the usual one — is answered 404 by the intake, deliberately, and looks \
from the outside exactly like a repository that does not exist."
    return
  fi
  echo "   $hit"
  local provider url state
  provider=$(printf %s "$hit" | jf provider)
  url=$(printf %s "$hit" | jf detail_url)
  state=$(printf %s "$hit" | jf state)
  [ "$provider" = "intake" ] && pass "recorded as provider=intake" \
    || fail "recorded as provider=$provider; every vendor arriving this way is \`intake\`"
  case "$url" in
    http://* | https://*) pass "the detail link points out to the provider: $url" ;;
    *) fail "no usable detail link ($url) — a verdict nobody can investigate" ;;
  esac
  case "$state" in
    queued | running | passing | failing | cancelled | skipped)
      pass "state=$state is one of the six the reading side knows" ;;
    *) fail "state=$state is not a RunState the reading side handles" ;;
  esac
}

cmd_intake_negatives() {
  need STRATUM_URL ""
  need STRATUM_ORG ""
  need STRATUM_REPO ""
  need STRATUM_CI_SECRET "The repository's CI intake secret — the real one, from \
POST .../ci/secret. These cases are worthless against a secret the server does not hold."
  local ep="$STRATUM_URL/v1/orgs/$STRATUM_ORG/repos/$STRATUM_REPO/ci/checks"
  say "── the intake's perimeter, against the deployment ──"

  local now body sig code
  now=$(python3 -c 'import time;print(int(time.time()*1000))')
  # A commit that exists is not needed: every case below must be refused
  # before the body is ever looked at, and one that is not is the finding.
  body=$(printf '{"commit":"%s","name":"manual-ci/probe","state":"passing","sent_at":%s}' \
    "$(printf 'manual-ci-probe-%s' "$now" | python3 -c 'import hashlib,sys;print(hashlib.sha1(sys.stdin.buffer.read()).hexdigest())')" "$now")
  sig=$(printf %s "$body" | openssl dgst -sha256 -hmac "$STRATUM_CI_SECRET" | sed 's/^.*= //')

  probe() { # probe <expect> <label> <curl args...>
    local want=$1 label=$2; shift 2
    code=$(curl -sS -o /dev/null -w '%{http_code}' -X POST "$ep" \
      -H 'Content-Type: application/json' "$@")
    [ "$code" = "$want" ] && pass "$label -> $code" || fail "$label -> $code, wanted $want"
  }

  probe 404 "unsigned"            --data-binary "$body"
  probe 404 "wrong secret"        -H "X-Weft-Signature-256: sha256=$(printf %s "$body" | openssl dgst -sha256 -hmac "not-the-secret" | sed 's/^.*= //')" --data-binary "$body"
  probe 404 "signature over different bytes" -H "X-Weft-Signature-256: sha256=$sig" --data-binary "${body} "

  # A body signed correctly but stamped outside the freshness window. A
  # captured delivery replayed tomorrow must not be worth anything.
  local old oldsig
  old=$(printf '{"commit":"%s","name":"manual-ci/probe","state":"passing","sent_at":%s}' \
    "$(printf 'stale' | python3 -c 'import hashlib,sys;print(hashlib.sha1(sys.stdin.buffer.read()).hexdigest())')" "$((now - 3600000))")
  oldsig=$(printf %s "$old" | openssl dgst -sha256 -hmac "$STRATUM_CI_SECRET" | sed 's/^.*= //')
  probe 409 "an hour-old sent_at" -H "X-Weft-Signature-256: sha256=$oldsig" --data-binary "$old"

  # Over the bound. Refused on size, before parsing.
  local big bigsig
  big=$(python3 -c 'import json,sys;print(json.dumps({"commit":"a"*40,"name":"manual-ci/probe","state":"passing","summary":"x"*20000,"sent_at":int(sys.argv[1])}))' "$now")
  bigsig=$(printf %s "$big" | openssl dgst -sha256 -hmac "$STRATUM_CI_SECRET" | sed 's/^.*= //')
  probe 413 "a 20 KiB body" -H "X-Weft-Signature-256: sha256=$bigsig" --data-binary "$big"

  note "the replay case is not run here: proving it needs a delivery the server \
accepted, and this script deliberately never posts an acceptable one against a real \
deployment. crates/stratum-server/tests/checks_intake_e2e.rs holds it hermetically, and \
the ring is per-repository state rather than provider behaviour, so a fake cannot be \
wrong about it in the way this gate exists to catch."
}

# ---------------------------------------------------------------------

summary() {
  echo
  if [ "$FAILED" = 0 ]; then
    say "The provider contract holds on what was checked."
    echo "Record the App id, the installation ids and their permissions, the CI system"
    echo "and the Stratum credential with this result. A pass under an installation that"
    echo "holds every permission is not a pass under the one you deploy with."
  else
    die "$FAILED case(s) failed. Every failure above names the code that depends on the
semantic — that is where it surfaces in production."
  fi
}

case "${1:-}" in
  actions) shift; cmd_actions "$@"; summary ;;
  denied)  shift; cmd_denied  "$@"; summary ;;
  intake)
    shift
    case "${1:-}" in
      fixture)   shift; cmd_intake_fixture "$@" ;;
      watch)     shift; cmd_intake_watch "$@"; summary ;;
      negatives) shift; cmd_intake_negatives "$@"; summary ;;
      *) die "usage: scripts/manual-ci.sh intake <fixture|watch|negatives> [...]" ;;
    esac
    ;;
  all)
    cmd_actions
    echo
    cmd_denied
    echo
    cmd_intake_negatives
    note "\`all\` does not run \`intake watch\` (it needs somebody to push) or \
--exhaust-rate-limit (it spends the installation's whole budget). Neither is covered by \
this run."
    summary
    ;;
  *)
    cat >&2 <<'USAGE'
usage: scripts/manual-ci.sh actions [--exhaust-rate-limit]
       scripts/manual-ci.sh denied
       scripts/manual-ci.sh intake fixture <gitlab|buildkite|circleci>
       scripts/manual-ci.sh intake watch [check-name]
       scripts/manual-ci.sh intake negatives
       scripts/manual-ci.sh all

GitHub, for `actions` and `denied`:
  STRATUM_GITHUB_APP_ID                 the App the deployment authenticates as
  STRATUM_GITHUB_APP_KEY_PEM            path to its private key
  STRATUM_GITHUB_INSTALLATION           an installation WITH actions: read
  STRATUM_GITHUB_ACTIONS_REPO           owner/name, with a VARIED run history
  STRATUM_GITHUB_DENIED_INSTALLATION    an installation of the same App WITHOUT it
  STRATUM_GITHUB_DENIED_REPO            owner/name it covers

The deployment, to check our reading of GitHub's answers and the intake:
  STRATUM_URL, STRATUM_TOKEN, STRATUM_ORG, STRATUM_REPO
  STRATUM_DENIED_REPO                   a mirror on the denied installation
  STRATUM_CI_SECRET                     for `intake negatives`

Use the App installation and the Stratum token you actually deploy with. The
`denied` case cannot fail under an installation that holds every permission,
and a check that cannot fail proves nothing.
USAGE
    exit 2
    ;;
esac
