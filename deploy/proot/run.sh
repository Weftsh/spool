#!/usr/bin/env bash
# Run a command inside an image root filesystem under PRoot.
#
#   run.sh ROOTFS [-u UID:GID] [-b HOST:GUEST]... [-e K=V]... [--env-file F] [-w DIR] [--] [CMD...]
#
# --env-file reads K=V lines from F and deletes F once read: a job token
# has to reach the runner without ever being in a command line, where
# `ps` shows it, which is the same rule fake-ecs's docker branch keeps
# with docker's --env-file.
#
# PRoot is a user-space chroot plus bind mounts, implemented with ptrace:
# no namespaces, no capabilities, no daemon. It is what lets a job on a
# Fargate task — which refuses privileged mode, every capability but
# CAP_SYS_PTRACE, and user-namespace creation — start a Postgres, a
# MinIO, or the production image itself, from the bytes a registry
# served. See deploy/proot/README.md for what that does and does not
# prove.
#
# The image config written by pull-image.py (ROOTFS/.image.json) supplies
# Env, WorkingDir, User and Entrypoint+Cmd unless overridden: PRoot,
# unlike a runtime, applies none of that by itself, and the first
# Postgres run here died on a missing PGDATA for exactly that reason.
#
# Always bound: /proc, /dev, /sys, and the host's /etc/resolv.conf,
# because PRoot injects no resolver and a Rust image's own is empty.
set -euo pipefail
ROOTFS=${1:?rootfs}; shift
PROOT=${PROOT:-$(dirname "$0")/bin/proot}
CFG="$ROOTFS/.image.json"
uid=""; binds=(); envs=(); workdir=""
while [ $# -gt 0 ]; do
  case "$1" in
    -u) uid=$2; shift 2 ;;
    -b) binds+=("-b" "$2"); shift 2 ;;
    -e) envs+=("$2"); shift 2 ;;
    --env-file)
      while IFS= read -r line || [ -n "$line" ]; do [ -n "$line" ] && envs+=("$line"); done < "$2"
      rm -f "$2"; shift 2 ;;
    -w) workdir=$2; shift 2 ;;
    --) shift; break ;;
    *) break ;;
  esac
done
if [ -f "$CFG" ]; then
  mapfile -t cfg_env < <(python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["config"]; print("\n".join(c.get("Env") or []))' "$CFG")
  [ -n "$workdir" ] || workdir=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["config"].get("WorkingDir") or "/")' "$CFG")
  [ -n "$uid" ] || uid=$(python3 -c 'import json,sys; u=json.load(open(sys.argv[1]))["config"].get("User") or ""; print(u)' "$CFG")
  if [ $# -eq 0 ]; then
    mapfile -t default_cmd < <(python3 -c 'import json,sys; c=json.load(open(sys.argv[1]))["config"]; print("\n".join((c.get("Entrypoint") or [])+(c.get("Cmd") or [])))' "$CFG")
    set -- "${default_cmd[@]}"
  fi
else
  cfg_env=()
fi
# A numeric uid from the image, or a name resolved in the image's passwd.
if [ -n "$uid" ] && ! [[ "$uid" =~ ^[0-9]+(:[0-9]+)?$ ]]; then
  name=${uid%%:*}
  uid=$(awk -F: -v n="$name" '$1==n{print $3":"$4}' "$ROOTFS/etc/passwd")
fi
[ -n "$uid" ] && [[ "$uid" != *:* ]] && uid="$uid:$uid"
# HOME, as a runtime would set it: the passwd entry for the uid the
# process runs as, else /root. `env -i` below starts from nothing, and
# MinIO refuses to start without a HOME to keep its config under.
home=$(awk -F: -v u="${uid%%:*}" '$3==u{print $6; exit}' "$ROOTFS/etc/passwd" 2>/dev/null || true)
cfg_env+=("HOME=${home:-/root}")
exec "$PROOT" ${uid:+-i "$uid"} -r "$ROOTFS" -b /proc -b /dev -b /sys -b /etc/resolv.conf "${binds[@]}" -w "${workdir:-/}" \
  /usr/bin/env -i "${cfg_env[@]}" "${envs[@]}" "$@"
