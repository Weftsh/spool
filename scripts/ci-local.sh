#!/usr/bin/env bash
# Run what CI runs, in the order CI runs it, before pushing.
#
# The point is that there is no second list. Every check below is copied
# from a step in .github/workflows/ci.yml, and the drift test at the
# bottom of this file fails if a step exists there and not here — because
# a local gate that has quietly fallen behind CI is worse than no local
# gate: you trust it and it lies.
#
#   scripts/ci-local.sh            # everything available on this machine
#   scripts/ci-local.sh --fast     # skip chaos and deploy-validation
#   scripts/ci-local.sh --only web # one job
#   scripts/ci-local.sh --only none # the preconditions only, no job
#
# Exit status is 1 if anything failed. Anything *skipped* is named loudly
# in the summary: a skip is not a pass.
set -uo pipefail
cd "$(dirname "$0")/.."

FAST=0
ONLY=""
while [ $# -gt 0 ]; do
  case "$1" in
    --fast) FAST=1 ;;
    --only) ONLY="${2:-}"; shift ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \?//'; exit 0 ;;
    *) echo "unknown argument $1" >&2; exit 2 ;;
  esac
  shift
done

FAILED=()
SKIPPED=()
PASSED=()

# ---------------------------------------------------- preflight: disk
#
# A full cycle writes tens of GB — a release build of the workspace, its
# test binaries, and MinIO's store on top. Running out happens *mid-job*, and
# the shape it takes is the problem: a compiler or a test process dies
# with an I/O error somewhere unrelated to whatever is actually wrong,
# and the run gets re-run, passes on the retry after something else
# freed a little, and goes down as flaky. This repository has already
# paid for that lesson once (see "Disk hygiene" in CONTRIBUTING.md), and
# a postmortem is a bad way to learn your disk was full.
#
# So it is a precondition, checked once, in seconds. Refusing to start is
# always cheaper than failing halfway and lying about why.
free_gib() { df -Pk . | awk 'NR==2 {printf "%d", $4/1024/1024}'; }

preflight_disk() {
  # A full run needs room for the release build and the test stores;
  # --fast and --only need proportionally less.
  local need=${STRATUM_MIN_FREE_GIB:-20}
  [ "$FAST" = 1 ] && need=${STRATUM_MIN_FREE_GIB:-12}
  [ -n "$ONLY" ] && need=${STRATUM_MIN_FREE_GIB:-12}
  local have; have="$(free_gib)"
  DISK_AT_START="$have"

  if [ "$have" -lt "$need" ]; then
    printf '\n\033[31m== disk\033[0m\n'
    printf '\033[31m   %s GiB free, and this run needs about %s GiB.\033[0m\n' "$have" "$need"
    cat <<'MSG'
   Refusing to start rather than failing partway: running out mid-run
   surfaces as an unrelated I/O error in whichever job is unlucky, and
   reads as a flaky test rather than as a full disk.

   Reclaim, then re-run:
     scripts/clean-build-artifacts.sh --deep    # both build trees + docker images

   Override deliberately with STRATUM_MIN_FREE_GIB=<n> if you know better.
MSG
    exit 1
  fi

  if [ "$have" -lt $((need * 2)) ]; then
    printf '\n\033[33m== disk: %s GiB free — enough to start, not much spare.\033[0m\n' "$have"
    printf '\033[33m   scripts/clean-build-artifacts.sh --deep reclaims both build trees.\033[0m\n'
  fi
}
preflight_disk


say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }
note() { printf '   %s\n' "$*"; }

# Run one named check. A failure is recorded and the job stops there —
# later steps in a CI job never run after an earlier one fails either.
# Each run's step output in a file of its own. It was one fixed path,
# /tmp/ci-local-step.log, shared by every run on the machine — and
# docs_e2e runs this script from inside the correctness gate's own
# `cargo test`, so the nested runs truncated the outer test step's log
# while it was still being written. The outer output became NUL bytes
# and a nested run's lines; had the tests failed, the tail printed as
# the failure would have been somebody else's.
STEP_LOG="$(mktemp "${TMPDIR:-/tmp}/ci-local-step.XXXXXX")"
trap 'rm -f "$STEP_LOG"' EXIT

step() {
  local name="$1"; shift
  note "$name"
  if ! "$@" > "$STEP_LOG" 2>&1; then
    printf '\033[31m   FAILED: %s\033[0m\n' "$name"
    tail -40 "$STEP_LOG"
    FAILED+=("$name")
    return 1
  fi
  return 0
}

