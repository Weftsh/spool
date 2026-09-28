#!/usr/bin/env bash
# The stack the manual browser pass needs, brought up from nothing.
#
# CLAUDE.md requires the manual pass to run against a *fully configured*
# deployment — Postgres, MinIO, the built dashboard, the SSH front door,
# captured mail, a stand-in for GitHub, a real CI provider on the other
# end of the webhook, and a self-hosted runner binary for the workflow
# stages — and then gave no way to build one. So it got rebuilt by hand
# each time, and each rebuild rediscovered the same four defects:
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
#
# Everything lives under .stack/ and is disposable: `up` starts from an
# empty database every time, because a manual pass against yesterday's
# leftovers is a pass against something nobody will ever deploy.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
RUN=${STRATUM_STACK_DIR:-$ROOT/.stack}
FAKES=$ROOT/scripts/manual-stack

# Every fixed port sits below 32768, outside Linux's default ephemeral
# range (net.ipv4.ip_local_port_range, 32768-60999). These used to be
# 55432, 59000, 59110 and 59120, inside it — and on a machine where test
# suites are opening loopback connections by the thousand, the kernel
# walks its outgoing local ports straight through them. A port an
# outgoing socket (or its TIME_WAIT) holds is not LISTENing, so the
# free-port check below cannot see it; docker then refused to publish
# postgres with "address already in use" on a port nothing was serving.
PGPORT=${PGPORT:-25432}
MINIOPORT=${MINIOPORT:-29000}
HTTPPORT=${HTTPPORT:-8080}
SSHPORT=${SSHPORT:-2222}
PG_CONTAINER=${PG_CONTAINER:-stratum-stack-pg}
MINIO_CONTAINER=${MINIO_CONTAINER:-stratum-stack-minio}
GITHUBPORT=${GITHUBPORT:-29110}
# The miniature CI provider. Not a mock of one: it verifies our webhook
# signature, clones with a real credential, runs the repository's own
# ci.sh, and signs a verdict back into the intake. See
# scripts/manual-stack/ci-runner.py.
CIPORT=${CIPORT:-29120}
CI_REPO=${CI_REPO:-pipeline}
# The repository the workflow and self-hosted-runner stages push
# `.weft/ci.yml` into. Seeded with a README and no workflow, because
# pushing the workflow is the thing being tested.
WF_REPO=${WF_REPO:-builds}
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
  for p in server ci-runner github minio pg; do
    if [ -f "$RUN/$p.pid" ]; then
      kill "$(cat "$RUN/$p.pid")" 2>/dev/null || true
      rm -f "$RUN/$p.pid"
    fi
  done
  # The postgres postmaster is started through `su`, so the pid file
  # names the wrapper rather than the server itself.
  pkill -f "postgres -D $RUN/pgdata" 2>/dev/null || true
  pkill -f "minio server $RUN/miniodata" 2>/dev/null || true
  # A self-hosted runner the walkthrough started and did not get to stop
  # (a pass interrupted mid-way). It would take the next pass's first job
  # under a name that pass never registered.
  pkill -f "weft-runner run --dir .*walk-runner-" 2>/dev/null || true
  if command -v docker > /dev/null 2>&1 && docker info > /dev/null 2>&1; then
    docker rm -f "$PG_CONTAINER" "$MINIO_CONTAINER" > /dev/null 2>&1 || true
  fi
  # Wait for the listeners to actually go, rather than sleeping and
  # hoping. `kill` returns as soon as the signal is delivered, and a
  # docker port publisher outlives the container by a moment, so `down`
  # used to hand back a stack whose ports were still bound. Whatever ran
  # next — `up` again, or a deployment smoke test that wants the same
  # 8080 and 2222 — then failed on a port it had every reason to think
  # was free, and the error said nothing about why.
  wait_ports_free "$HTTPPORT" "$SSHPORT" "$PGPORT" "$MINIOPORT" \
    "$GITHUBPORT" "$CIPORT"
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

