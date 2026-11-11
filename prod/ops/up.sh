#!/usr/bin/env bash
#
# Start the production stack.
#
#   1. generate prod/.env with fresh secrets if it is absent (never overwrites an existing one)
#   2. create + own the bind-mount directories (ops/setup-dirs.sh)
#   3. `docker compose up -d --wait` and then run the health checks
#
# The app image is built from prod/app (no Rust compilation); the release binary must already have
# been staged by prod/build/build-binary.sh. This script does not build Rust.
set -euo pipefail

PROD="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="$PROD/.env"

generate_env() {
    echo ">> no $ENV_FILE — generating one with fresh secrets"
    local pg_pass redis_pass admin_token enc_key
    pg_pass="$(openssl rand -hex 24)"
    redis_pass="$(openssl rand -hex 24)"
    admin_token="$(openssl rand -hex 32)"
    enc_key="$(openssl rand -base64 32)"

    umask 077
    cat > "$ENV_FILE" <<EOF
POSTGRES_USER=hydra
POSTGRES_PASSWORD=$pg_pass
POSTGRES_DB=hydra
REDIS_PASSWORD=$redis_pass
HYDRA_DB_URL=postgres://hydra:$pg_pass@postgres:5432/hydra?sslmode=disable
HYDRA_DB_REQUIRE_TLS=0
HYDRA_REDIS_URL=redis://:$redis_pass@redis:6379
HYDRA_REDIS_REQUIRE_AUTH=1
HYDRA_ADMIN_TOKEN=$admin_token
HYDRA_ENCRYPTION_KEY=$enc_key
HYDRA_ENCRYPTION_KEY_VERSION=1
HYDRA_USAGE_SINK=greptimedb
HYDRA_GREPTIMEDB_URL=http://greptimedb:4000/hydra_usage
HYDRA_BREAKER_QUORUM=1
HYDRA_SHUTDOWN_DRAIN_SECS=20
RUST_LOG=info
ADMIN_PORT=18081
EOF
    chmod 600 "$ENV_FILE"
}

if [ ! -f "$ENV_FILE" ]; then
    generate_env
fi

"$PROD/ops/setup-dirs.sh"

if [ ! -x "$PROD/releases/hydra" ]; then
    echo "!! release binary missing — run prod/build/build-binary.sh first" >&2
    exit 1
fi

echo ">> starting the stack and waiting for healthchecks"
docker compose -f "$PROD/docker-compose.yml" --env-file "$ENV_FILE" up -d --wait --wait-timeout 240

echo ">> stack is up; running health checks"
"$PROD/ops/health.sh"