# Every job name this script knows, so that a misspelt `--only` is an
# error rather than a run of nothing. It reported "Only the <name> job
# ran" and a summary with no failures, which reads exactly like a pass —
# the same trap as a SKIP being mistaken for one, and worse, because
# nothing ran at all.
ALL_JOBS="correctness-gate chaos web deploy-validation terraform-validation s3-contract github-signin-contract oidc-contract"
# `none` runs the preconditions — the disk check above — and no job at
# all. It is spelled out rather than being any unmatched word, because
# "any unmatched word means run nothing" is indistinguishable from a
# typo, which is how this was found: `--only correctness` (the job is
# `correctness-gate`) ran nothing and printed a summary with no failures.
if [ -n "$ONLY" ] && [ "$ONLY" != none ]; then
  case " $ALL_JOBS " in
    *" $ONLY "*) ;;
    *)
      printf '\033[31munknown --only job %s\033[0m\n' "$ONLY" >&2
      printf 'one of: %s, or `none` for the preconditions alone\n' "$ALL_JOBS" >&2
      exit 2 ;;
  esac
fi

wants() { [ -z "$ONLY" ] || [ "$ONLY" = "$1" ]; }

# --------------------------------------------------------------- minio
# The testkit needs one, and CI runs the same script as its own step.
#
# This used to be a third copy of the fetch — CI had one, the testkit had
# one, and this had one, each picking a platform slug and curling
# dl.min.io. All three went stale together when MinIO withdrew its
# prebuilt binaries, which is the argument for there being one: the whole
# point of this file is that there is no second list.
#
# `STRATUM_MINIO_URL` short-circuits it. On a Mac there is no binary to
# fetch at all (see scripts/fetch-minio.sh), so the answer is a MinIO
# running from the image with the harness pointed at it — and a run that
# has one must not go looking for a binary it will never find.
ensure_minio() {
  [ -n "${STRATUM_MINIO_URL:-}" ] && return 0
  [ -x .testkit/bin/minio ] && return 0
  scripts/fetch-minio.sh || return 1
}

# --------------------------------------------------- job: correctness-gate
if wants correctness-gate; then
  say "correctness-gate"
  if ! ensure_minio; then
    SKIPPED+=("correctness-gate: could not fetch MinIO — dl.min.io is gone; start one (docker run … quay.io/minio/minio) and export STRATUM_MINIO_URL")
  elif step "cargo fmt --all --check" cargo fmt --all --check \
    && step "cargo clippy --workspace --all-targets -- -D warnings" \
         cargo clippy --workspace --all-targets -- -D warnings \
    && step "cargo test --workspace --release" cargo test --workspace --release
  then
    PASSED+=("correctness-gate")
  fi
fi

# -------------------------------------------------------------- job: chaos
# The `#[ignore]`d suite: SIGKILLs the server at named store operations and
# storms it with a seeded fault plan. Skipped by --fast because it is
# minutes of wall clock, and never run under llvm-cov — see the rule at the
# top of crates/stratum-server/tests/chaos_e2e.rs.
if wants chaos; then
  say "chaos"
  if [ "$FAST" = 1 ] && [ -z "$ONLY" ]; then
    SKIPPED+=("chaos: --fast")
  elif ! ensure_minio; then
    SKIPPED+=("chaos: could not fetch MinIO — start one and export STRATUM_MINIO_URL")
  elif step "cargo test --test chaos_e2e -- --ignored" \
         cargo test -p stratum-server --test chaos_e2e --release -- --ignored
  then
    PASSED+=("chaos")
  fi
fi

# --------------------------------------------------------------- job: web
# The design-system contract greps are the checks most likely to be
# skipped locally and to fail in CI, because nothing about editing a page
# suggests they exist.
in_dash() { ( cd web/dashboard && "$@" ); }

