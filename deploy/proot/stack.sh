#!/usr/bin/env bash
# The prod-parity stack of deploy/compose.yml, without a container runtime.
#
#   stack.sh up      pull + start postgres and minio, create the bucket,
#                    and start the ECS stand-in with its proot backend
#   stack.sh app     start the production image built by build-image.sh
#   stack.sh smoke   run deploy/smoke.sh against it (HTTP + SSH, and the
#                    two runner legs when the runner images are built)
#   stack.sh down    stop everything
#
# The runner images: build-image.sh puts them at $PROOT_WORK/runner and
# $PROOT_WORK/github-runner; `up` and `smoke` look there and say what
# they found.
#
# Each service is an image root filesystem run under PRoot by run.sh. A
# Fargate task's containers share one network namespace, and so do these
# processes, so the compose service names resolve to 127.0.0.1 through a
# hosts file bound over /etc/hosts. The values below are compose.yml's;
# the two must not drift, and `deploy/proot/README.md` says which lines
# to change together.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"; repo="$(cd "$here/../.." && pwd)"
work="${PROOT_WORK:-$HOME/.proot}"; S="$work/stack"; mkdir -p "$S"
run="$here/run.sh"
MINIO_IMAGE="${MINIO_IMAGE:-ghcr.io/weftsh/minio:$(tr -d '[:space:]' < "$repo/.minio-version")}"
POSTGRES_IMAGE="${POSTGRES_IMAGE:-ghcr.io/weftsh/postgres:$(tr -d '[:space:]' < "$repo/.postgres-version")}"
printf '127.0.0.1 localhost postgres minio fake-ecs app\n::1 localhost\n' > "$S/hosts"
hosts=(-b "$S/hosts:/etc/hosts")
wait_for() { # <label> <seconds> <cmd...>
  local label=$1 secs=$2; shift 2
  for i in $(seq 1 "$secs"); do "$@" >/dev/null 2>&1 && { echo "$label ready after ${i}s"; return 0; }; sleep 1; done
  echo "$label not ready after ${secs}s" >&2; return 1
}
pull() { [ -f "$2/.image.json" ] || { echo "pulling $1"; python3 "$here/pull-image.py" "$1" "$2"; }; }

