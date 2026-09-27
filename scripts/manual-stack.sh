#!/usr/bin/env bash
# The stack the manual browser pass needs, brought up from nothing.
#
# CLAUDE.md requires the manual pass to run against a *fully configured*
# deployment — Postgres, MinIO, the built site and dashboard, the SSH
# front door, captured mail, and stand-ins for GitHub and Stripe — and
# then gave no way to build one. So it got rebuilt by hand each time, and
# each rebuild rediscovered the same four defects:
#
#   * readiness loops that retried 60 times with no sleep between
#     attempts, so a service that refuses the connection instantly burned
#     every try in well under a second and reported itself dead;
#   * `/minio/health/live` rather than `/health/ready`, which answers 200
#     while the S3 API is still coming up, so the next call fails;
#   * no bucket, so every repo creation 404'd against the object store
#     and read as the product being broken;
#   * no traversal permission for the `postgres` uid, which surfaces as
#     `initdb: Permission denied` naming the data directory rather than
#     the ancestor actually refusing.
#
# All four are fixed here, once.
#
#   scripts/manual-stack.sh up     # build it, seed it, print how to use it
#   scripts/manual-stack.sh down   # stop everything
#   scripts/manual-stack.sh env    # print the environment, for `eval`
#   scripts/manual-stack.sh overage <org> <gb>|clear
#                                  # put an org past its transfer pool, or undo it
#
# Everything lives under .stack/ and is disposable: `up` starts from an
# empty database every time, because a manual pass against yesterday's
# leftovers is a pass against something nobody will ever deploy.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUN=${STRATUM_STACK_DIR:-$ROOT/.stack}
FAKES=$ROOT/scripts/manual-stack

PGPORT=${PGPORT:-55432}
MINIOPORT=${MINIOPORT:-59000}
HTTPPORT=${HTTPPORT:-8080}
SSHPORT=${SSHPORT:-2222}
STRIPEPORT=${STRIPEPORT:-59100}
# One secret, named once: the server verifies webhooks with it (via the
# env file below) and the Stripe fake signs them with it. The fake was
# first handed `$STRATUM_STRIPE_WEBHOOK_SECRET`, which only exists inside
# the env file's heredoc — `set -u` stopped the stack at "fakes…".
STRIPE_WEBHOOK_SECRET=${STRIPE_WEBHOOK_SECRET:-whsec_manual}
PG_CONTAINER=${PG_CONTAINER:-stratum-stack-pg}
MINIO_CONTAINER=${MINIO_CONTAINER:-stratum-stack-minio}
GITHUBPORT=${GITHUBPORT:-59110}
# The miniature CI provider. Not a mock of one: it verifies our webhook
# signature, clones with a real credential, runs the repository's own
# ci.sh, and signs a verdict back into the intake. See
# scripts/manual-stack/ci-runner.py.
CIPORT=${CIPORT:-59120}
CI_REPO=${CI_REPO:-pipeline}
# The ECS stand-in for hosted runners — deploy/fake-ecs/fake-ecs.py, the
# same one deploy/compose.yml uses. It verifies the SigV4 the app signs
# and starts the REAL runner image with the local docker daemon, so the
# workflow stages drive the product's own dispatch path and nothing is
# faked but the cloud. Without it the walkthrough would be looking at a
# forge that cannot run CI of its own.
ECSPORT=${ECSPORT:-59130}
RUNNER_IMAGE=${RUNNER_IMAGE:-weft-runner:local}
# The GitHub Actions runner image (Dockerfile.github-runner), started by
# the same stand-in under a second task definition. The real agent cannot
# finish a job without GitHub on the other end, so what the stack proves
# for it is the launch: the dispatcher's RunTask has the shape the image
# reads, and the container collects its registration from this server.
GITHUB_RUNNER_IMAGE=${GITHUB_RUNNER_IMAGE:-weft-github-runner:local}
WF_REPO=${WF_REPO:-builds}
# Matches deploy/compose.yml. Not a secret: the fake checks the signature
# with it, so a signer that drifts from AWS fails here rather than in
# production, and it authorises nothing else anywhere.
DISPATCH_KEY_ID=AKIALOCALDISPATCH
DISPATCH_SECRET=local-dispatch-secret-not-a-real-key
BUCKET=stratum
PASSWORD="a long enough password"

say() { printf '\033[36m%s\033[0m\n' "$*"; }
die() { printf '\033[31m%s\033[0m\n' "$*" >&2; exit 1; }

# Wait for a thing to answer, and actually wait. The bug this replaces
# was a loop with no sleep in it.
wait_for() { # wait_for <name> <logfile> <command...>
  local name=$1 log=$2; shift 2
  local deadline=$(( SECONDS + 60 ))
  until "$@" >/dev/null 2>&1; do
    if (( SECONDS >= deadline )); then
      printf '\033[31m%s never came up (60s)\033[0m\n' "$name" >&2
      [ -f "$log" ] && tail -20 "$log" >&2
      exit 1
    fi
    sleep 0.5
  done
}

stop_all() {
  for p in server ci-runner fake-ecs stripe github minio pg; do
    if [ -f "$RUN/$p.pid" ]; then
      kill "$(cat "$RUN/$p.pid")" 2>/dev/null || true
      rm -f "$RUN/$p.pid"
    fi
  done
  # The postgres postmaster is started through `su`, so the pid file
  # names the wrapper rather than the server itself.
  pkill -f "postgres -D $RUN/pgdata" 2>/dev/null || true
  pkill -f "minio server $RUN/miniodata" 2>/dev/null || true
  if command -v docker > /dev/null 2>&1; then
    docker rm -f "$PG_CONTAINER" "$MINIO_CONTAINER" > /dev/null 2>&1 || true
    # Runner tasks the stand-in started. It deliberately does NOT use
    # `docker run --rm` — a task that died is the one whose stderr you
    # want — so a stopped runner outlives the process that started it and
    # has to be cleared by label, exactly as scripts/ci-local.sh does.
    docker ps -aq --filter label=stratum.fake-ecs \
      | xargs -r docker rm -f > /dev/null 2>&1 || true
  fi
  # Wait for the listeners to actually go, rather than sleeping and
  # hoping. `kill` returns as soon as the signal is delivered, and a
  # docker port publisher outlives the container by a moment, so `down`
  # used to hand back a stack whose ports were still bound. Whatever ran
  # next — `up` again, or deploy-validation, which wants the same 8080
  # and 2222 — then failed on a port it had every reason to think was
  # free, and the error said nothing about why.
  # Every port this stack binds, not just the four the deployment smoke
  # test also wants. `up` binds the fakes' ports too, and a `down`
  # followed immediately by an `up` raced them exactly the same way.
  wait_ports_free "$HTTPPORT" "$SSHPORT" "$PGPORT" "$MINIOPORT" \
    "$STRIPEPORT" "$GITHUBPORT" "$CIPORT" "$ECSPORT"
}