if wants web; then
  say "web"
  # The version CI pins, from the workflow itself rather than a second
  # copy of the number here.
  node_want="$(awk '/setup-node/{f=1} f&&/node-version:/{gsub(/[^0-9]/,"",$2); print $2; exit}' \
    .github/workflows/ci.yml)"
  node_have="$(command -v node > /dev/null 2>&1 && node --version | tr -d 'v' | cut -d. -f1)"
  if ! command -v node > /dev/null 2>&1; then
    SKIPPED+=("web: node is not installed")
  elif [ -n "$node_want" ] && [ "${node_have:-0}" -lt "$node_want" ]; then
    # A too-old toolchain is a SKIP, not a FAIL. Astro refuses to build
    # and the run ends red for a reason that has nothing to do with the
    # change under test — which is exactly how people learn to ignore a
    # local gate. Name the version instead.
    SKIPPED+=("web: node $(node --version) is older than the $node_want CI pins — \
nvm install $node_want")
  else
    # `npm ci` in CI, `npm install` here: CI starts from a clean checkout,
    # and reinstalling from scratch on every local run is minutes wasted.
    # But "node_modules exists" is not "node_modules matches the lockfile":
    # a merge that brings in a package leaves the old tree in place, and
    # the dashboard build then fails on a missing module — red for a
    # reason CI (which always runs `npm ci`) would never see. npm records
    # what it installed in node_modules/.package-lock.json; reinstall when
    # the real lockfile is newer than that record.
    npm_stale() {
      [ -d "$1/node_modules" ] || return 0
      [ "$1/package-lock.json" -nt "$1/node_modules/.package-lock.json" ]
    }
    ok=1
    ! npm_stale web/dashboard || step "dashboard — npm install" in_dash npm install || ok=0
    [ "$ok" = 1 ] && step "dashboard — unit tests" in_dash npx vitest run || ok=0
    [ "$ok" = 1 ] && step "dashboard — build" in_dash npm run build || ok=0
    if [ "$ok" = 1 ]; then
      # CI installs the browser; here it is already on the image, and
      # `playwright install` would re-download it every run.
      if [ -z "${CHROMIUM_PATH:-}" ] && [ -x /opt/pw-browsers/chromium-1194/chrome-linux/chrome ]; then
        export CHROMIUM_PATH=/opt/pw-browsers/chromium-1194/chrome-linux/chrome
      fi
      # That path is the CI image's. On a development machine there is no
      # browser unless someone installed one, and Playwright's answer to
      # that is to fail every test with the same launch error — 74 lines
      # of red that say nothing about the change under test. Name the one
      # real problem instead. (Same reasoning as the node and terraform
      # version guards above: a local gate that goes red for
      # environmental reasons is a local gate people learn to ignore.)
      pw_cache="${PLAYWRIGHT_BROWSERS_PATH:-}"
      if [ -z "$pw_cache" ]; then
        case "$(uname -s)" in
          Darwin) pw_cache="$HOME/Library/Caches/ms-playwright" ;;
          *)      pw_cache="$HOME/.cache/ms-playwright" ;;
        esac
      fi
      if [ -z "${CHROMIUM_PATH:-}" ] && ! ls -d "$pw_cache"/chromium* > /dev/null 2>&1; then
        SKIPPED+=("dashboard — Playwright e2e: no browser installed. \
cd web/dashboard && npx playwright install chromium")
        ok=0
      fi
      if [ "$ok" = 1 ]; then
      # CI=true is what makes this a reproduction rather than an
      # approximation: Playwright then runs one worker instead of many,
      # refuses to reuse an already-running preview server, and fails on
      # a stray `test.only`. A timing race that only shows up serialised
      # is exactly what got through last time.
      # A shell function cannot take a `VAR=… ` prefix the way a binary
      # can, so CI is exported for this step and unset after it.
      export CI=true
      step "dashboard — Playwright e2e" in_dash npx playwright test || ok=0
      fi
      unset CI
    fi
    [ "$ok" = 1 ] && PASSED+=("web")
  fi
fi

