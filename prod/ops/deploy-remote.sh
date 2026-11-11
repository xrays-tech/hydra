#!/usr/bin/env bash
#
# Deploy the prod/ tree to the remote host named in prod/deploy.env, load the app image there, and
# start the stack.
#
#   1. source prod/deploy.env (SERVER_HOST/SERVER_USER/SERVER_PASSWORD/REMOTE_DIR)
#   2. build the app image locally (prod/app — no Rust build; the release binary is bind-mounted)
#   3. `docker save | ssh docker load` so the remote does not need to build anything
#   4. rsync prod/ (INCLUDING releases/hydra, EXCLUDING data/, deploy.env and *.tar)
#   5. remotely: ops/setup-dirs.sh && docker compose up -d
#
# The password is never echoed and never printed in a command line this script shows.
set -euo pipefail

PROD="$(cd "$(dirname "$0")/.." && pwd)"
DEPLOY_FILE="$PROD/deploy.env"

if [ ! -f "$DEPLOY_FILE" ]; then
    echo "!! $DEPLOY_FILE missing — copy prod/deploy.env.example and fill it" >&2
    exit 1
fi

set -a
# shellcheck disable=SC1090
. "$DEPLOY_FILE"
set +a

: "${SERVER_HOST:?SERVER_HOST must be set in deploy.env}"
: "${SERVER_USER:?SERVER_USER must be set in deploy.env}"
: "${SERVER_PASSWORD:?SERVER_PASSWORD must be set in deploy.env}"
: "${REMOTE_DIR:?REMOTE_DIR must be set in deploy.env}"

for bin in docker rsync sshpass; do
    command -v "$bin" >/dev/null 2>&1 || { echo "!! required tool not found: $bin" >&2; exit 1; }
done

SSH_OPTS=(-o StrictHostKeyChecking=accept-new)
SSH=(sshpass -p "$SERVER_PASSWORD" ssh "${SSH_OPTS[@]}" "$SERVER_USER@$SERVER_HOST")

echo ">> ensuring remote directory $REMOTE_DIR exists"
"${SSH[@]}" "mkdir -p '$REMOTE_DIR'"

echo ">> building app image locally (hydra-prod:latest)"
docker compose -f "$PROD/docker-compose.yml" --env-file "$PROD/.env" build hydra

echo ">> transferring app image to $SERVER_HOST"
docker save hydra-prod:latest | gzip -1 | "${SSH[@]}" "gunzip | docker load"

echo ">> syncing prod/ tree to $SERVER_HOST:$REMOTE_DIR/prod"
# The release binary is needed on the remote (bind-mounted); data/ is not.
rsync -az --delete \
    --exclude 'data/' \
    --exclude 'deploy.env' \
    --exclude '*.tar' \
    -e "sshpass -p '$SERVER_PASSWORD' ssh ${SSH_OPTS[*]}" \
    "$PROD/" "$SERVER_USER@$SERVER_HOST:$REMOTE_DIR/prod/"

echo ">> starting the stack on the remote host"
"${SSH[@]}" "cd '$REMOTE_DIR/prod' && ./ops/setup-dirs.sh && docker compose -f docker-compose.yml --env-file .env up -d"

echo ">> done."