# True while anything is listening on $1.
port_busy() { lsof -nP -iTCP:"$1" -sTCP:LISTEN > /dev/null 2>&1; }

# Refuse to start over a port something else holds, naming the holder so
# the fix is one `kill` and not a search. A stack that starts anyway
# runs against another run's fakes and passes or fails for their reasons.
ports_free_or_die() {
  local port held=""
  for port in "$@"; do
    if port_busy "$port"; then
      held="$held
  port $port: $(lsof -nP -iTCP:"$port" -sTCP:LISTEN | awk 'NR>1 {print $1, "pid", $2}' | sort -u | tr '\n' ';')"
    fi
  done
  [ -z "$held" ] || die "another stack, or its leftovers, still holds:$held
stop it (kill the pids above, or run its own 'down'), then 'up' again"
}

# Block until every named port is free, or give up after ~15s and say
# which one is still held — a stuck port is worth naming, not waiting on
# forever.
wait_ports_free() {
  local port deadline=$(( $(date +%s) + 15 ))
  for port in "$@"; do
    while port_busy "$port"; do
      if [ "$(date +%s)" -ge "$deadline" ]; then
        printf '\033[33m  port %s is still in use; something else is holding it\033[0m\n' \
          "$port" >&2
        break
      fi
      sleep 0.2
    done
  done
}

