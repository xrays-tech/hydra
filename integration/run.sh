#!/usr/bin/env bash
#
# Hydra admin API — integration test runner.
#
# Spins up a throwaway Hydra container (image hydra:latest), polls /health,
# runs test_crud.py against the admin REST API, then tears everything down on
# exit (container + volume) so each run is hermetic and reproducible.
#
# Prereqs:
#   - Docker daemon running
#   - python3 on PATH (for both test_crud.py and the health poll)
#   - Image hydra:latest built — run `./environment/build.sh` first
#
# Usage:
#   ./integration/run.sh                 # default: build, run, test, teardown
#   HYDRA_IT_PORT=8090 ./integration/run.sh
#   HYDRA_IT_KEEP=1 ./integration/run.sh # keep container + volume after run (debug)
#
# Exit code: the test script's exit code (0 = all assertions passed).
set -uo pipefail

# ---------------------------------------------------------------------------
# Configuration (env-overridable)
# ---------------------------------------------------------------------------
IMAGE="${HYDRA_IT_IMAGE:-hydra:latest}"
CONTAINER="${HYDRA_IT_CONTAINER:-hydra-it}"
VOLUME="${HYDRA_IT_VOLUME:-hydra-it-data}"
PORT="${HYDRA_IT_PORT:-8081}"
# >= 16 chars: the server fail-closes on a shorter admin token
# (AdminService::MIN_ADMIN_TOKEN_LEN, main.rs "HYDRA_ADMIN_TOKEN is too short").
# The previous default here was 14 chars ("hydra-it-token"), so this gate could
# never boot — it reported "boot failed" for a configuration error.
TOKEN="${HYDRA_IT_TOKEN:-hydra-it-token-2026}"
if [[ ${#TOKEN} -lt 16 ]]; then
    echo "[config] !! HYDRA_IT_TOKEN is ${#TOKEN} chars; the server requires >= 16" >&2
    echo "[config]    (main.rs refuses to start: 'HYDRA_ADMIN_TOKEN is too short')." >&2
    exit 1
fi
KEEP="${HYDRA_IT_KEEP:-}"   # non-empty => skip teardown
HEALTH_TIMEOUT="${HYDRA_IT_HEALTH_TIMEOUT:-30}"   # seconds to wait for boot

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
TEST_PY="${SCRIPT_DIR}/test_crud.py"

# The DB URL uses the cross-compilation-resolved main.rs env name (HYDRA_DB_URL),
# NOT the HYDRA_DATABASE_URL the Dockerfile sets — main.rs reads HYDRA_DB_URL.
# (See crates/hydra-server/src/main.rs:97.)
#
# HYDRA_ADMIN_ADDR=0.0.0.0:8081 overrides the localhost-only DEFAULT_ADMIN_LISTEN
# so the port is reachable from the host. (main.rs:50,242.)
DB_URL="sqlite:/app/data/hydra.db?mode=rwc"

# Provider-key encryption master key. The container fail-closes without it
# (provider api-keys are AES-256-GCM encrypted at rest). The integration DB and
# its keys are throwaway (re-entered by test_crud.py each run, volume torn down
# on exit), so a fresh random key per run is correct and hermetic.
if command -v openssl >/dev/null 2>&1; then
    ENC_KEY="$(openssl rand 32 | base64)"
else
    ENC_KEY="$(python3 -c 'import os,base64; print(base64.b64encode(os.urandom(32)).decode())')"
fi
export HYDRA_ENCRYPTION_KEY="$ENC_KEY"

# ---------------------------------------------------------------------------
# Teardown (always runs, even on error / Ctrl-C)
# ---------------------------------------------------------------------------
cleanup() {
    local keep="${KEEP}"
    echo ""
    echo "[teardown] cleaning up..."
    if [[ -n "$keep" ]]; then
        echo "[teardown] HYDRA_IT_KEEP set — leaving container '$CONTAINER' and volume '$VOLUME' running."
        return 0
    fi
    docker stop "$CONTAINER" >/dev/null 2>&1 || true
    docker rm "$CONTAINER" >/dev/null 2>&1 || true
    docker volume rm "$VOLUME" >/dev/null 2>&1 || true
    echo "[teardown] removed container '$CONTAINER' and volume '$VOLUME'."
}
trap cleanup EXIT

# ---------------------------------------------------------------------------
# Phase: image check
# ---------------------------------------------------------------------------
echo "[image] checking for $IMAGE..."
if ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "[image] !! $IMAGE not found."
    echo "[image]    Build it first with:  ./environment/build.sh"
    exit 1
fi
echo "[image] ok."

# ---------------------------------------------------------------------------
# Phase: run container
# ---------------------------------------------------------------------------
echo "[run] starting container '$CONTAINER' (port ${PORT}->8081)..."
# Idempotent: tear down any leftover from a previous interrupted run first.
docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
docker volume rm "$VOLUME" >/dev/null 2>&1 || true

if ! docker run -d \
        --name "$CONTAINER" \
        -p "${PORT}:8081" \
        -v "${VOLUME}:/app/data" \
        -e "HYDRA_DB_URL=${DB_URL}" \
        -e "HYDRA_ADMIN_ADDR=0.0.0.0:8081" \
        -e "HYDRA_ADMIN_TOKEN=${TOKEN}" \
        -e "HYDRA_ENCRYPTION_KEY=${ENC_KEY}" \
        "$IMAGE" >/dev/null; then
    echo "[run] !! docker run failed."
    exit 1
fi
echo "[run] container started (id: $(docker ps -q --filter "name=^${CONTAINER}$" | cut -c1-12))."

# ---------------------------------------------------------------------------
# Phase: health poll
# ---------------------------------------------------------------------------
wait_for_health() {
    local url="http://localhost:${PORT}/api/v1/health"
    local deadline=$(( $(date +%s) + HEALTH_TIMEOUT ))
    echo "[health] polling ${url} (up to ${HEALTH_TIMEOUT}s)..."
    while :; do
        # Fail-fast: container died during boot.
        if [[ -z "$(docker ps -q --filter "name=^${CONTAINER}$")" ]]; then
            echo "[health] !! container '$CONTAINER' is not running — boot failed."
            echo "[health] !! logs:"
            docker logs "$CONTAINER" 2>&1 | sed 's/^/    /'
            return 1
        fi
        if python3 - "$url" "$TOKEN" <<'PY' 2>/dev/null
import sys, urllib.request
url, token = sys.argv[1], sys.argv[2]
req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
try:
    with urllib.request.urlopen(req, timeout=2) as r:
        sys.exit(0 if r.status == 200 else 1)
except Exception:
    sys.exit(1)
PY
            then
            echo "[health] ok."
            return 0
        fi
        if [[ $(date +%s) -gt $deadline ]]; then
            echo "[health] !! timed out after ${HEALTH_TIMEOUT}s waiting for ${url}."
            echo "[health] !! logs:"
            docker logs "$CONTAINER" 2>&1 | sed 's/^/    /'
            return 1
        fi
        sleep 0.5
    done
}

if ! wait_for_health; then
    exit 1
fi

# ---------------------------------------------------------------------------
# Phase: test
# ---------------------------------------------------------------------------
echo "[test] running test_crud.py..."
echo ""
HYDRA_BASE_URL="http://localhost:${PORT}" HYDRA_ADMIN_TOKEN="${TOKEN}" \
    python3 "$TEST_PY"
rc=$?
echo ""
echo "[test] test_crud.py exited with code ${rc}."
exit "$rc"
