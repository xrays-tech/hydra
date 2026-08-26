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
# The image bundles ALL optional features (server + cluster-redis + usage-clickhouse) so a
# SINGLE `hydra:latest` image serves both single-node (HYDRA_ROLE unset → zero external
# deps, unchanged behavior) and cluster mode (HYDRA_ROLE=leader|edge + Redis).
#
# Usage:
#   ./environment/build.sh              # full: cross-compile + stage + docker build
#   ./environment/build.sh --no-build   # cross-compile + stage only (skip docker build)
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

TARGET="x86_64-unknown-linux-gnu"
GLOBAL_TARGET_BIN="$HOME/.cargo/global-target/$TARGET/release/hydra"
DO_DOCKER=1
[[ "${1:-}" == "--no-build" ]] && DO_DOCKER=0

echo ">> [1/3] cross-compiling hydra for $TARGET (release) via rust_build_linux..."
# Run from the package dir so --features server,cluster-redis,usage-clickhouse --bin hydra
# resolves correctly for the cross target. usage-clickhouse compiles BOTH sinks (sqlite +
# clickhouse) into one binary; cluster-redis enables the cluster mode (Redis backbone).
# HYDRA_ROLE / HYDRA_USAGE_SINK select behavior at runtime.
( cd crates/hydra-server && rust_build_linux --features server,cluster-redis,usage-clickhouse --bin hydra )

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
    echo ">> [3/3] building docker image hydra:latest..."
    docker build -t hydra:latest -f environment/Dockerfile .
    echo ">> done."
    echo "   run: docker run --rm -p 443:443 -p 8081:8081 -v \"$PWD/data\":/app/data -e HYDRA_ADMIN_TOKEN=<token> hydra:latest"
else
    echo ">> staged bin/hydra (docker build skipped)."
fi
