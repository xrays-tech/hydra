#!/usr/bin/env bash
#
# Release-build the Hydra server binary from source and stage it into
# environment/bin/.
#
# This is the remote/CI-friendly counterpart of scripts/local_release.sh: it
# compiles FROM SOURCE on the current machine (no committed binary needed),
# then copies the result into environment/bin/ so the Docker image build has
# the executable in scope. Typical usage on a build host / CI:
#
#   ./environment/release.sh                      # postgres (default) + stage
#   DB_BACKEND=mysql ./environment/release.sh     # mysql variant + stage
#   docker build -t hydra:latest -f environment/Dockerfile .   # then package
#
# DB_BACKEND selects the compiled relational backend (default `postgres`), exactly as build.sh:
#   postgres -> `db-postgres`
#   mysql    -> `db-mysql`
# The staged binary is single-backend; build/tag the image per backend (see build.sh).
#
# NOTE: environment/Dockerfile COPYs bin/hydra (repo-root level), so when
# packaging the image from this repo root the staged binary must be mirrored
# to ./bin/hydra first (scripts/local_release.sh does that automatically).
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

# Literal lists (read by scripts/check_compose_env.cjs): exactly one backend feature per build.
if [[ "$DB_BACKEND" == "postgres" ]]; then
    FEATURES="server,cluster-redis,usage-clickhouse,usage-greptimedb,db-postgres"
else
    FEATURES="server,cluster-redis,usage-clickhouse,usage-greptimedb,db-mysql"
fi

BIN_NAME="hydra"
PKG_DIR="crates/hydra-server"
STAGE_DIR="$ROOT/environment/bin"

echo ">> [1/2] building $BIN_NAME (release, $DB_BACKEND, full feature set) from $PKG_DIR..."
# Run from the package dir so `--features … --bin hydra` resolves for the [[bin]]
# hydra target (required-features = ["server"]), matching build.sh — the same
# feature set, so this binary can serve a cluster deployment too. `usage-clickhouse`
# and `usage-greptimedb` are the delivered usage sinks: the retired `sqlite` sink was deleted
# with ADR-0002 (2026-10-07) and `HYDRA_USAGE_SINK=sqlite` is now REFUSED BY NAME at
# startup, so the runtime choice is `clickhouse`, `greptimedb`, or `none`. Cluster mode is
# `HYDRA_REDIS_URL` presence; there is no raft/Arachne control plane. Exactly one of
# `db-postgres` / `db-mysql` is compiled in (db/backend.rs refuses neither/both).
( cd "$PKG_DIR" && cargo build --release --features "$FEATURES" --bin "$BIN_NAME" )

SRC="$ROOT/target/release/$BIN_NAME"
if [[ ! -f "$SRC" ]]; then
    echo "!! expected binary not found at $SRC" >&2
    exit 1
fi

echo ">> [2/2] staging binary -> $STAGE_DIR/$BIN_NAME"
mkdir -p "$STAGE_DIR"
cp -f "$SRC" "$STAGE_DIR/$BIN_NAME"
chmod +x "$STAGE_DIR/$BIN_NAME"
file "$STAGE_DIR/$BIN_NAME" | cut -d, -f1-2

echo ">> done. binary at $STAGE_DIR/$BIN_NAME ($DB_BACKEND backend)"