# ------------------------------------------------- job: deploy-validation
# The image a client runs, run: CI's own commands — both images, the
# one-box compose stack, deploy/smoke.sh with the real git client and a
# self-hosted runner. Needs a docker daemon; without one it is a SKIP.
# The stack binds :8080 and :2222, so a running manual stack, or another
# compose project on those ports, is refused rather than smoked by
# mistake.
if wants deploy-validation; then
  say "deploy-validation"
  if [ "$FAST" = 1 ] && [ -z "$ONLY" ]; then
    SKIPPED+=("deploy-validation: --fast")
  elif ! docker info > /dev/null 2>&1; then
    SKIPPED+=("deploy-validation: needs a docker daemon (docker info did not answer)")
  elif lsof -nP -iTCP:8080 -sTCP:LISTEN > /dev/null 2>&1 \
       || lsof -nP -iTCP:2222 -sTCP:LISTEN > /dev/null 2>&1; then
    SKIPPED+=("deploy-validation: :8080 or :2222 is already in use — \
scripts/manual-stack.sh down, or docker compose down")
  else
    deploy_smoke() {
      docker build -t spool:local . &&
      docker build -f Dockerfile.runner -t weft-runner:local . &&
      ./deploy/dev-host-key.sh &&
      SPOOL_IMAGE=spool:local docker compose up -d --wait &&
      BASE_URL=http://127.0.0.1:8080 SSH_ENDPOINT=ssh://git@127.0.0.1:2222 \
        BOOTSTRAP_CMD="docker compose exec -T spool stratum-server admin bootstrap" \
        SMOKE_413=1 SMOKE_SELF_HOSTED=1 SMOKE_RUNNER_NETWORK=host \
        ./deploy/smoke.sh
      local rc=$?
      [ "$rc" = 0 ] || docker compose logs --no-color --tail 100
      docker compose down -v > /dev/null 2>&1
      return "$rc"
    }
    step "build both images, compose up, deploy/smoke.sh" deploy_smoke \
      && PASSED+=("deploy-validation")
  fi
fi

# ------------------------------------------ job: terraform-validation
# Credential-free: fmt, validate, and the env-lock module's test.
if wants terraform-validation; then
  say "terraform-validation"
  tf_want="$(awk -F'"' '/terraform_version:/{print $2; exit}' .github/workflows/ci.yml)"
  tf_have="$(command -v terraform > /dev/null 2>&1 && terraform version | \
    head -1 | sed 's/[^0-9.]//g')"
  # Same reasoning as the node guard: deploy/terraform declares
  # required_version, and an older CLI fails init with a version error
  # that says nothing about the change being tested.
  tf_too_old=0
  if [ -n "$tf_have" ] && [ -n "$tf_want" ]; then
    [ "$(printf '%s\n%s\n' "$tf_want" "$tf_have" | sort -V | head -1)" != "$tf_want" ] \
      && tf_too_old=1
  fi
  if [ "$tf_too_old" = 1 ]; then
    SKIPPED+=("terraform fmt + validate: terraform $tf_have is older than the \
$tf_want CI pins, and deploy/terraform requires a newer core")
  fi
  if command -v terraform > /dev/null 2>&1 && [ "$tf_too_old" = 0 ]; then
    # Validate in a data directory of our own, never in deploy/terraform's
    # `.terraform/`. CI checks out clean, so `init -backend=false` there
    # only ever meets a directory nobody has initialised. On a machine
    # that has run a real apply, `.terraform/terraform.tfstate` records
    # the S3 backend, and terraform 1.15 still reaches for it — and the
    # deploy credentials — even under `-backend=false`: the step failed
    # with "No valid credential sources found" in a tree whose terraform
    # was fine, in the same directory a destroy was running from. The
    # data dir lives under target/ so the provider download is paid once
    # and swept with everything else.
    tf_data="$PWD/target/ci-local/terraform"
    step "terraform fmt + validate" bash -c '
      tf_data="$1"
      terraform -chdir=deploy/terraform fmt -check -recursive &&
      TF_DATA_DIR="$tf_data/root" \
        terraform -chdir=deploy/terraform init -backend=false -input=false &&
      TF_DATA_DIR="$tf_data/root" \
        terraform -chdir=deploy/terraform validate &&
      TF_DATA_DIR="$tf_data/bootstrap" \
        terraform -chdir=deploy/terraform/bootstrap init -backend=false -input=false &&
      TF_DATA_DIR="$tf_data/bootstrap" \
        terraform -chdir=deploy/terraform/bootstrap validate &&
      TF_DATA_DIR="$tf_data/env-lock" \
        terraform -chdir=deploy/terraform/modules/env-lock init -backend=false -input=false &&
      TF_DATA_DIR="$tf_data/env-lock" \
        terraform -chdir=deploy/terraform/modules/env-lock test' _ "$tf_data" \
      && PASSED+=("terraform-validation")
  elif ! command -v terraform > /dev/null 2>&1; then
    SKIPPED+=("terraform fmt + validate: terraform is not installed")
  fi
fi

