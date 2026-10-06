#!/usr/bin/env bash
# Load tests/e2e/seed-data.json into a running hydra instance via the admin
# REST API. Idempotent: deletes the seed rows first so re-runs are clean.
# Used by the Playwright suite (and for manual UI smoke).
set -euo pipefail

ADMIN="${HYDRA_ADMIN_ADDR:-127.0.0.1:8081}"
TOKEN="${HYDRA_ADMIN_TOKEN:?HYDRA_ADMIN_TOKEN must be set}"
# Resolve the default data file NEXT TO THIS SCRIPT, not against the caller's
# CWD: the suite and the CI job run it from the repo root, but a gate that has
# `cd`-ed into a scratch directory (to keep its logs and SQLite file together)
# used to fail with "seed file not found" for no visible reason.
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SEED="${1:-$HERE/seed-data.json}"

[ -f "$SEED" ] || { echo "seed file not found: $SEED" >&2; exit 1; }
command -v jq >/dev/null 2>&1 || { echo "jq is required to read $SEED" >&2; exit 1; }

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

post() {  # post <resource> <json> — fails the script on any non-2xx / missing entry
  local resource="$1" body="$2"
  if [ "$body" = "null" ] || [ -z "$body" ]; then
    echo "  !! $resource: seed JSON has no such entry (jq produced '$body')" >&2
    return 1
  fi
  # Capture body AND status: `curl` without -f exits 0 on HTTP 4xx/5xx, and the
  # old version piped that into /dev/null and printed "+ $resource" regardless —
  # so a seed that inserted NOTHING still reported success (and the CI "Seed
  # fixtures" step passed).
  local resp code out
  resp="$(curl -sS -w $'\n%{http_code}' -X POST "http://${ADMIN}/api/v1/$resource" \
    -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" -d "$body")" || {
      echo "  !! $resource: curl failed (is hydra up at ${ADMIN}?)" >&2; return 1; }
  code="${resp##*$'\n'}"; out="${resp%$'\n'*}"
  if [[ "$code" != 2* ]]; then
    echo "  !! $resource: HTTP $code — ${out:0:300}" >&2
    return 1
  fi
  echo "  + $resource (HTTP $code)"
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

# Read back what we just wrote. Without this, a partial seed is only visible as a
# confusing downstream UI failure (the UI specs assert on rows that may not exist).
echo "==> verifying seeded rows are readable"
missing=0
for pair in "providers/p-seed" "provider-keys/k-seed" "tenants/t-seed" "tenant-providers/tp-seed" "tenant-models/tm-seed" "limit-roles/r-seed" "provider-key-bindings/b-seed"; do
  code="$(curl -sS -o /dev/null -w '%{http_code}' -H "Authorization: Bearer $TOKEN" "http://${ADMIN}/api/v1/$pair")"
  if [[ "$code" != 2* ]]; then
    echo "  !! GET /api/v1/$pair -> HTTP $code (the seed did not stick)" >&2
    missing=1
  fi
done
if [ "$missing" != 0 ]; then
  echo "==> seed FAILED: some rows are not readable after insert" >&2
  exit 1
fi
echo "  + all seeded rows verified"

echo "==> seed complete. Health:"
curl_admin GET /api/v1/health
echo