# An S3 request signed as the stack's MinIO root, through curl's own
# SigV4 — no SDK, no `mc`. Prints the HTTP status.
s3() { # s3 <method> <path>
  curl -s -o /dev/null -w '%{http_code}' -X "$1" \
    --aws-sigv4 "aws:amz:us-east-1:s3" --user minioadmin:minioadmin \
    "http://127.0.0.1:$MINIOPORT$2"
}

write_env() {
  # No backticks anywhere in this heredoc: it is unquoted, so a
  # backtick is command substitution and a comment mentioning one
  # gets *run*.
  cat > "$RUN/env.sh" <<ENV
export AWS_ACCESS_KEY_ID=minioadmin
export AWS_SECRET_ACCESS_KEY=minioadmin
export AWS_REGION=us-east-1
export STRATUM_DB_URL="postgres://stratum@127.0.0.1:$PGPORT/stratum"
export STRATUM_STORE_URL="http://127.0.0.1:$MINIOPORT/$BUCKET"
export STRATUM_DATA_DIR="$RUN/data"
export STRATUM_DASHBOARD_DIR="$ROOT/web/dashboard/dist"
# Loopback only. Nothing in this stack runs in a container that would
# need to reach the server from outside the host: the self-hosted runner
# is a native process, the way a customer runs it.
export STRATUM_BIND="127.0.0.1:$HTTPPORT"
export STRATUM_PUBLIC_URL="http://127.0.0.1:$HTTPPORT"
export STRATUM_WEBHOOK_SECRET="manual-stack-secret"
# The SSH front door. Without it the dashboard correctly hides the SSH
# clone row and the pass becomes a walkthrough of a different product.
export STRATUM_SSH_BIND="127.0.0.1:$SSHPORT"
export STRATUM_SSH_HOST_KEY="\$(cat "$RUN/host-key")"
export STRATUM_SSH_PUBLIC_URL="ssh://git@127.0.0.1:$SSHPORT"
# Mail to a directory, so the pass opens an invitation the way the person
# it was sent to does.
export STRATUM_MAIL_TRANSPORT="capture"
export STRATUM_MAIL_FROM="no-reply@stratum.test"
export STRATUM_MAIL_DIR="$RUN/mail"
# A GitHub App pointed at the local fake, and a git base that is a
# directory of bare repositories — so mirroring fetches from disk, a
# push to a mirror is forwarded to disk, and no packet leaves this
# machine.
export STRATUM_GITHUB_APP_ID="12345"
export STRATUM_GITHUB_APP_KEY_PEM="$RUN/gh-app-key.pem"
export STRATUM_GITHUB_API_BASE="http://127.0.0.1:$GITHUBPORT"
export STRATUM_GITHUB_GIT_BASE="file://$RUN/origins"
export STRATUM_GITHUB_INSTALL_URL="http://127.0.0.1:$GITHUBPORT/apps/stratum/installations/new"
# The App's OAuth client, so the install callback proves the installer
# controls the installation and GitHub sign-in has a client to use — the
# fake exchanges any code_owning_ code followed by the installation id.
# (No angle brackets in this heredoc: bash 3.2 reads them as
# redirections inside a command substitution.)
export STRATUM_GITHUB_CLIENT_ID="Iv1.fake"
export STRATUM_GITHUB_CLIENT_SECRET="fake-client-secret"
export STRATUM_GITHUB_OAUTH_BASE="http://127.0.0.1:$GITHUBPORT"
# 5s is the production default and makes every workflow stage of the
# manual pass wait on a poll it does not care about.
export STRATUM_RUNNER_POLL_SECS="1"
# The local CI provider and the repository it watches, and the
# repository the workflow stages push to. The walkthrough reads these to
# drive the loops; without them its stages report a missing prerequisite
# rather than passing quietly, the same way a missing SSH URL does.
export CI_RUNNER_URL="http://127.0.0.1:$CIPORT"
export CI_RUNNER_REPO="$CI_REPO"
export RUNNER_WF_REPO="$WF_REPO"
# Where the stack is and an org admin token for poking it by hand. The
# token read is escaped on purpose, so it happens when the caller evals
# this file rather than when the file is written: write_env runs before
# bootstrap mints the token, so baking the value in captured the
# *previous* stack's token and every call 401d.
export BASE="http://127.0.0.1:$HTTPPORT"
export STACK_TOKEN="\$(cat "$RUN/token" 2>/dev/null)"
ENV
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

cmd_down() { stop_all; say "stack down"; }

cmd_up() {
  command -v git >/dev/null || die "git is required"
  command -v lsof >/dev/null || die "lsof is required (to tell a free port from a held one)"
  command -v openssl >/dev/null || die "openssl is required (the fake GitHub App's key)"
  # The SSH host key, and the walkthrough's own client key and clone, all
  # need OpenSSH. Said here rather than discovered as an empty host key
  # and a server that will not start its SSH door.
  command -v ssh-keygen >/dev/null && command -v ssh >/dev/null \
    || die "the OpenSSH client (ssh, ssh-keygen) is required — e.g. apt-get install openssh-client"
  # Docker first, and by default. postgres and minio on the host needed a
  # Debian layout, a `postgres` system user, and root to `su` to it —
  # three assumptions that hold on the CI image and on nothing else, so
  # the stack simply would not come up on a development machine. The
  # containers need none of them, which is also how the product is
  # actually deployed. Set STRATUM_STACK_NO_DOCKER=1 to force the host path.
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
    # The host path runs postgres as its own uid, through `su`.
    id postgres > /dev/null 2>&1 || die \
      "no docker daemon, and no 'postgres' user to run the host PostgreSQL as.
Start Docker, or create the user (the Debian postgresql package does)."
    [ "$(id -u)" = 0 ] || die \
      "no docker daemon, and the host PostgreSQL path needs root to su to 'postgres'.
Start Docker, or run as root."
  fi
  # MinIO: the pinned image under docker, the pinned binary otherwise —
  # the same release `.minio-version` pins for the test harness, so the
  # pass and the suite are not talking to two different stores.
  #
  # A daemon that answers is not a daemon that can pull. One whose
  # registry access is broken (a proxy it was not told about, quay.io
  # refusing the manifest HEAD with a 401) used to stop the stack at
  # "minio…" after postgres was already up; with a host binary on hand it
  # runs that instead and says so. On macOS the host binary can never be
  # had: MinIO publishes no darwin build any more (scripts/fetch-minio.sh).
  local minio_release minio_image pull_log
  minio_release=$(tr -d '[:space:]' < "$ROOT/.minio-version")
  # STRATUM_MINIO_IMAGE names another copy of it, the same variable
  # scripts/fetch-minio.sh reads (a repository, no tag).
  minio_image="${STRATUM_MINIO_IMAGE:-quay.io/minio/minio}:$minio_release"
  MINIO_HOST=1
  if [ "$USE_DOCKER" = 1 ]; then
    pull_log=$(mktemp)
    if docker image inspect "$minio_image" > /dev/null 2>&1 \
       || docker pull -q "$minio_image" > "$pull_log" 2>&1; then
      MINIO_HOST=0
    elif [ -x "$ROOT/.testkit/bin/minio" ]; then
      # The end of docker's sentence is the reason; the start is the name.
      say "minio: docker could not pull $minio_image (…$(tail -1 "$pull_log" | tail -c 90))"
      say "  running .testkit/bin/minio on the host instead"
    else
      die "docker could not pull $minio_image, and there is no .testkit/bin/minio to fall back to:
$(tail -3 "$pull_log")
Fix the daemon's registry access, or run scripts/fetch-minio.sh (Linux)."
    fi
    rm -f "$pull_log"
  fi
  if [ "$MINIO_HOST" = 1 ]; then
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
    die "no server binary — run: cargo build --release -p stratum-server -p stratum-runner"
  fi
  # Both browser gates read dist, not src: a source edit with no build
  # behind it is tested against the previous bundle.
  [ -f "$ROOT/web/dashboard/dist/index.html" ] \
    || die "no web/dashboard/dist — run: (cd web/dashboard && npm ci && npm run build)"
  if [ -n "$(find "$ROOT/web/dashboard/src" -newer "$ROOT/web/dashboard/dist/index.html" -print -quit)" ]; then
    say "dashboard: web/dashboard/dist is older than web/dashboard/src — the pass will"
    say "  test the previous bundle. Rebuild: (cd web/dashboard && npm run build)"
  fi
  # The runner binary for self-hosted runners, picked from the same
  # profile as the server — a release server next to a debug runner from
  # last week is two different builds of the product — and said out loud
  # when it is stale against its own sources.
  RUNNER_BIN=
  local profile
  profile=$(basename "$(dirname "$bin")")
  for cand in "$ROOT/target/$profile/weft-runner" "$ROOT/target/release/weft-runner" \
              "$ROOT/target/debug/weft-runner"; do
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
  # silently — and hand the pass a stack whose GitHub and CI were
  # somebody else's, hours old and watching a server that was gone. The
  # ci stages then failed on a provider that "never reported", which
  # read as a product bug and was leftover Python processes.
  ports_free_or_die "$HTTPPORT" "$SSHPORT" "$PGPORT" "$MINIOPORT" \
    "$GITHUBPORT" "$CIPORT"
  rm -rf "$RUN/pgdata" "$RUN/miniodata" "$RUN/data" "$RUN/mail" "$RUN/ci-work" "$RUN/origins"
  # `$RUN/miniodata/$BUCKET` is made before minio starts so it is there
  # the first time the server writes; the bucket is then *proved* over
  # the S3 API below, rather than assumed from a directory.
  mkdir -p "$RUN/pgdata" "$RUN/miniodata/$BUCKET" "$RUN/data" "$RUN/mail" "$RUN/logs" \
           "$RUN/origins" "$RUN/ci-work"

  # postgres runs under its own uid, so every directory on the way down
  # to PGDATA has to be traversable by it — not just PGDATA itself.
  if [ "$USE_DOCKER" = 0 ]; then
    local anc=$RUN
    while [ "$anc" != "/" ]; do
      chmod o+x "$anc" 2>/dev/null || true
      anc=$(dirname "$anc")
    done
  fi

  # Credentials, generated rather than committed. A private key in a
  # repository is a private key somebody will eventually reuse.
  [ -f "$RUN/host-key" ] || ssh-keygen -q -t ed25519 -N "" -C manual-stack -f "$RUN/host-key"
  [ -f "$RUN/gh-app-key.pem" ] || openssl genrsa -out "$RUN/gh-app-key.pem" 2048 2>/dev/null

  # Origins for the mirror flow to fetch from, and forward pushes to,
  # over file://. Rebuilt every time with the rest of the stack: a push
  # through the mirror lands here, and yesterday's pushes are yesterday's.
  for name in acme-inc/widget acme-inc/atlas; do
    local bare=$RUN/origins/$name.git
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

  else
    say "postgres…"
    chown -R postgres:postgres "$RUN/pgdata"
    # `-s /bin/sh`: the postgres account's own shell may be nologin, and
    # `su` then refuses with "This account is currently not available".
    su -s /bin/sh postgres -c "$pgbin/initdb -D $RUN/pgdata -U stratum -A trust --no-sync" \
      > "$RUN/logs/initdb.log" 2>&1 || { tail -5 "$RUN/logs/initdb.log" >&2; exit 1; }
    su -s /bin/sh postgres -c "$pgbin/postgres -D $RUN/pgdata -p $PGPORT -k $RUN/pgdata -c listen_addresses=127.0.0.1" \
      > "$RUN/logs/pg.log" 2>&1 &
    echo $! > "$RUN/pg.pid"
    wait_for postgres "$RUN/logs/pg.log" "$pgbin/pg_isready" -h 127.0.0.1 -p "$PGPORT" -q
    "$pgbin/createdb" -h 127.0.0.1 -p "$PGPORT" -U stratum stratum
  fi

  if [ "$MINIO_HOST" = 0 ]; then
    say "minio… (container)"
    docker run -d --name "$MINIO_CONTAINER" \
      -p "127.0.0.1:$MINIOPORT:9000" \
      -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
      -e MINIO_BROWSER=off \
      -v "$RUN/miniodata:/data" \
      "$minio_image" server /data --address ":9000" > /dev/null
    docker logs -f "$MINIO_CONTAINER" > "$RUN/logs/minio.log" 2>&1 &
  else
    say "minio…"
    MINIO_ROOT_USER=minioadmin MINIO_ROOT_PASSWORD=minioadmin MINIO_BROWSER=off \
      "$ROOT/.testkit/bin/minio" server "$RUN/miniodata" --address "127.0.0.1:$MINIOPORT" \
      > "$RUN/logs/minio.log" 2>&1 &
    echo $! > "$RUN/minio.pid"
  fi
  # /health/ready, not /health/live: liveness turns 200 before the S3
  # API is serving, which is how the bucket write right after it used
  # to fail.
  wait_for minio "$RUN/logs/minio.log" \
    curl -fsS "http://127.0.0.1:$MINIOPORT/minio/health/ready"
  # The bucket, proved rather than assumed. A directory made before the
  # server started is a bucket to some MinIO releases and not to others;
  # a HEAD over the API is the question the server will actually ask.
  if [ "$(s3 HEAD "/$BUCKET")" != 200 ]; then
    local made
    made=$(s3 PUT "/$BUCKET")
    [ "$(s3 HEAD "/$BUCKET")" = 200 ] \
      || die "minio is up but bucket '$BUCKET' could not be made (PUT answered $made); see $RUN/logs/minio.log"
  fi

  say "fake github…"
  STRATUM_PUBLIC_URL="http://127.0.0.1:$HTTPPORT" FAKE_GITHUB_PORT=$GITHUBPORT \
    python3 "$FAKES/fake-github.py" > "$RUN/logs/github.log" 2>&1 &
  echo $! > "$RUN/github.pid"
  wait_for github "$RUN/logs/github.log" \
    curl -fsS -o /dev/null "http://127.0.0.1:$GITHUBPORT/app/installations"

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

  # Every seed call says which call failed rather than failing quietly.
  # A seeded stack that is missing half its data looks like a broken
  # product to the next person, and they will debug the product.
  api() {
    curl -sf -X "$1" "http://127.0.0.1:$HTTPPORT$2" \
      -H "Authorization: Bearer $token" -H 'Content-Type: application/json' -d "$3" -o /dev/null \
      || { say "seed: $1 $2 failed (see $RUN/logs/server.log)"; return 1; }
  }
  # The same call, but hand the body back. Secrets and tokens are shown
  # exactly once by the routes that mint them, so a discarded response
  # is a credential that cannot be recovered.
  api_json() { curl -sf -X "$1" "http://127.0.0.1:$HTTPPORT$2" \
    -H "Authorization: Bearer $token" -H 'Content-Type: application/json' -d "$3"; }
  # One field out of a JSON object, or empty. `python3 -c` rather than a
  # jq dependency: nothing else in this script needs one.
  jfield() { python3 -c 'import json,sys;print(json.loads(sys.stdin.read() or "{}").get(sys.argv[1],""))' "$1"; }

  # Every repository is private to its organisation; there is no other
  # kind, so none of these says so.
  api POST /v1/orgs/acme/repos '{"name":"widget","description":"the fast one"}' || true
  api POST /v1/orgs/acme/repos '{"name":"payments-api","description":"money, counted"}' || true
  api POST /v1/orgs/acme/repos '{"name":"ledger","description":"private ledger work"}' || true
  # Two commits, so a file has history to page through and a version to
  # switch back to.
  api POST /v1/orgs/acme/repos/widget/commits '{"message":"first commit","operations":[
    {"op":"put","path":"README.md","content":"# widget\n\nThe fast one.\n"},
    {"op":"put","path":"src/main.rs","content":"fn main() {}\n"},
    {"op":"put","path":"docs/guide.md","content":"# Guide\n"}]}' || true
  api POST /v1/orgs/acme/repos/widget/commits '{"message":"expand the readme","operations":[
    {"op":"put","path":"README.md","content":"# widget\n\nThe fast one, and the one this stack clones.\n"}]}' || true
  api POST /v1/orgs/acme/repos/payments-api/commits '{"message":"scaffold","operations":[
    {"op":"put","path":"README.md","content":"# payments-api\n"}]}' || true

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
  # ------------------------------------------------------------------
  say "ci provider…"
  api POST /v1/orgs/acme/repos \
    "{\"name\":\"$CI_REPO\",\"description\":\"builds on every push, through a real CI provider\"}" \
    || say "      the CI loop will not run"
  # `ci.sh` is what passing *means* here. The provider runs it and does
  # not know what is in it, which is the actual relationship between a
  # project and its CI.
  api POST "/v1/orgs/acme/repos/$CI_REPO/commits" '{"message":"first commit","operations":[
    {"op":"put","path":"README.md","content":"# pipeline\n\nCI clones this and runs ci.sh.\n"},
    {"op":"put","path":"ci.sh","content":"#!/bin/sh\n# What passing means for this repository. The CI provider runs this and\n# does not know what is in it.\n#\n# `:(exclude)ci.sh`, and it is load-bearing: git grep searches every\n# tracked file including this one, and this one has to name the marker in\n# order to search for it. Without the exclusion the check can never pass,\n# on any tree — which is exactly how it behaved until a walkthrough run\n# reported a tree with nothing wrong in it as failing.\nset -e\ntest -f README.md\nif git grep -lF '"'"'FIXME!!'"'"' -- . '"'"':(exclude)ci.sh'"'"' ; then\n  echo \"a leftover marker is still in the tree\"\n  exit 1\nfi\necho \"tree is clean\"\n"}]}' \
    || say "      the provider will find nothing to run"

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
  # An empty repository for the workflow and self-hosted-runner stages to
  # push a workflow into.
  #
  # Its own repo, not `widget`: those stages push with the real git CLI
  # and then assert on what the Checks tab holds, and sharing a
  # repository with the CI provider's `ci/local` rows — or with the repo
  # stages that walk widget's file tree — would make each stage's
  # assertions depend on the other's leftovers. No runner is registered
  # here: registering one from the command Settings → Runners shows is
  # what the walkthrough tests.
  # ------------------------------------------------------------------
  say "workflow repo…"
  api POST /v1/orgs/acme/repos \
    "{\"name\":\"$WF_REPO\",\"description\":\"self-hosted runners build this one\"}" \
    || say "      the workflow stages will say so"
  api POST "/v1/orgs/acme/repos/$WF_REPO/commits" '{"message":"first commit","operations":[
    {"op":"put","path":"README.md","content":"# builds\n\nA workflow in .weft/ runs here, on a registered runner, on every push.\n"}]}' \
    || true

  local SELF_HOSTED_STATUS
  if [ -n "$RUNNER_BIN" ]; then
    SELF_HOSTED_STATUS="$RUNNER_BIN. The walkthrough registers one from
               Settings → Runners and runs it on this machine. By hand: Add a
               runner there, then run the two commands it shows with
               --dir <somewhere>; push a .weft/ci.yml to acme/$WF_REPO."
  else
    SELF_HOSTED_STATUS="OFF — no weft-runner binary (cargo build --release
               -p stratum-runner). The walkthrough's self-hosted stages will
               report this as a problem, not skip it."
  fi

  cat <<DONE

  stack up — http://127.0.0.1:$HTTPPORT   (server: $bin)

    sign in    ada@acme.dev / $PASSWORD  (owner)
               dev@acme.dev, view@acme.dev  (member, viewer)
    org token  $RUN/token
    logs       $RUN/logs/
    mail       $RUN/mail/
    env        eval "\$(scripts/manual-stack.sh env)"

    ci         a real provider on http://127.0.0.1:$CIPORT, watching
               acme/$CI_REPO — push to it and it clones, runs ci.sh and
               signs a verdict back. What it did: /runs

    github     a stand-in App on http://127.0.0.1:$GITHUBPORT; mirrors
               fetch from, and forward pushes to, $RUN/origins

    runners    ${SELF_HOSTED_STATUS}

  manual pass:
    eval "\$(scripts/manual-stack.sh env)"   # RUNNER_BIN, CI_RUNNER_URL, RUNNER_WF_REPO…
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
  *) die "usage: manual-stack.sh [up|down|env]" ;;
esac
