#!/usr/bin/env bash
#
# Build the Hydra Linux Docker image.
#
# Workflow (per project convention):
#   1. Cross-compile the linux-gnu RELEASE binary on this machine using `rust_build_linux`
#      (= `cargo zigbuild --release --target x86_64-unknown-linux-gnu`).
#   2. Copy the result from the global target dir into `bin/hydra` so the Dockerfile can COPY it.
#   3. Build the image (context = repo root).
#
# IMPORTANT: the cross-compile MUST run from the `crates/hydra-server` package directory —
# building from the workspace root does not resolve `--features server` to the `[[bin]] hydra`
# target (which has `required-features = ["server"]`) for the cross target, so the binary is
# silently skipped. This script does that for you.
#
# The image bundles the server + one relational backend + Redis data-plane + ClickHouse usage
# features so a SINGLE build serves both single-node (no `HYDRA_REDIS_URL` -> zero external deps)
# and cluster mode (`HYDRA_REDIS_URL` set -> shared Redis rate/breaker state). Config authority is
# the relational database; there is no raft control plane/leader.
#
# DB_BACKEND selects the compiled relational backend (default `postgres`):
#   postgres -> `db-postgres` -> images `hydra:postgres` and `hydra:latest`
#   mysql    -> `db-mysql`    -> image  `hydra:mysql`
# The default backend also owns the legacy `hydra:latest` tag.
#
# `scripts/check_compose_env.cjs` derives the required backend feature from the manifests so this
# cannot drift again.
#
# Usage:
#   ./environment/build.sh                       # postgres (default): cross-compile + stage + docker build
#   DB_BACKEND=mysql ./environment/build.sh      # mysql variant
#   ./environment/build.sh --no-build            # cross-compile + stage only (skip docker build)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

DB_BACKEND="${DB_BACKEND:-postgres}"
case "$DB_BACKEND" in
    postgres) : ;;
    mysql) : ;;
    *)
        echo "!! DB_BACKEND must be 'postgres' or 'mysql' (got '$DB_BACKEND')" >&2
        exit 2
        ;;
esac

# Both variants are written as LITERAL feature lists so `scripts/check_compose_env.cjs` can read
# them without evaluating shell. Exactly one backend feature is active per build.
if [[ "$DB_BACKEND" == "postgres" ]]; then
    FEATURES="server,cluster-redis,usage-clickhouse,db-postgres"
else
    FEATURES="server,cluster-redis,usage-clickhouse,db-mysql"
fi

TARGET="x86_64-unknown-linux-gnu"
GLOBAL_TARGET_BIN="$HOME/.cargo/global-target/$TARGET/release/hydra"
DO_DOCKER=1
[[ "${1:-}" == "--no-build" ]] && DO_DOCKER=0

echo ">> [1/3] cross-compiling hydra for $TARGET (release, $DB_BACKEND) via rust_build_linux..."
# Run from the package dir so `--features … --bin hydra` resolves correctly for the cross target
# (the `[[bin]] hydra` target has `required-features = ["server"]`). usage-clickhouse compiles the
# usage sink; cluster-redis is the data-plane backbone; db-postgres/db-mysql selects the relational
# backend (exactly one is required — `db/backend.rs` refuses to compile without one).
( cd crates/hydra-server && rust_build_linux --features "$FEATURES" --bin hydra )

if [[ ! -f "$GLOBAL_TARGET_BIN" ]]; then
    echo "!! expected binary not found at $GLOBAL_TARGET_BIN" >&2
    exit 1
fi

echo ">> [2/3] staging binary -> bin/hydra"
mkdir -p bin
cp -f "$GLOBAL_TARGET_BIN" bin/hydra
chmod +x bin/hydra
echo "    $(file bin/hydra | cut -d, -f1-2)"

if [[ "$DO_DOCKER" -eq 1 ]]; then
    echo ">> [3/3] building docker image hydra:$DB_BACKEND..."
    docker build -t "hydra:$DB_BACKEND" -f environment/Dockerfile .
    # `hydra:latest` follows the DEFAULT backend only; a mysql build must not silently become the
    # image a `docker run hydra:latest` picks up.
    if [[ "$DB_BACKEND" == "postgres" ]]; then
        docker tag "hydra:$DB_BACKEND" hydra:latest
        echo ">> done. images: hydra:postgres (default) + hydra:latest"
    else
        echo ">> done. image: hydra:mysql (hydra:latest left untouched)"
    fi
    echo "   run: docker run --rm -p 8080:8080 -p 8081:8081 \\"
    echo "          -e HYDRA_ADMIN_TOKEN=<token> \\"
    echo "          -e HYDRA_DB_URL='postgres://hydra:hydra@HOST:5432/hydra' \\"
    echo "          hydra:$DB_BACKEND"
else
    echo ">> staged bin/hydra (docker build skipped)."
fi
