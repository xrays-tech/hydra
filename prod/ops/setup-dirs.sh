#!/usr/bin/env bash
#
# Create the bind-mount directories the stack needs and fix ownership for the container users.
#
# Run this before `docker compose up` (ops/up.sh calls it). Idempotent.
#
# Ownership:
#   * postgres data  -> uid/gid 999 (the `postgres:16` image user)
#   * redis data     -> uid/gid 999 (the redis image user)
#   * app data       -> uid/gid 1000 (the non-root `hydra` user in the runtime image)
#   * greptimedb data -> left owned by the invoking operator; the image runs as root and only
#     needs a writable directory.
set -euo pipefail

PROD="$(cd "$(dirname "$0")/.." && pwd)"

mkdir -p \
    "$PROD/data/postgres" \
    "$PROD/data/redis" \
    "$PROD/data/greptimedb" \
    "$PROD/data/app" \
    "$PROD/releases"

# The chowns need root (the bind dir is owned by the operator). On the target host the deploy
# user is root; locally this will prompt for sudo.
if [ "$(id -u)" -eq 0 ]; then
    SUDO=()
else
    SUDO=(sudo)
fi

echo ">> setting postgres/redis data ownership to 999:999"
"${SUDO[@]}" chown -R 999:999 "$PROD/data/postgres" "$PROD/data/redis"
"${SUDO[@]}" chmod 700 "$PROD/data/postgres" "$PROD/data/redis"

# The app image runs as uid/gid 10001 (`hydra`, a dedicated high uid because ubuntu:24.04 already
# owns 1000); give it a writable /app/data.
"${SUDO[@]}" chown -R 10001:10001 "$PROD/data/app"
"${SUDO[@]}" chmod 700 "$PROD/data/app"

chmod 755 "$PROD/data/greptimedb"

# Secrets are operator-only if they exist.
for f in "$PROD/.env" "$PROD/deploy.env"; do
    [ -f "$f" ] && chmod 600 "$f"
done

echo ">> dirs ready under $PROD/data and $PROD/releases"
