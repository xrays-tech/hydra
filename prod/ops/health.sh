#!/usr/bin/env bash
#
# Health probe for the single edge instance.
#
# The admin probe is `/api/v1/health` with `Authorization: Bearer <HYDRA_ADMIN_TOKEN>` — there is
# no token-free health route (ADR-0001 retired the edge role's `/healthz`).
#
# The :443 listener is reported INFORMATIONALLY: Hydra serves per-tenant TLS/SNI there, so a
# handshake only succeeds once a tenant certificate is configured in the DB (via the admin/config
# flow). A bare TCP accept proves the listener is bound; a failed handshake with no tenant cert is
# expected on a fresh deployment, not a failure of this script.
set -euo pipefail

PROD="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="$PROD/.env"

if [ ! -f "$ENV_FILE" ]; then
    echo "!! $ENV_FILE missing — run ops/up.sh first" >&2
    exit 1
fi

read_env() {
    # Last assignment wins, matching compose's env precedence.
    grep -E "^$1=" "$ENV_FILE" | tail -n1 | cut -d= -f2-
}

TOKEN="$(read_env HYDRA_ADMIN_TOKEN)"
ADMIN_PORT="$(read_env ADMIN_PORT)"
ADMIN_PORT="${ADMIN_PORT:-18081}"

rc=0

if curl -fsS -m 5 -H "Authorization: Bearer $TOKEN" \
        "http://127.0.0.1:${ADMIN_PORT}/api/v1/health" >/dev/null 2>&1; then
    echo "OK    hydra admin health (127.0.0.1:${ADMIN_PORT})"
else
    echo "FAIL  hydra admin health (http://127.0.0.1:${ADMIN_PORT}/api/v1/health)"
    rc=1
fi

# Informational: is the TLS listener bound? The handshake itself needs a tenant cert from the DB.
if timeout 3 bash -c 'exec 3<>/dev/tcp/127.0.0.1/443' 2>/dev/null; then
    echo "OK    hydra TLS listener on :443 (TCP accepted)"
    echo "      NOTE: a full TLS handshake needs a tenant cert configured in the DB; on a fresh"
    echo "            deployment :443 accepts but every SNI handshake fails until one is provisioned."
else
    echo "WARN  no TCP listener on :443 (set HYDRA_TLS_LISTEN in env/app.env and publish 443)"
fi

exit "$rc"
