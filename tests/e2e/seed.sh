#!/usr/bin/env bash
# Load tests/e2e/seed-data.json into a running hydra instance via the admin
# REST API. Idempotent: deletes the seed rows first so re-runs are clean.
# Used by the Playwright suite (and for manual UI smoke).
set -euo pipefail

ADMIN="${HYDRA_ADMIN_ADDR:-127.0.0.1:8081}"
TOKEN="${HYDRA_ADMIN_TOKEN:?HYDRA_ADMIN_TOKEN must be set}"
SEED="${1:-tests/e2e/seed-data.json}"

[ -f "$SEED" ] || { echo "seed file not found: $SEED" >&2; exit 1; }

curl_admin() {  # curl_admin METHOD path [json-file-field-name]
  local method="$1" path="$2"
  curl -sS -X "$method" "http://${ADMIN}${path}" \
    -H "Authorization: Bearer ${TOKEN}" \
    -H "content-type: application/json" "${@:3}"
}

echo "==> seeding hydra at http://${ADMIN} from $SEED"

# Idempotent cleanup (ignore 404s).
for id in b-seed r-seed tm-seed tp-seed m-seed k-seed t-seed p-seed; do
  # Try the resource that owns this id; failures are expected/ignored.
  curl -sS -X DELETE "http://${ADMIN}/api/v1/limit-roles/$id"     -H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
  curl -sS -X DELETE "http://${ADMIN}/api/v1/tenant-models/$id"   -H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
  curl -sS -X DELETE "http://${ADMIN}/api/v1/tenant-providers/$id"-H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
  curl -sS -X DELETE "http://${ADMIN}/api/v1/provider-models/$id" -H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
  curl -sS -X DELETE "http://${ADMIN}/api/v1/provider-keys/$id"   -H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
  curl -sS -X DELETE "http://${ADMIN}/api/v1/tenants/$id"         -H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
  curl -sS -X DELETE "http://${ADMIN}/api/v1/providers/$id"       -H "Authorization: Bearer $TOKEN" >/dev/null 2>&1 || true
done

post() {  # post <resource> <json>
  curl -sS -X POST "http://${ADMIN}/api/v1/$1" \
    -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" -d "$2" >/dev/null
  echo "  + $1"
}

# Provider (+ its model + key) first because of FK constraints.
post providers          "$(jq -c '.providers[0]'          "$SEED")"
post provider-models    "$(jq -c '.provider_models[0]'    "$SEED")"
post provider-keys      "$(jq -c '.provider_keys[0]'      "$SEED")"
post tenants            "$(jq -c '.tenants[0]'            "$SEED")"
post tenant-providers   "$(jq -c '.tenant_providers[0]'   "$SEED")"
post tenant-models      "$(jq -c '.tenant_models[0]'      "$SEED")"
post limit-roles        "$(jq -c '.limit_roles[0]'        "$SEED")"
post provider-key-bindings "$(jq -c '.provider_key_bindings[0]' "$SEED")"

echo "==> seed complete. Health:"
curl_admin GET /api/v1/health
echo