case "${1:-}" in
up)
  pull "$POSTGRES_IMAGE" "$S/postgres"; pull "$MINIO_IMAGE" "$S/minio"
  mkdir -p "$S/pgdata" "$S/pgrun" "$S/minio-data"
  # compose.yml: postgres, POSTGRES_USER/PASSWORD/DB stratum. Listens on
  # loopback only: the task's network is the job's, not a compose bridge.
  "$run" "$S/postgres" -u "$(id -u):$(id -g)" "${hosts[@]}" -b "$S/pgdata:/var/lib/postgresql/data" -b "$S/pgrun:/var/run/postgresql" \
    -e POSTGRES_USER=stratum -e POSTGRES_PASSWORD=stratum -e POSTGRES_DB=stratum \
    -- docker-entrypoint.sh postgres -c listen_addresses=127.0.0.1 > "$S/postgres.log" 2>&1 &
  echo $! > "$S/postgres.pid"
  wait_for postgres 60 "$run" "$S/postgres" -u "$(id -u):$(id -g)" -- pg_isready -h 127.0.0.1 -U stratum
  # compose.yml: minio, server /data --address :9000, root user stratum.
  "$run" "$S/minio" "${hosts[@]}" -b "$S/minio-data:/data" \
    -e MINIO_ROOT_USER=stratum -e MINIO_ROOT_PASSWORD=stratum-dev-only -e MINIO_BROWSER=off \
    -- minio server /data --address 127.0.0.1:9000 > "$S/minio.log" 2>&1 &
  echo $! > "$S/minio.pid"
  wait_for minio 30 curl -sf http://127.0.0.1:9000/minio/health/ready
  # compose.yml's minio-init (`mc mb -p local/stratum`), as one signed
  # PUT: mc is a second image to fetch, and a bucket is one request.
  python3 "$here/s3-mkbucket.py" http://127.0.0.1:9000 stratum stratum stratum-dev-only
  # compose.yml's fake-ecs, with tasks as processes under PRoot. The
  # runner root filesystems are what build-image.sh leaves; without them
  # the stand-in still answers a RunTask, with AGENT failures, and the
  # smoke's runner legs stay off.
  mkdir -p "$S/tasks"
  FAKE_ECS_ACCESS_KEY_ID=AKIALOCALDISPATCH FAKE_ECS_SECRET_ACCESS_KEY=local-dispatch-secret-not-a-real-key \
  FAKE_ECS_REGION=us-east-1 FAKE_ECS_CLUSTER=local \
  FAKE_ECS_TASK_DEFINITION=weft-runner-local FAKE_ECS_RUNNER_IMAGE="$work/runner" \
  FAKE_ECS_GITHUB_TASK_DEFINITION=weft-gh-runner-local FAKE_ECS_GITHUB_RUNNER_IMAGE="$work/github-runner" \
  FAKE_ECS_BACKEND=proot FAKE_ECS_PROOT_RUN="$run" FAKE_ECS_PROOT_WORK="$S/tasks" FAKE_ECS_PROOT_HOSTS="$S/hosts" \
  PROOT="${PROOT:-$here/bin/proot}" \
    python3 "$repo/deploy/fake-ecs/fake-ecs.py" > "$S/fake-ecs.log" 2>&1 &
  echo $! > "$S/fake-ecs.pid"
  wait_for fake-ecs 15 curl -sf http://127.0.0.1:8443/
  [ -d "$work/runner/usr" ] && echo "runner image: $work/runner" || echo "runner image: not built (build-image.sh Dockerfile.runner)" ;;
app)
  rootfs="${2:-$work/app}"
  [ -f "$rootfs/.image.json" ] || { echo "no image at $rootfs: run build-image.sh first" >&2; exit 1; }
  [ -s "$S/ssh-host-key" ] || ssh-keygen -t ed25519 -N "" -C stratum-proot -f "$S/ssh-host-key" >/dev/null
  mkdir -p "$S/appdata"
  # compose.yml's `app` environment, hostnames as in the hosts file.
  app_env=(
    -e STRATUM_DB_URL=postgres://stratum:stratum@postgres:5432/stratum
    -e STRATUM_STORE_URL=http://minio:9000/stratum
    -e AWS_ACCESS_KEY_ID=stratum -e AWS_SECRET_ACCESS_KEY=stratum-dev-only -e AWS_REGION=us-east-1
    -e STRATUM_PUBLIC_URL=http://127.0.0.1:8080 -e STRATUM_WEBHOOK_SECRET=local-smoke-secret
    -e STRATUM_SSH_BIND=0.0.0.0:2222 -e STRATUM_SSH_PUBLIC_URL=ssh://git@127.0.0.1:2222
    -e STRATUM_CDN_BASE=http://127.0.0.1:8080 -e STRATUM_CDN_ORIGIN_SECRET=local-smoke-cdn-secret
    -e "STRATUM_SSH_HOST_KEY=$(cat "$S/ssh-host-key")"
    -e TINI_SUBREAPER=1
    # Hosted runners, as compose.yml hands them to the app: the same
    # variables the terraform root passes, the ECS endpoint at the
    # stand-in, the budget controls set so the metered path runs.
    -e STRATUM_RUNNER_ECS_CLUSTER=local -e STRATUM_RUNNER_ECS_TASK_DEFINITION=weft-runner-local
    -e STRATUM_RUNNER_ECS_GITHUB_TASK_DEFINITION=weft-gh-runner-local
    -e STRATUM_RUNNER_ECS_SUBNETS=subnet-local -e STRATUM_RUNNER_ECS_SECURITY_GROUP=sg-local
    -e STRATUM_RUNNER_ECS_URL=http://fake-ecs:8443
    -e STRATUM_RUNNER_AWS_ACCESS_KEY_ID=AKIALOCALDISPATCH
    -e STRATUM_RUNNER_AWS_SECRET_ACCESS_KEY=local-dispatch-secret-not-a-real-key
    -e STRATUM_RUNNER_AWS_REGION=us-east-1
    -e STRATUM_RUNNER_MINUTES_PER_MONTH=2000 -e STRATUM_RUNNER_MAX_TIMEOUT_MINUTES=360
    -e STRATUM_RUNNER_URL=http://app:8080
  )
  for v in HTTPS_PROXY https_proxy NO_PROXY no_proxy GIT_SSL_CAINFO SSL_CERT_FILE; do
    [ -n "${!v:-}" ] && app_env+=(-e "$v=${!v}")
  done
  printf '%s\n' "$rootfs" > "$S/app.rootfs"; printf '%s\0' "${app_env[@]}" > "$S/app.env"
  "$run" "$rootfs" "${hosts[@]}" -b "$S/appdata:/var/lib/stratum" ${EXTRA_CA:+-b "$EXTRA_CA:$EXTRA_CA"} "${app_env[@]}" \
    > "$S/app.log" 2>&1 &
  echo $! > "$S/app.pid"
  wait_for app 60 curl -sf http://127.0.0.1:8080/healthz
  curl -s -o /dev/null -w 'readyz: %{http_code}\n' http://127.0.0.1:8080/readyz ;;
