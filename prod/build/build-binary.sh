#!/usr/bin/env bash
#
# Build the Hydra release binary and stage it for the edge instance.
#
# DEFAULT: build on the HOST (native `cargo build --release`), then copy target/release/hydra to
# prod/releases/hydra. This is the fast path and it is sufficient because the binary requires
# at most GLIBC_2.34 while the runtime image (ubuntu:24.04) ships glibc 2.39 — a host/CI-built
# binary runs directly in the image, no in-Ubuntu rebuild needed. (Measured 2026-11-11; the earlier
# exit 139/126 was Fedora SELinux bind-mount labelling, fixed by the `:z` mount options in
# docker-compose.yml, not a glibc mismatch.)
#
# --container: build inside prod/build/Dockerfile.builder instead (ubuntu:24.04 + Rust 1.98.0 +
# clang/llvm; the artifact is extracted with `docker cp`). Use this when the host toolchain is
# missing or you want the build fully pinned to the container recipe. The binary is NEVER executed
# here.
#
# Usage (from anywhere):
#   prod/build/build-binary.sh              # host build (default)
#   prod/build/build-binary.sh --container  # container build
#
# The script prints the highest GLIBC version the staged binary requires as a compatibility check.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
PROD="$ROOT/prod"
BUILD_MODE="host"

case "${1:-}" in
    "") ;;
    --container) BUILD_MODE="container" ;;
    -h|--help)
        sed -n '2,30p' "$0" | sed 's/^# \{0,1\}//'
        exit 0
        ;;
    *)
        echo "!! unknown argument: ${1} (expected --container or --help)" >&2
        exit 2
        ;;
esac

# The exact feature set the production compose manifest asks the binary for. GreptimeDB is the
# usage sink; exactly one relational backend is compiled in (db/backend.rs refuses neither/both).
FEATURES="server,cluster-redis,usage-greptimedb,db-postgres"

report_glibc() {
    local bin="$1"
    if [ ! -f "$bin" ]; then
        echo "!! binary not found: $bin" >&2
        return 1
    fi
    if command -v objdump >/dev/null 2>&1; then
        local max
        max="$(objdump -T "$bin" | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1 || true)"
        echo ">> max GLIBC requirement: ${max:-none found (static?) }"
        echo "   runtime image ubuntu:24.04 ships glibc 2.39; a requirement <= GLIBC_2.34 is compatible."
    else
        echo ">> objdump not available; skipping the GLIBC requirement check"
    fi
}

cd "$ROOT"
mkdir -p "$PROD/releases"

if [ "$BUILD_MODE" = "host" ]; then
    echo ">> [host] building hydra (release) with features: $FEATURES"
    cargo build --release -p hydra-server --features "$FEATURES" --bin hydra

    src="$ROOT/target/release/hydra"
    if [ ! -f "$src" ]; then
        echo "!! expected binary not found at $src" >&2
        exit 1
    fi

    report_glibc "$src"
else
    echo ">> [container] building builder image (hydra-builder) from prod/build/Dockerfile.builder ..."
    docker build -f prod/build/Dockerfile.builder -t hydra-builder .

    echo ">> [container] creating a stopped builder container to extract the artifact ..."
    cid="$(docker create hydra-builder)"
    tmpdir="$(mktemp -d)"
    trap 'docker rm -f "$cid" >/dev/null 2>&1 || true; rm -rf "$tmpdir"' EXIT

    src="$tmpdir/hydra"
    docker cp "$cid:/src/target/release/hydra" "$src"
    report_glibc "$src"
fi

echo ">> staging releases/hydra"
cp -f "$src" "$PROD/releases/hydra"
chmod +x "$PROD/releases/hydra"

echo ">> done. binary at:"
ls -l "$PROD/releases/hydra"
