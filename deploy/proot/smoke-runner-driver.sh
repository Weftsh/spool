# The self-hosted smoke leg's "other machine" as a process under PRoot.
#
# Sourced by deploy/smoke.sh when SMOKE_RUNNER_DRIVER names this file. The
# runner image is a root filesystem (SMOKE_RUNNER_IMAGE names the directory,
# as fake-ecs's proot backend does), the process runs as uid 10002 with a
# scratch /work, and the four verbs the leg needs are a pid file, a log
# file and a session to signal. The exit code is written by the wrapper
# the moment the runner exits, which is what a container's State.ExitCode
# is once the daemon has noticed.
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SMOKE_RUNNER_WORK="${SMOKE_RUNNER_WORK:-${PROOT_WORK:-$HOME/.proot}/smoke-runners}"
runner_start() { # NAME ROOTFS SCRIPT
  local d="$SMOKE_RUNNER_WORK/$1"
  rm -rf "$d"; mkdir -p "$d/work"
  ( setsid bash -c '
      d=$1; rootfs=$2; script=$3; here=$4; hosts=$5
      "$here/run.sh" "$rootfs" -u 10002:10002 -b "$d/work:/work" ${hosts:+-b "$hosts:/etc/hosts"} -- sh -c "$script" > "$d/log" 2>&1
      echo $? > "$d/exit"' _ "$d" "$2" "$3" "$here" "${SMOKE_RUNNER_HOSTS:-}" > /dev/null 2>&1 < /dev/null &
    echo $! > "$d/pid" )
}
# Both succeed with nothing to say while the runner is starting or still
# running: smoke.sh runs under set -e, and `docker inspect` never fails
# there either. The first run through this driver died silently at the
# removal step for exactly that reason.
runner_logs() { cat "$SMOKE_RUNNER_WORK/$1/log" 2>/dev/null || true; }
runner_exit() { cat "$SMOKE_RUNNER_WORK/$1/exit" 2>/dev/null || true; }
runner_rm() {
  local d="$SMOKE_RUNNER_WORK/$1"
  # The group is usually gone already (the runner exited on its own);
  # under smoke.sh's set -e a failing kill would end the run right here.
  [ -f "$d/pid" ] && { kill -- -"$(cat "$d/pid")" 2>/dev/null || true; }
  rm -rf "$d"
  true
}