smoke)
  rootfs=$(cat "$S/app.rootfs"); mapfile -d '' app_env < "$S/app.env"
  cat > "$S/bootstrap.sh" <<BS
#!/usr/bin/env bash
exec "$run" "$rootfs" $(printf '%q ' "${hosts[@]}") -b "$S/appdata:/var/lib/stratum" $(printf '%q ' "${app_env[@]}") -- stratum-server admin bootstrap "\$@"
BS
  chmod +x "$S/bootstrap.sh"
  legs=()
  if [ -d "$work/runner/usr" ]; then
    # A runner started by the ECS stand-in reaches the app at `app`,
    # which the stack's hosts file makes 127.0.0.1; the self-hosted
    # leg's runner is a process under the same PRoot driver.
    legs=(SMOKE_RUNNER=1 SMOKE_SELF_HOSTED=1 SMOKE_RUNNER_IMAGE="$work/runner" SMOKE_RUNNER_URL=http://app:8080
          SMOKE_RUNNER_DRIVER="$here/smoke-runner-driver.sh" SMOKE_RUNNER_WORK="$S/smoke-runners" SMOKE_RUNNER_HOSTS="$S/hosts")
  else
    echo "runner legs off: no runner image at $work/runner"
  fi
  cd "$repo" && env BASE_URL=http://127.0.0.1:8080 SSH_ENDPOINT=ssh://git@127.0.0.1:2222 BOOTSTRAP_CMD="$S/bootstrap.sh" \
    SMOKE_RUN_ID="proot-$(date +%s)" PROOT="${PROOT:-$here/bin/proot}" ${legs[@]+"${legs[@]}"} bash ${SMOKE_TRACE:+-x} deploy/smoke.sh ;;
down)
  for p in app fake-ecs minio postgres; do [ -f "$S/$p.pid" ] && { pkill -P "$(cat "$S/$p.pid")" 2>/dev/null; kill "$(cat "$S/$p.pid")" 2>/dev/null; rm -f "$S/$p.pid"; }; done
  pkill -x stratum-server 2>/dev/null; pkill -x minio 2>/dev/null; pkill -x postgres 2>/dev/null; pkill -x weft-runner 2>/dev/null; echo "stack down" ;;
*) sed -n '2,8p' "$0"; exit 2 ;;
esac