write_env() {
  # No backticks anywhere in this heredoc: it is unquoted, so a
  # backtick is command substitution and a comment mentioning one
  # gets *run* — which is where the "weft.localhost: command not
  # found" noise on every `up` came from.
  cat > "$RUN/env.sh" <<ENV
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1
export STRATUM_DB_URL="postgres://stratum@127.0.0.1:$PGPORT/stratum"
export STRATUM_STORE_URL="http://127.0.0.1:$MINIOPORT/$BUCKET"
export STRATUM_DATA_DIR="$RUN/data"
export STRATUM_SITE_DIR="$ROOT/web/site/dist"
export STRATUM_DASHBOARD_DIR="$ROOT/web/dashboard/dist"
# Loopback normally. When hosted runners are up the server has to be
# reachable from inside a runner *container*, and a container's own
# 127.0.0.1 is the container — so the bind widens to every interface and
# the runner is handed a host-gateway address below. The published URL
# stays loopback either way: that is what a person types.
export STRATUM_BIND="$BINDADDR:$HTTPPORT"
export STRATUM_PUBLIC_URL="http://127.0.0.1:$HTTPPORT"
export STRATUM_WEBHOOK_SECRET="manual-stack-secret"
# The SSH front door. Without it the dashboard correctly hides the SSH
# clone row and the pass becomes a walkthrough of a different product.
export STRATUM_SSH_BIND="127.0.0.1:$SSHPORT"
export STRATUM_SSH_HOST_KEY="\$(cat "$RUN/host-key")"
export STRATUM_SSH_PUBLIC_URL="ssh://git@127.0.0.1:$SSHPORT"
# Customer sites, on their own domain. Without it the settings panel
# correctly reports no address and the pass becomes a walkthrough of a
# product that does not host sites.
#
# weft.localhost rather than plain localhost for two reasons that
# both matter. The server refuses a sites domain with no dot in it,
# because a suffix match on a bare name would claim the product's own
# hostnames as customer sites. And RFC 6761 makes every name under
# .localhost loopback, which macOS and Chrome both honour at any
# depth, so docs--acme.weft.localhost reaches this stack with no
# hosts file and no DNS. (No backticks in this comment: it sits inside
# an unquoted heredoc, where a backtick is a command substitution and
# printed "weft.localhost: command not found" four times per start.)
# That is what lets the browser pass drive a real site rather than a
# mocked one.
export STRATUM_SITES_DOMAIN="weft.localhost"
# Mail to a directory, so the pass opens an invitation the way the person
# it was sent to does.
export STRATUM_MAIL_TRANSPORT="capture"
export STRATUM_MAIL_FROM="no-reply@stratum.test"
export STRATUM_MAIL_DIR="$RUN/mail"
export STRATUM_STRIPE_KEY="sk_test_manual"
export STRATUM_STRIPE_BASE="http://127.0.0.1:$STRIPEPORT"
export STRATUM_STRIPE_PRICE="price_seat"
export STRATUM_STRIPE_WEBHOOK_SECRET="$STRIPE_WEBHOOK_SECRET"
# The metered prices and the meters they report to, so the stack meters
# use past the pool the way production does. The ids are the fake's;
# scripts/manual-stripe.sh meters mints real ones in a sandbox.
export STRATUM_STRIPE_PRICE_MINUTES="price_minutes"
export STRATUM_STRIPE_PRICE_EGRESS="price_egress"
export STRATUM_STRIPE_PRICE_STORAGE="price_storage"
export STRATUM_STRIPE_METER_MINUTES="weft_hosted_minutes"
export STRATUM_STRIPE_METER_EGRESS="weft_private_egress_mb"
export STRATUM_STRIPE_METER_STORAGE="weft_private_storage_mb_days"
export STRATUM_STRIPE_PRICE_PACKAGES="price_packages"
export STRATUM_STRIPE_METER_PACKAGES="weft_private_packages_mb_days"
# Fold and report every twenty seconds rather than every fifteen
# minutes, so a person watching the fake's page sees the meter events a
# clone produced before they have finished reading the billing screen.
export STRATUM_BILLING_ROLLUP_SECS="20"
export STRATUM_STORAGE_SWEEP_SECS="20"
# A GitHub App pointed at the local fake, and a git base that is a
# directory of bare repositories — so mirroring fetches from disk and no
# packet leaves this machine.
export STRATUM_GITHUB_APP_ID="12345"
export STRATUM_GITHUB_APP_KEY_PEM="$RUN/gh-app-key.pem"
export STRATUM_GITHUB_API_BASE="http://127.0.0.1:$GITHUBPORT"
export STRATUM_GITHUB_GIT_BASE="file://$RUN/origins"
export STRATUM_GITHUB_INSTALL_URL="http://127.0.0.1:$GITHUBPORT/apps/stratum/installations/new"
# The App's OAuth client, so the install callback proves the installer
# controls the installation — the fake exchanges any code_owning_ code
# followed by the installation id. (No angle brackets in this heredoc:
# bash 3.2 reads them as redirections inside a command substitution.)
export STRATUM_GITHUB_CLIENT_ID="Iv1.fake"
export STRATUM_GITHUB_CLIENT_SECRET="fake-client-secret"
export STRATUM_GITHUB_OAUTH_BASE="http://127.0.0.1:$GITHUBPORT"
# The local CI provider and the repository it watches. The walkthrough
# reads these to drive the loop; without them its CI stages report a
# missing prerequisite rather than passing quietly, the same way a
# missing SSH URL does.
export CI_RUNNER_URL="http://127.0.0.1:$CIPORT"
export CI_RUNNER_REPO="$CI_REPO"
# What the manual gates read. scripts/manual-registry.sh documents
# eval of this output as the way to get BASE and WEFT_TOKEN, and until
# now env set neither — so following those instructions to the letter
# got "BASE is not set", and an operator had to guess the port and go
# looking for the token file. Same class as the missing eval in the
# walkthrough instructions: a prerequisite you have to reconstruct is
# one that gets reconstructed wrongly.
#
# No backticks in this block. The heredoc is unquoted, so a backtick is
# command substitution and the comment would run.
#
# The token read is escaped on purpose, so it happens when the caller
# evals this file rather than when the file is written: write_env runs
# three seconds before bootstrap mints the token, so baking the value in
# captured the *previous* stack's token and every call 401d.
export BASE="http://127.0.0.1:$HTTPPORT"
export WEFT_TOKEN="\$(cat "$RUN/token" 2>/dev/null)"
ENV
  # Hosted runners, only when the stand-in is actually up. These are the
  # same variables the terraform root passes the real service, with the
  # ECS endpoint pointed at deploy/fake-ecs — see deploy/compose.yml,
  # which configures the app identically.
  #
  # Written conditionally rather than always: an app configured with an
  # ECS endpoint that nothing is listening on would queue every job and
  # report nothing, which reads as the product being broken.
  if [ "$RUNNER_OK" = 1 ]; then
    cat >> "$RUN/env.sh" <<ENV
export STRATUM_RUNNER_ECS_CLUSTER="local"
export STRATUM_RUNNER_ECS_TASK_DEFINITION="weft-runner-local"
$( [ "$GITHUB_RUNNER_OK" = 1 ] && echo '# The GitHub Actions runner family, on the same stand-in. Written only
# when its image exists: absent, the server treats GitHub runners as a
# feature this deployment does not have and refuses the launch, which
# is the honest state rather than a task that fails to start.
export STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION="weft-gh-runner-local"' )
export STRATUM_RUNNER_ECS_SUBNETS="subnet-local"
export STRATUM_RUNNER_ECS_SECURITY_GROUP="sg-local"
export STRATUM_RUNNER_ECS_URL="http://127.0.0.1:$ECSPORT"
export STRATUM_RUNNER_AWS_ACCESS_KEY_ID="$DISPATCH_KEY_ID"
export STRATUM_RUNNER_AWS_SECRET_ACCESS_KEY="$DISPATCH_SECRET"
export STRATUM_RUNNER_AWS_REGION="us-east-1"
# Where a runner reaches this server, and where it is told to clone from
# (the server derives clone_url from this, not from STRATUM_PUBLIC_URL).
# host.docker.internal is the docker host as seen from a container.
export STRATUM_RUNNER_URL="http://$RUNNER_HOST:$HTTPPORT"
# 5s is the production default and makes every workflow stage of the
# manual pass wait on a poll it does not care about.
export STRATUM_RUNNER_POLL_SECS="1"
# The same budget and ceiling the reference deployment ships
# (deploy/compose.yml, deploy/terraform/variables.tf). Unset, the
# binary meters nothing, the billing page has no minutes panel, and the
# walkthrough's minutes stage asserts nothing — a pass of a product
# that is not the one we deploy.
export STRATUM_RUNNER_MINUTES_PER_MONTH="2000"
export STRATUM_RUNNER_MAX_TIMEOUT_MINUTES="360"
# The walkthrough is what the stack exists for, and its workflow stages
# read these the way the CI stages read CI_RUNNER_URL: present means
# "prove the loop", absent means "say the stack is half-configured".
export RUNNER_ECS_URL="http://127.0.0.1:$ECSPORT"
export RUNNER_WF_REPO="$WF_REPO"
ENV
  fi
  # The self-hosted stages start the REAL runner binary on this machine,
  # from the command Settings → Runners shows. It is the one piece of
  # this stack a customer runs themselves, so it runs here the way it
  # runs there: natively, not in a container, registered by a person.
  if [ -n "$RUNNER_BIN" ]; then
    cat >> "$RUN/env.sh" <<ENV
export RUNNER_BIN="$RUNNER_BIN"
ENV
  fi
}

cmd_env() { [ -f "$RUN/env.sh" ] || die "no stack — run: scripts/manual-stack.sh up"; cat "$RUN/env.sh"; }

# Put an organization past its transfer pool, honestly: a row in the
# same `metrics_minute` table a real clone writes, against one of the
# org's private repositories, for as many gigabytes as asked. The
# billing view, the clone door and the rollup all read that table, so
# what the walkthrough then sees is the product at the cap, not a
# mocked answer. `count = 0` marks the row as seeded — a served request
# always counts at least one — and `clear` deletes exactly those rows.
# Three seats at the stack's 10 GB is a 30 GB pool; 40 crosses it.
cmd_overage() {
  [ -f "$RUN/env.sh" ] || die "no stack — run: scripts/manual-stack.sh up"
  local org=${1:-} what=${2:-}
  [ -n "$org" ] && [ -n "$what" ] || die "usage: manual-stack.sh overage <org> <gb>|clear"
  local sql
  if [ "$what" = clear ]; then
    sql="DELETE FROM metrics_minute WHERE count = 0 AND kind = 'clone' AND repo_id IN \
           (SELECT r.id FROM repos r JOIN orgs o ON o.id = r.org_id WHERE o.name = '$org');"
  else
    case "$what" in *[!0-9]*|'') die "gb must be a whole number, or 'clear'";; esac
    sql="INSERT INTO metrics_minute (repo_id, minute, kind, count, bytes, ms_sum, histogram) \
           SELECT r.id, (extract(epoch from now()) * 1000)::bigint / 60000, 'clone', 0, \
                  ${what}::bigint * 1073741824, 0, '{}' \
           FROM repos r JOIN orgs o ON o.id = r.org_id \
           WHERE o.name = '$org' AND r.public = false ORDER BY r.name LIMIT 1;"
  fi
  local out
  if command -v docker > /dev/null 2>&1 && [ -n "$(docker ps -q -f "name=^${PG_CONTAINER}\$")" ]; then
    out=$(docker exec "$PG_CONTAINER" psql -U stratum -d stratum -v ON_ERROR_STOP=1 -t -c "$sql")
  else
    local psql
    psql=$(command -v psql || true)
    [ -n "$psql" ] || for cand in /usr/lib/postgresql/*/bin /opt/homebrew/opt/postgresql@*/bin /usr/local/opt/postgresql@*/bin; do
      [ -x "$cand/psql" ] && psql="$cand/psql"
    done
    [ -n "$psql" ] || die "no psql on PATH and no $PG_CONTAINER container"
    out=$("$psql" -h 127.0.0.1 -p "$PGPORT" -U stratum -d stratum -v ON_ERROR_STOP=1 -t -c "$sql")
  fi
  # psql's tag says how many rows moved; an org with no private
  # repository seeds nothing, and that has to be said rather than left
  # for the walkthrough to discover as "the clone was not refused".
  case "$out" in
    *"INSERT 0 0"*) die "$org has no private repository to record transfer against" ;;
  esac
  if [ "$what" = clear ]; then
    say "overage cleared for $org ($(echo "$out" | tr -d '\n'))"
  else
    say "$org: $what GB of transfer recorded against its first private repository"
  fi
}