# ------------------------------------------- manual gate: s3-contract
# Not a CI job, deliberately: this needs credentials for a real bucket and
# those do not belong in CI. The MinIO half of the same contract already
# ran inside correctness-gate. This block exists so the summary *names*
# the real-S3 half as unchecked rather than letting a green local run
# imply it passed — the same reason the manual browser pass is called out.
if wants s3-contract; then
  say "s3-contract (manual gate)"
  if [ -z "${STRATUM_S3_BUCKET:-}" ] || [ -z "${AWS_ACCESS_KEY_ID:-}" ]; then
    SKIPPED+=("s3-contract: no real-S3 credentials. The contract ran against \
MinIO in correctness-gate; real S3 is a MANUAL gate — scripts/manual-s3.sh check \
--both-addressing-styles, under the deploy role")
  else
    step "store contract against real S3" \
      ./scripts/manual-s3.sh check --both-addressing-styles \
      && PASSED+=("s3-contract")
  fi
fi

# --------------------------------- manual gate: github-signin-contract
# The second, about the GitHub App: signing *in* with
# GitHub skips our own confirmation mail, and the whole justification is
# one field — `verified`, on the primary entry of GET /user/emails — in
# a response the fake answers from what we believe. This block names the
# real-GitHub half as unchecked. Run it under the OAuth client you
# deploy with: half of what is checked is that the permissions that App
# holds admit the two reads, and an App with everything ticked cannot
# fail that.
if wants github-signin-contract; then
  say "github-signin-contract (manual gate)"
  if [ -z "${STRATUM_GITHUB_CLIENT_ID:-}" ] || [ -z "${STRATUM_GITHUB_CLIENT_SECRET:-}" ]; then
    SKIPPED+=("github-signin-contract: no OAuth client. The sign-in routes ran against \
FakeGithub in correctness-gate; real GitHub is a MANUAL gate — scripts/manual-github-signin.sh all, \
under the client you deploy with (it needs a person at a browser), then \`fixtures\`")
  else
    SKIPPED+=("github-signin-contract: needs a person at a browser to approve the \
authorization, so it is never run unattended — scripts/manual-github-signin.sh all")
  fi
fi

# ----------------------------------------------- manual gate: oidc-contract
# Single sign-on makes an account for anybody the company's provider
# vouches for, and every automated test of it runs against the fake
# provider in stratum-testkit, which says what we believe Okta, Entra ID,
# Google and Keycloak send. This block names the real-provider half as
# unchecked. Run it under the issuer and client you deploy with, once per
# provider you support: a run against one claims nothing about another.
if wants oidc-contract; then
  say "oidc-contract (manual gate)"
  if [ -z "${STRATUM_OIDC_ISSUER:-}" ] || [ -z "${STRATUM_OIDC_CLIENT_SECRET:-}" ]; then
    SKIPPED+=("oidc-contract: no identity provider configured. Sign-in ran against the fake \
provider in correctness-gate; a real one is a MANUAL gate — scripts/manual-oidc.sh all, \
under the issuer and client you deploy with (it needs a person to sign in), then \`fixtures\`")
  else
    SKIPPED+=("oidc-contract: needs a person to sign in at the provider, so it is never \
run unattended — scripts/manual-oidc.sh all")
  fi
fi

# ------------------------------------------------------------- the summary
say "disk"
note "$(free_gib) GiB free now; $DISK_AT_START GiB before this run"
note "scripts/clean-build-artifacts.sh --deep when you are done for the day"

say "summary"
for p in "${PASSED[@]:-}"; do [ -n "$p" ] && printf '\033[32m   PASS  %s\033[0m\n' "$p"; done
for s in "${SKIPPED[@]:-}"; do [ -n "$s" ] && printf '\033[33m   SKIP  %s\033[0m\n' "$s"; done
for f in "${FAILED[@]:-}"; do [ -n "$f" ] && printf '\033[31m   FAIL  %s\033[0m\n' "$f"; done

if [ "${#FAILED[@]}" -gt 0 ]; then
  printf '\n\033[31mCI would fail. Fix the above before pushing.\033[0m\n'
  exit 1
fi
if [ -n "$ONLY" ]; then
  printf '\n\033[33mOnly the %s job ran (--only). The others were not checked.\033[0m\n' "$ONLY"
  exit 0
fi
if [ "${#SKIPPED[@]}" -gt 0 ]; then
  printf '\n\033[33mNothing run here failed — but the skips above were NOT checked,\033[0m\n'
  printf '\033[33mand CI will run them.\033[0m\n'
  exit 0
fi
printf '\n\033[32mgood to push\033[0m\n'
