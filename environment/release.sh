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
#   ./environment/release.sh                      # build + stage
#   docker build -t hydra:latest -f environment/Dockerfile .   # then package
#
# NOTE: environment/Dockerfile COPYs bin/hydra (repo-root level), so when
# packaging the image from this repo root the staged binary must be mirrored
# to ./bin/hydra first (scripts/local_release.sh does that automatically).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

BIN_NAME="hydra"
PKG_DIR="crates/hydra-server"
STAGE_DIR="$ROOT/environment/bin"

echo ">> [1/2] building $BIN_NAME (release, full feature set) from $PKG_DIR..."
# Run from the package dir so `--features … --bin hydra` resolves for the [[bin]]
# hydra target (required-features = ["server"]), matching build.sh — the same
# feature set, so this binary can serve a cluster deployment too. `usage-clickhouse`
# is the ONLY usage backend that needs a feature: the retired `sqlite` sink was deleted
# with ADR-0002 (2026-10-07) and `HYDRA_USAGE_SINK=sqlite` is now REFUSED BY NAME at
# startup, so the runtime choice is `clickhouse` or `none` — this comment claimed
# "BOTH sinks (sqlite + clickhouse)" for a while after the sink itself was gone.
# `arachne` is required for cluster mode: without it the binary REFUSES to boot when
# HYDRA_CLUSTER_PEERS is set, so a recipe that omits it cannot run the shipped compose
# manifests.
( cd "$PKG_DIR" && cargo build --release --features server,cluster-redis,arachne,usage-clickhouse --bin "$BIN_NAME" )

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

echo ">> done. binary at $STAGE_DIR/$BIN_NAME"