cmd_down() { stop_all; say "stack down"; }

cmd_up() {
  command -v git >/dev/null || die "git is required"
  # Docker first, and by default. postgres and minio on the host needed a
  # Debian layout, a `postgres` system user, and root to `su` to it —
  # three assumptions that hold on the CI image and on nothing else, so
  # the stack simply would not come up on a development machine. The
  # containers need none of them, which is also how the product is
  # actually deployed. Set STRATUM_STACK_NO_DOCKER=1 to force the old path.
  USE_DOCKER=0
  if [ -z "${STRATUM_STACK_NO_DOCKER:-}" ] && command -v docker > /dev/null 2>&1 \
     && docker info > /dev/null 2>&1; then
    USE_DOCKER=1
  fi

  # Find PostgreSQL the way crates/stratum-testkit/src/pg.rs does, and for
  # the same reason: this only ever looked in /usr/lib/postgresql/*/bin,
  # which is Debian's layout and the CI runner's. On any machine that
  # installs postgres somewhere else — homebrew being the obvious one —
  # the stack refused to come up while `initdb` sat on PATH the whole
  # time, which reads as "postgres is missing" rather than "this script
  # only knows one distribution".
  local pgbin=""
  if [ "$USE_DOCKER" = 1 ]; then
    pgbin=""
  elif [ -n "${STRATUM_PG_BIN_DIR:-}" ]; then
    pgbin="$STRATUM_PG_BIN_DIR"
  elif command -v initdb > /dev/null 2>&1; then
    pgbin=$(dirname "$(command -v initdb)")
  else
    # Debian/Ubuntu and the GitHub runner image, then homebrew's
    # versioned kegs (postgres is keg-only there, so not on PATH).
    for cand in $(ls -d /usr/lib/postgresql/*/bin 2>/dev/null | sort -V) \
                $(ls -d /opt/homebrew/opt/postgresql@*/bin 2>/dev/null | sort -V) \
                $(ls -d /usr/local/opt/postgresql@*/bin 2>/dev/null | sort -V); do
      [ -x "$cand/initdb" ] && pgbin="$cand"
    done
  fi
  if [ "$USE_DOCKER" = 0 ]; then
    [ -n "$pgbin" ] && [ -x "$pgbin/initdb" ] || die \
      "no PostgreSQL binaries found, and no docker daemon to run one in.
Start Docker, or set STRATUM_PG_BIN_DIR / put initdb on PATH."
  fi
  # Only the host path needs a binary. Under docker the stack runs
  # `quay.io/minio/minio` and never looks at `.testkit/bin` — so this
  # check, which was not guarded the way the PostgreSQL one above it is,
  # refused to start a stack it was fully able to run. On macOS it could
  # never be satisfied at all: MinIO publishes no darwin binary any more
  # (see scripts/fetch-minio.sh), so the advice to "run the test suite
  # once to fetch it" named a fetch that cannot succeed.
  if [ "$USE_DOCKER" = 0 ]; then
    [ -x "$ROOT/.testkit/bin/minio" ] || die \
      "no minio at .testkit/bin/minio, and no docker daemon to run one in.
Start Docker, or run scripts/fetch-minio.sh."
  fi

  local bin
  if [ -x "$ROOT/target/release/stratum-server" ]; then
    bin=$ROOT/target/release/stratum-server
  elif [ -x "$ROOT/target/debug/stratum-server" ]; then
    bin=$ROOT/target/debug/stratum-server
  else
    die "no server binary — run: cargo build --release -p stratum-server"
  fi
  for d in web/site/dist web/dashboard/dist; do
    [ -d "$ROOT/$d" ] || die "no $d — run: (cd ${d%/dist} && npm ci && npm run build)"
  done
  # The runner binary for self-hosted runners, same pick as the server:
  # release when it is there, debug otherwise, and said out loud when
  # it is stale next to the server — the walkthrough's self-hosted
  # stages then test a runner the server was not built with.
  RUNNER_BIN=
  for cand in "$ROOT/target/release/weft-runner" "$ROOT/target/debug/weft-runner"; do
    if [ -x "$cand" ]; then RUNNER_BIN=$cand; break; fi
  done
  if [ -z "$RUNNER_BIN" ]; then
    say "self-hosted runners: no weft-runner binary — build it with"
    say "  cargo build --release -p stratum-runner"
    say "  (the walkthrough's self-hosted stages will report this as a problem)"
  # Stale means older than its own sources. Comparing it with the server
  # binary was wrong: cargo leaves a binary alone when nothing it depends
  # on changed, so a fresh server next to an untouched runner read as
  # "rebuild it" when there was nothing to rebuild.
  elif [ -n "$(find "$ROOT/crates/stratum-runner/src" -newer "$RUNNER_BIN" -print -quit)" ]; then
    say "self-hosted runners: $RUNNER_BIN is older than crates/stratum-runner/src — rebuild it"
  fi

  stop_all
  # A port another run still holds is fatal, not a warning. `up` used to
  # print "still in use", start its own fakes — which failed to bind,
  # silently — and hand the pass a stack whose Stripe, GitHub, CI and
  # ECS were somebody else's, hours old and watching a server that was
  # gone. The ci stages then failed on a provider that "never reported",
  # which read as a product bug and was three leftover Python processes.
  ports_free_or_die "$HTTPPORT" "$SSHPORT" "$PGPORT" "$MINIOPORT" \
    "$STRIPEPORT" "$GITHUBPORT" "$CIPORT" "$ECSPORT"
  rm -rf "$RUN/pgdata" "$RUN/miniodata" "$RUN/data" "$RUN/mail" "$RUN/ci-work"
  # `$RUN/miniodata/$BUCKET` is the bucket: the filesystem backend takes
  # each top-level directory as one, and it is made before minio starts
  # so it is there the first time the server writes.
  mkdir -p "$RUN/pgdata" "$RUN/miniodata/$BUCKET" "$RUN/data" "$RUN/mail" "$RUN/logs" \
           "$RUN/origins" "$RUN/ci-work"

  # postgres runs under its own uid, so every directory on the way down
  # to PGDATA has to be traversable by it — not just PGDATA itself.
  local anc=$RUN
  while [ "$anc" != "/" ]; do
    chmod o+x "$anc" 2>/dev/null || true
    anc=$(dirname "$anc")
  done

  # Credentials, generated rather than committed. A private key in a
  # repository is a private key somebody will eventually reuse.
  [ -f "$RUN/host-key" ] || ssh-keygen -t ed25519 -N "" -C manual-stack -f "$RUN/host-key" >/dev/null
  [ -f "$RUN/gh-app-key.pem" ] || openssl genrsa -out "$RUN/gh-app-key.pem" 2048 2>/dev/null

  # Origins for the mirror flow to fetch from, over file://.
  for name in acme-inc/widget acme-inc/atlas; do
    local bare=$RUN/origins/$name.git
    [ -d "$bare" ] && continue
    mkdir -p "$(dirname "$bare")"
    local work=$RUN/origins/.build
    rm -rf "$work"; mkdir -p "$work"
    git -C "$work" init -q -b main
    printf '# %s\n\nAn origin the mirror flow can really fetch.\n' "${name##*/}" > "$work/README.md"
    git -C "$work" add -A
    git -C "$work" -c user.email=stack@stratum.test -c user.name="Manual Stack" \
      commit -qm "first commit"
    git clone -q --bare "$work" "$bare"
    rm -rf "$work"
  done

  if [ "$USE_DOCKER" = 1 ]; then
    say "postgres… (container)"
    # `trust` keeps the connection string in env.sh password-free, which
    # is what the host path gave with `initdb -A trust`.
    docker run -d --name "$PG_CONTAINER" \
      -p "127.0.0.1:$PGPORT:5432" \
      -e POSTGRES_USER=stratum -e POSTGRES_DB=stratum \
      -e POSTGRES_HOST_AUTH_METHOD=trust \
      postgres:16 > /dev/null
    docker logs -f "$PG_CONTAINER" > "$RUN/logs/pg.log" 2>&1 &
    wait_for postgres "$RUN/logs/pg.log" \
      docker exec "$PG_CONTAINER" pg_isready -U stratum -q

    say "minio… (container)"
    # $RUN/miniodata is bind-mounted, so the bucket directory made above
    # is present before minio starts — the filesystem backend takes each
    # top-level directory as a bucket, and creating it afterwards is what
    # used to make the first repo write 404.
    docker run -d --name "$MINIO_CONTAINER" \
      -p "127.0.0.1:$MINIOPORT:9000" \
      -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
      -e MINIO_BROWSER=off \
      -v "$RUN/miniodata:/data" \
      quay.io/minio/minio server /data --address ":9000" > /dev/null
    docker logs -f "$MINIO_CONTAINER" > "$RUN/logs/minio.log" 2>&1 &
    # /health/ready, not /health/live: liveness turns 200 before the S3
    # API is serving, which is how the bucket write right after it used
    # to fail.
    wait_for minio "$RUN/logs/minio.log" \
      curl -fsS "http://127.0.0.1:$MINIOPORT/minio/health/ready"
  else
    say "postgres…"
    chown -R postgres:postgres "$RUN/pgdata"
    su postgres -c "$pgbin/initdb -D $RUN/pgdata -U stratum -A trust --no-sync" \
      > "$RUN/logs/initdb.log" 2>&1 || { tail -5 "$RUN/logs/initdb.log" >&2; exit 1; }
    su postgres -c "$pgbin/postgres -D $RUN/pgdata -p $PGPORT -k $RUN/pgdata -c listen_addresses=127.0.0.1" \
      > "$RUN/logs/pg.log" 2>&1 &
    echo $! > "$RUN/pg.pid"
    wait_for postgres "$RUN/logs/pg.log" "$pgbin/pg_isready" -h 127.0.0.1 -p "$PGPORT" -q
    "$pgbin/createdb" -h 127.0.0.1 -p "$PGPORT" -U stratum stratum

    say "minio…"
    MINIO_ROOT_USER=minioadmin MINIO_ROOT_PASSWORD=minioadmin \
      "$ROOT/.testkit/bin/minio" server "$RUN/miniodata" --address "127.0.0.1:$MINIOPORT" \
      > "$RUN/logs/minio.log" 2>&1 &
    echo $! > "$RUN/minio.pid"
    wait_for minio "$RUN/logs/minio.log" \
      curl -fsS "http://127.0.0.1:$MINIOPORT/minio/health/ready"
  fi

  say "fakes…"
  # FAKE_STRIPE_PORT, not the default. $STRIPEPORT was overridable here
  # and never reached the fake, which reads FAKE_STRIPE_PORT and falls
  # back to 59100 — so a second stack on non-default ports came up as far
  # as the fakes and then died on `Address already in use` for a port
  # nobody had asked it to use. The github fake was always passed its
  # port; this one was not.
  # The fake serves the card page and the portal itself and delivers
  # the events those pages cause to our webhook, signed with the secret
  # the server verifies with — so a person can finish the trip.
  FAKE_STRIPE_PORT=$STRIPEPORT \
    FAKE_STRIPE_WEBHOOK_URL="http://127.0.0.1:$HTTPPORT/webhooks/stripe" \
    FAKE_STRIPE_WEBHOOK_SECRET="$STRIPE_WEBHOOK_SECRET" \
    python3 "$FAKES/fake-stripe.py" > "$RUN/logs/stripe.log" 2>&1 &
  echo $! > "$RUN/stripe.pid"
  STRATUM_PUBLIC_URL="http://127.0.0.1:$HTTPPORT" FAKE_GITHUB_PORT=$GITHUBPORT \
    python3 "$FAKES/fake-github.py" > "$RUN/logs/github.log" 2>&1 &
  echo $! > "$RUN/github.pid"
  wait_for stripe "$RUN/logs/stripe.log" \
    curl -fsS -o /dev/null "http://127.0.0.1:$STRIPEPORT/v1/ping"
  wait_for github "$RUN/logs/github.log" \
    curl -fsS -o /dev/null "http://127.0.0.1:$GITHUBPORT/app/installations"

  # ------------------------------------------------------------------
  # Hosted runners: the ECS stand-in.
  #
  # Run on the host rather than in a container, like the other fakes —
  # it only needs python3 and the docker CLI, both of which are already
  # prerequisites here. It starts runner containers on the default
  # bridge, so FAKE_ECS_NETWORK is set explicitly: the auto-detection in
  # fake-ecs.py inspects its own container, which does not exist when it
  # is a host process.
  #
  # Two things have to be true or the workflow stages prove nothing, and
  # both are checked here rather than discovered as a mysterious timeout
  # in the browser: a docker daemon, and the runner image built from
  # Dockerfile.runner.
  # ------------------------------------------------------------------
  RUNNER_OK=0
  GITHUB_RUNNER_OK=0
  RUNNER_HOST=host.docker.internal
  BINDADDR=127.0.0.1
  if [ "$USE_DOCKER" = 0 ]; then
    say "hosted runners: no docker daemon — the walkthrough's workflow stages will say so"
  elif ! docker image inspect "$RUNNER_IMAGE" > /dev/null 2>&1; then
    say "hosted runners: no $RUNNER_IMAGE image — build it with"
    say "  docker build -f Dockerfile.runner -t $RUNNER_IMAGE ."
    say "the walkthrough's workflow stages will report it as a problem"
  else
    RUNNER_OK=1
    # A container cannot reach a server bound to the host's loopback.
    BINDADDR=0.0.0.0
    # The GitHub Actions runner image is optional on top: without it the
    # stand-in is told no second definition and the server is told no
    # GitHub family, so the walkthrough's GitHub-runner stages report
    # the missing image rather than a launch that fails.
    if docker image inspect "$GITHUB_RUNNER_IMAGE" > /dev/null 2>&1; then
      GITHUB_RUNNER_OK=1
    else
      say "GitHub Actions runners: no $GITHUB_RUNNER_IMAGE image — build it with"
      say "  docker build --platform linux/amd64 -f Dockerfile.github-runner -t $GITHUB_RUNNER_IMAGE ."
      say "the server will refuse GitHub runner launches until it exists"
    fi
    FAKE_ECS_ACCESS_KEY_ID=$DISPATCH_KEY_ID \
    FAKE_ECS_SECRET_ACCESS_KEY=$DISPATCH_SECRET \
    FAKE_ECS_REGION=us-east-1 \
    FAKE_ECS_CLUSTER=local \
    FAKE_ECS_TASK_DEFINITION=weft-runner-local \
    FAKE_ECS_RUNNER_IMAGE="$RUNNER_IMAGE" \
    FAKE_ECS_GITHUB_TASK_DEFINITION=$( [ "$GITHUB_RUNNER_OK" = 1 ] && echo weft-gh-runner-local ) \
    FAKE_ECS_GITHUB_RUNNER_IMAGE=$( [ "$GITHUB_RUNNER_OK" = 1 ] && echo "$GITHUB_RUNNER_IMAGE" ) \
    FAKE_ECS_PORT=$ECSPORT \
    FAKE_ECS_NETWORK=bridge \
      python3 "$ROOT/deploy/fake-ecs/fake-ecs.py" > "$RUN/logs/fake-ecs.log" 2>&1 &
    echo $! > "$RUN/fake-ecs.pid"
    wait_for "ecs stand-in" "$RUN/logs/fake-ecs.log" \
      curl -fsS -o /dev/null "http://127.0.0.1:$ECSPORT/"
  fi

  write_env
  say "server…"
  set -a; . "$RUN/env.sh"; set +a
  "$bin" > "$RUN/logs/server.log" 2>&1 &
  echo $! > "$RUN/server.pid"
  wait_for server "$RUN/logs/server.log" \
    curl -fsS -o /dev/null "http://127.0.0.1:$HTTPPORT/healthz"

  say "seeding…"
  "$bin" admin bootstrap --org acme > "$RUN/bootstrap.json"
  local token
  token=$(python3 -c "import json;print(json.load(open('$RUN/bootstrap.json'))['admin_token'])")
  echo "$token" > "$RUN/token"
  # The three accounts `walkthrough.mjs` expects, one per role.
  "$bin" admin user-create --email ada@acme.dev  --name "Ada Owner"  --password "$PASSWORD" --org acme --role owner  >/dev/null
  "$bin" admin user-create --email dev@acme.dev  --name "Dev Person" --password "$PASSWORD" --org acme --role member >/dev/null
  "$bin" admin user-create --email view@acme.dev --name "Vi Viewer"  --password "$PASSWORD" --org acme --role viewer >/dev/null

  api() { curl -sf -X "$1" "http://127.0.0.1:$HTTPPORT$2" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' -d "$3" -o /dev/null; }
  # The same call, but hand the body back. Secrets and tokens are shown
  # exactly once by the routes that mint them, so a discarded response
  # is a credential that cannot be recovered.
  api_json() { curl -sf -X "$1" "http://127.0.0.1:$HTTPPORT$2" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' -d "$3"; }
  # One field out of a JSON object, or empty. `python3 -c` rather than a
  # jq dependency: nothing else in this script needs one.
  jfield() { python3 -c 'import json,sys;print(json.loads(sys.stdin.read() or "{}").get(sys.argv[1],""))' "$1"; }
  # Paid, through the front door rather than by decree. A bootstrapped
  # org is `free` like one a customer creates — there is no card step:
  # the provider is the merchant of record and only meets a card on its
  # subscription page. `admin set-plan --plan paid` used to do this in
  # one line and left an org that was paid with no customer and no
  # subscription behind it — a state no customer can be in, so the
  # billing screen the pass looked at was one no customer would ever see.
  # The subscription is opened on the provider's page: ask for it (which
  # makes acme's customer at the fake), then press the fake's "Subscribe"
  # the way a person would, so acme is paid through the same three
  # events a real completion sends.
  sub_url=$(api_json POST /v1/orgs/acme/billing/subscribe '{}' | jfield url)
  [ -n "$sub_url" ] || die "seed: the subscribe route gave no checkout page — is the Stripe fake up?"
  curl -sf -o /dev/null -X POST "$sub_url/complete" \
    || die "seed: acme could not subscribe (see $RUN/logs/server.log)"
  api POST /v1/orgs/acme/repos '{"name":"widget","public":true,"description":"the fast one — CI checks out from here"}'
  api POST /v1/orgs/acme/repos '{"name":"payments-api","description":"money, counted"}'
  api POST /v1/orgs/acme/repos '{"name":"ledger","description":"private ledger work"}'
  # Two commits, so a file has history to page through and a version to
  # switch back to.
  api POST /v1/orgs/acme/repos/widget/commits '{"message":"first commit","operations":[
    {"op":"put","path":"README.md","content":"# widget\n\nThe fast one.\n"},
    {"op":"put","path":"src/main.rs","content":"fn main() {}\n"},
    {"op":"put","path":"docs/guide.md","content":"# Guide\n"}]}'
  api POST /v1/orgs/acme/repos/widget/commits '{"message":"expand the readme","operations":[
    {"op":"put","path":"README.md","content":"# widget\n\nThe fast one. CI checks out from here.\n"}]}'
  api POST /v1/orgs/acme/repos/payments-api/commits '{"message":"scaffold","operations":[
    {"op":"put","path":"README.md","content":"# payments-api\n"}]}'

  # A person with a personal namespace, a filled-in profile, a pin and a
  # star.
  #
  # This exists because the manual browser pass kept walking a different
  # product than the one being built. The profile page and the star
  # control both render *identity* and *counts*, and a stack seeded only
  # with an org and three role accounts shows the empty rendering of
  # both — a handle over a repo grid, and a bare zero — which passes
  # while proving nothing. A page whose filled-in state a person never
  # looks at is a page whose filled-in state is broken for weeks.
  #
  # It has to go through signup rather than `admin user-create`, because
  # only signup creates a **personal namespace**, and `/{handle}` is a
  # personal namespace. It has to use a session rather than the org
  # token, because a profile edit and a star are both a *person's* acts
  # and the server refuses a service token for exactly that reason —
  # which is itself worth having exercised here.
  local jar=$RUN/ada.cookies handle=ada-dev email=ada@stratum.dev
  curl -sf -X POST "http://127.0.0.1:$HTTPPORT/v1/auth/signup" \
    -H 'Content-Type: application/json' \
    -d "{\"handle\":\"$handle\",\"email\":\"$email\",\"name\":\"Ada Lovelace\",\"password\":\"$PASSWORD\"}" \
    -o /dev/null || say "signup for $handle failed — profile seeding skipped"
  # The confirmation link is in the captured mail, the same place the
  # walkthrough reads it from.
  local verify=""
  for _ in $(seq 1 50); do
    verify=$(python3 - "$RUN/mail" "$email" <<'PYV'
# Search the message *body*, not `json.dumps(m)`.
#
# Dumping the message re-escapes its newlines as the two-character
# sequence backslash-n, which `\s` does not match — so a token pattern
# that excludes whitespace runs straight past the end of the URL and
# swallows the rest of the paragraph. That produced an 92-character
# token where the real one is 84, the verify POST failed, `curl -sf`
# swallowed the failure, and every seeding step after it ran with an
# empty cookie jar. The stack reported success and seeded nothing.
#
# The first version of this was checked against a hand-written fixture
# with the link on one line, which matched. Reality wraps the mail.
import json, os, re, sys
d, to = sys.argv[1], sys.argv[2]
for name in sorted(os.listdir(d)) if os.path.isdir(d) else []:
    if not name.endswith(".json"):
        continue
    m = json.load(open(os.path.join(d, name)))
    if m.get("to") != to:
        continue
    # An explicit charset rather than "not whitespace": the token is
    # base32-ish with underscores, and percent-escapes survive to be
    # decoded by the caller.
    hit = re.search(r"#verify=([A-Za-z0-9_%-]+)", m.get("text", ""))
    if hit:
        print(hit.group(1))
        break
PYV
)
    [ -n "$verify" ] && break
    sleep 0.1
  done
  if [ -n "$verify" ]; then
    verify=$(python3 -c "import sys,urllib.parse;print(urllib.parse.unquote(sys.argv[1]))" "$verify")
    curl -sf -X POST "http://127.0.0.1:$HTTPPORT/v1/auth/verify" \
      -c "$jar" -H 'Content-Type: application/json' \
      -d "{\"token\":\"$verify\"}" -o /dev/null \
      || say "verify failed for $email — profile seeding will be empty"
    # Prove the session exists before spending eight requests on it.
    #
    # Without this the failure is invisible: `curl -sf -o /dev/null`
    # reports nothing a human sees, so a broken verify produced an empty
    # cookie jar and every call below silently did nothing, while the
    # stack printed "stack up" and looked seeded. The first run of this
    # script did exactly that.
    if ! curl -sf -b "$jar" "http://127.0.0.1:$HTTPPORT/v1/auth/me" -o /dev/null; then
      say "no session for $email — profile, pins, star and mirror unseeded"
    fi
    # Says which call failed rather than failing quietly. A seeded stack
    # that is missing half its data looks like a broken product to the
    # next person, and they will debug the product.
    person() { curl -sf -X "$1" "http://127.0.0.1:$HTTPPORT$2" \
      -b "$jar" -c "$jar" -H 'Content-Type: application/json' -d "$3" -o /dev/null \
      || say "seed: $1 $2 failed"; }
    # Every field the rail renders, so the manual pass sees the filled-in
    # page rather than the fallback.
    person PATCH "/v1/users/$handle" '{"display_name":"Ada Lovelace",
      "bio":"Notes on the Analytical Engine, mostly.",
      "pronouns":"she/her","company":"Analytical Engines Ltd","location":"London",
      "links":[{"label":null,"url":"https://ada.example/"}]}'
    person POST "/v1/orgs/$handle/repos" '{"name":"engine","public":true,
      "description":"the analytical one"}'
    person POST "/v1/orgs/$handle/repos/engine/commits" '{"message":"first commit",
      "operations":[{"op":"put","path":"README.md","content":"# engine\n\nNotes.\n"}]}'
    # `repo`, not `name`: a pin names a repository the way a URL does.
    # The wrong key answers 422, which the silent curl swallowed.
    person PUT "/v1/users/$handle/pins" "{\"pins\":[{\"org\":\"$handle\",\"repo\":\"engine\"}]}"
    # A star on somebody else's public repo — the ordinary case, and the
    # one that used to be broken: starring is not a members-only act.
    person PUT "/v1/orgs/acme/repos/widget/star" ''
  else
    say "no confirmation mail for $email — profile and star seeding skipped"
  fi

  # A mirror, so the manual pass can see an imported count beside our
  # own. The fake GitHub reports 60,300 stars for any repository it is
  # asked about, which is the number from the product argument: a
  # migrated project's real reputation, shown separately and never
  # summed with the four stars it honestly has here.
  # `atlas-upstream`, not `atlas`. The walkthrough's mirror stage creates
  # `acme/atlas` itself, deliberately, to exercise the whole round trip —
  # connect the app, pick an installation, sync. Seeding the same name
  # first made that stage answer **409**, and the sync it was there to
  # watch "ended on: nothing at all". Two problems in the manual gate,
  # from a fixture that was only ever meant to give the browser pass an
  # imported star count to look at.
  #
  # The gate gets the clean name; this row's name is arbitrary and its
  # job is only to exist and carry a count.
  api POST /v1/orgs/acme/mirrors '{"name":"atlas-upstream","provider":"github",
    "origin":"acme-inc/atlas","public":true,"description":"mirrored from upstream"}' \
    || say "seed: mirror atlas-upstream failed — no imported star count to look at"

  # ------------------------------------------------------------------
  # A CI provider, and a repository for it to build.
  #
  # Every other check in this stack is *posted* by whatever is driving
  # the pass, which proves the reading side and nothing else. Four seams
  # only exist when a third party is really on the other end of the
  # wire: that the outbound webhook fires at all, that the signature we
  # send verifies under somebody else's check, that a `repo:read` token
  # clones from outside, and that the intake accepts what a real client
  # sends rather than what our own test helper sends.
  #
  # The repository is **private** on purpose. A public one clones with no
  # credential, and then the credential seam is not tested — the same
  # shape as SSH keys that were registered and revoked for weeks without
  # anything ever cloning with one.
  # ------------------------------------------------------------------
  say "ci provider…"
  api POST /v1/orgs/acme/repos \
    "{\"name\":\"$CI_REPO\",\"description\":\"builds on every push, through a real CI provider\"}" \
    || say "seed: repo $CI_REPO failed — the CI loop will not run"
  # `ci.sh` is what passing *means* here. The provider runs it and does
  # not know what is in it, which is the actual relationship between a
  # project and its CI.
  api POST "/v1/orgs/acme/repos/$CI_REPO/commits" '{"message":"first commit","operations":[
    {"op":"put","path":"README.md","content":"# pipeline\n\nCI clones this and runs ci.sh.\n"},
    {"op":"put","path":"ci.sh","content":"#!/bin/sh\n# What passing means for this repository. The CI provider runs this and\n# does not know what is in it.\n#\n# `:(exclude)ci.sh`, and it is load-bearing: git grep searches every\n# tracked file including this one, and this one has to name the marker in\n# order to search for it. Without the exclusion the check can never pass,\n# on any tree — which is exactly how it behaved until a walkthrough run\n# reported a tree with nothing wrong in it as failing.\nset -e\ntest -f README.md\nif git grep -lF '"'"'FIXME!!'"'"' -- . '"'"':(exclude)ci.sh'"'"' ; then\n  echo \"a leftover marker is still in the tree\"\n  exit 1\nfi\necho \"tree is clean\"\n"}]}' \
    || say "seed: $CI_REPO ci.sh failed — the provider will find nothing to run"

  # The three credentials the provider needs, each minted through the
  # route a real maintainer would use, and each shown exactly once.
  local ci_token ci_intake ci_hook
  ci_token=$(api_json POST /v1/orgs/acme/tokens \
    "{\"scopes\":[\"repo:read\"],\"repo\":\"$CI_REPO\",\"label\":\"ci-runner\"}" | jfield token)
  ci_intake=$(api_json POST "/v1/orgs/acme/repos/$CI_REPO/ci/secret" '' | jfield secret)
  ci_hook=$(api_json POST "/v1/orgs/acme/repos/$CI_REPO/webhooks" \
    "{\"url\":\"http://127.0.0.1:$CIPORT/hook\"}" | jfield secret)
  if [ -z "$ci_token" ] || [ -z "$ci_intake" ] || [ -z "$ci_hook" ]; then
    # Named individually: "CI is broken" sends the next person to the
    # provider, and three of the four ways this fails are on this side.
    say "seed: CI credentials incomplete (token=${ci_token:+ok} intake=${ci_intake:+ok} hook=${ci_hook:+ok})"
    say "      the CI provider will not start; the walkthrough's CI stages will say so"
  else
    CI_RUNNER_PORT=$CIPORT CI_RUNNER_PUBLIC_URL="http://127.0.0.1:$CIPORT" \
      STRATUM_URL="http://127.0.0.1:$HTTPPORT" \
      CI_RUNNER_ORG=acme CI_RUNNER_REPO="$CI_REPO" \
      CI_RUNNER_HOOK_SECRET="$ci_hook" CI_RUNNER_INTAKE_SECRET="$ci_intake" \
      CI_RUNNER_CLONE_TOKEN="$ci_token" CI_RUNNER_WORK="$RUN/ci-work" \
      python3 "$FAKES/ci-runner.py" > "$RUN/logs/ci-runner.log" 2>&1 &
    echo $! > "$RUN/ci-runner.pid"
    wait_for "ci provider" "$RUN/logs/ci-runner.log" \
      curl -fsS -o /dev/null "http://127.0.0.1:$CIPORT/healthz"
  fi

  # ------------------------------------------------------------------
  # An empty repository for the hosted-runner stages to push a workflow
  # into.
  #
  # Its own repo, not `widget`: the workflow stages push with the real
  # git CLI and then assert on what the Checks tab holds, and sharing a
  # repository with the CI provider's `ci/local` rows — or with the repo
  # stages that walk widget's file tree — would make each stage's
  # assertions depend on the other's leftovers. It is seeded with a
  # README and NOT with a workflow, because pushing the workflow is the
  # thing being tested.
  # ------------------------------------------------------------------
  say "workflow repo…"
  api POST /v1/orgs/acme/repos \
    "{\"name\":\"$WF_REPO\",\"description\":\"hosted runners build this one\"}" \
    || say "seed: repo $WF_REPO failed — the workflow stages will say so"
  api POST "/v1/orgs/acme/repos/$WF_REPO/commits" '{"message":"first commit","operations":[
    {"op":"put","path":"README.md","content":"# builds\n\nA workflow in .weft/ runs here on every push.\n"}]}' \
    || say "seed: $WF_REPO README failed"

  local SELF_HOSTED_STATUS
  if [ -n "$RUNNER_BIN" ]; then
    SELF_HOSTED_STATUS="$RUNNER_BIN. The walkthrough registers one from
               Settings → Runners and runs it. By hand: Add a runner there,
               then run the two commands it shows with --dir <somewhere>."
  else
    SELF_HOSTED_STATUS="OFF — no weft-runner binary (cargo build --release
               -p stratum-runner). The walkthrough's self-hosted stages will
               report this as a problem, not skip it."
  fi
  local RUNNER_STATUS
  if [ "$RUNNER_OK" = 1 ]; then
    RUNNER_STATUS="hosted runners on, through the ECS stand-in at
               http://127.0.0.1:$ECSPORT. acme/$WF_REPO has .weft/ci.yml;
               a push starts a real $RUNNER_IMAGE container. Tasks:
               docker ps -a --filter label=stratum.fake-ecs"
    if [ "$GITHUB_RUNNER_OK" = 1 ]; then
      RUNNER_STATUS="$RUNNER_STATUS
               GitHub Actions runners: on, as weft-gh-runner-local from
               $GITHUB_RUNNER_IMAGE (launch shape only: no GitHub here)"
    else
      RUNNER_STATUS="$RUNNER_STATUS
               GitHub Actions runners: OFF — no $GITHUB_RUNNER_IMAGE image
               (docker build --platform linux/amd64 -f Dockerfile.github-runner
               -t $GITHUB_RUNNER_IMAGE .)"
    fi
  else
    RUNNER_STATUS="OFF — no docker daemon or no $RUNNER_IMAGE image. The
               walkthrough's workflow stages will report this as a problem,
               not skip it."
  fi

  cat <<DONE

  stack up — http://127.0.0.1:$HTTPPORT

    sign in    ada@acme.dev / $PASSWORD  (owner)
               dev@acme.dev, view@acme.dev  (member, viewer)
    org token  $RUN/token
    logs       $RUN/logs/
    env        eval "\$(scripts/manual-stack.sh env)"

    ci         a real provider on http://127.0.0.1:$CIPORT, watching
               acme/$CI_REPO — push to it and it clones, runs ci.sh and
               signs a verdict back. What it did: /runs

    runners    ${RUNNER_STATUS}

    self-hosted ${SELF_HOSTED_STATUS}

  manual pass:
    eval "\$(scripts/manual-stack.sh env)"   # RUNNER_BIN, RUNNER_ECS_URL, CI_RUNNER_URL…
    cd web/dashboard && BASE=http://127.0.0.1:$HTTPPORT \\
      STRATUM_MAIL_DIR=$RUN/mail node tools/walkthrough.mjs

  stop:
    scripts/manual-stack.sh down
DONE
}

case "${1:-up}" in
  up) cmd_up ;;
  down) cmd_down ;;
  env) cmd_env ;;
  overage) shift; cmd_overage "$@" ;;
  *) die "usage: manual-stack.sh [up|down|env|overage <org> <gb>|clear]" ;;
esac
