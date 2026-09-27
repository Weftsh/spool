#!/usr/bin/env bash
# Generate the LOCAL-ONLY SSH host key the compose stack serves with.
# Idempotent; the key is gitignored and never leaves this machine. Real
# deployments get their host key from Secrets Manager (see terraform).
set -euo pipefail
cd "$(dirname "$0")"
if [ ! -f .dev-ssh-host-key ]; then
  ssh-keygen -t ed25519 -N "" -C "stratum-compose-dev" -f .dev-ssh-host-key >/dev/null
  echo "generated deploy/.dev-ssh-host-key (local compose only)"
else
  echo "deploy/.dev-ssh-host-key already exists"
fi
# The container reads the bind-mounted key as its non-root user; this key
# is a local-only throwaway, so world-readable is fine (and gitignored).
chmod 644 .dev-ssh-host-key
