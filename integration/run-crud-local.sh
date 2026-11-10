#!/usr/bin/env bash
# Run the black-box CRUD integration suite (`test_crud.py`) against a DISPOSABLE
# hydra instance.
#
# WHY: `integration/test_crud.py` is a 25 KB, stdlib-only suite covering the full
# CRUD lifecycle of all 7 config entities plus the edge cases, and NOTHING ran it
# — not CI, not the local gate, not `run.sh` (which needs Docker + a built image
# and hardcodes 8080/8081, so it cannot run next to a dev stack). A suite nobody
# runs is a suite nobody knows the state of; when first executed it passed
# 116/116, which is exactly the point: that should be a fact CI checks, not a
# thing we find out years later.
#
# Same shape as `scripts/e2e-local.sh`: a disposable real PostgreSQL/MySQL schema
# (HYDRA_TEST_DATABASE_URL), scratch ports, everything torn down on exit.
#
# Usage:
#   integration/run-crud-local.sh
#   HYDRA_IT_ADMIN_PORT=18081 HYDRA_IT_DATA_PORT=18080 integration/run-crud-local.sh
#   KEEP=1 integration/run-crud-local.sh        # keep the scratch dir + log
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SCRATCH=".acceptance/it-crud"
ADMIN_PORT="${HYDRA_IT_ADMIN_PORT:-18081}"
DATA_PORT="${HYDRA_IT_DATA_PORT:-18080}"
TOKEN="${HYDRA_IT_TOKEN:-hydra-it-token-2026}"
BASE_URL="http://127.0.0.1:${ADMIN_PORT}"

# The database backend is compile-time selected. Derive it from the test URL so this script
# works for both PG and MySQL; a build with neither backend is a deliberate compile error.
case "${HYDRA_TEST_DATABASE_URL:-}" in
  postgres://*|postgresql://*) DB_FEATURE=db-postgres ;;
  mysql://*|mariadb://*)        DB_FEATURE=db-mysql ;;
  *)
    echo "it-crud: set HYDRA_TEST_DATABASE_URL to a real PostgreSQL or MySQL base URL" >&2
    echo "         e.g. postgres://hydra:hydra@127.0.0.1:55432/hydra" >&2
    exit 2 ;;
esac

# The server fail-closes on a token shorter than 16 chars (main.rs).
if [ "${#TOKEN}" -lt 16 ]; then
  echo "it-crud: HYDRA_IT_TOKEN must be >= 16 chars (the server refuses to start)" >&2
  exit 1
fi

needs_build=0
[ -x target/debug/hydra ] || needs_build=1
if [ "$needs_build" = 0 ]; then
  newer=$(find crates admin-ui Cargo.toml \
      \( -name '*.rs' -o -name '*.toml' -o -name '*.sql' -o -name '*.js' -o -name '*.html' -o -name '*.css' \) \
      -newer target/debug/hydra -print -quit 2>/dev/null)
  [ -n "$newer" ] && needs_build=1
fi
if [ "$needs_build" = 1 ]; then
  echo "==> building target/debug/hydra"
  cargo build -p hydra-server --features "server,$DB_FEATURE" --bin hydra >/dev/null
fi

rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"

# A disposable real schema/database owned by the integration helper. The URL is
# printed as `name<TAB>url`; the name is what cleanup drops.
IFS=$'\t' read -r DB_NAME DB_URL < <(python3 integration/_shared_db.py create it-crud)
[ -n "${DB_URL:-}" ] || { echo "it-crud: could not provision a disposable database" >&2; exit 2; }

# `HYDRA_USAGE_SINK=none` (required since ADR-0002 D-1: there is NO default) is hard-set rather
# than overridable, and it sits INSIDE the assignment chain — a `#` comment between two
# backslash-continued assignments ENDS the chain, which silently dropped every assignment above
# and started this node on the default port. This drill asserts nothing about metering, so `none`
# is also the honest value: it cannot make the node demand a cargo feature this build lacks.
HYDRA_ADMIN_TOKEN="$TOKEN" \
HYDRA_ADMIN_ADDR="127.0.0.1:${ADMIN_PORT}" \
HYDRA_LISTEN="127.0.0.1:${DATA_PORT}" \
HYDRA_DB_URL="$DB_URL" \
HYDRA_USAGE_SINK=none \
HYDRA_ENCRYPTION_KEY="${HYDRA_ENCRYPTION_KEY:-MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=}" \
RUST_LOG="${RUST_LOG:-warn}" \
  ./target/debug/hydra > "$SCRATCH/hydra.log" 2>&1 &
PID=$!
cleanup() {
  kill "$PID" 2>/dev/null || true
  wait "$PID" 2>/dev/null || true
  python3 integration/_shared_db.py drop "$DB_NAME" >/dev/null 2>&1 || true
  [ "${KEEP:-0}" = 1 ] || rm -rf "$SCRATCH"
}
trap cleanup EXIT INT TERM

ready=no
for _ in $(seq 1 60); do
  kill -0 "$PID" 2>/dev/null || { tail -20 "$SCRATCH/hydra.log"; echo "it-crud: hydra exited" >&2; exit 1; }
  if curl -fsS -H "Authorization: Bearer ${TOKEN}" "${BASE_URL}/api/v1/health" >/dev/null 2>&1; then ready=yes; break; fi
  sleep 1
done
[ "$ready" = yes ] || { tail -20 "$SCRATCH/hydra.log"; echo "it-crud: hydra not ready" >&2; exit 1; }

echo "==> python3 integration/test_crud.py against ${BASE_URL}"
HYDRA_BASE_URL="$BASE_URL" HYDRA_ADMIN_TOKEN="$TOKEN" python3 integration/test_crud.py
crud_rc=$?
[ "$crud_rc" = 0 ] || exit "$crud_rc"

# The SQLite `VACUUM INTO` backup assertion that used to live here was retired with SQLite.
# The backup/restore runbook is rewritten against PostgreSQL/MySQL in the Phase 4b docs pass.

# ── the ERROR CONTRACT, on the same disposable instance ────────────────────
# `admin-ui/app.js` grew HTML/empty-body handling for error responses, which means
# the server's error shape was never a tested contract. This probes every
# documented endpoint with a malformed body, an undocumented method, no token and
# an exhausted auth budget, and cross-checks the observed codes against the ones
# `api-docs.js` documents for that endpoint.
echo "==> python3 integration/check_error_contract.py against ${BASE_URL}"
err_out="$(HYDRA_BASE_URL="$BASE_URL" HYDRA_ADMIN_TOKEN="$TOKEN" python3 integration/check_error_contract.py 2>&1)"
err_rc=$?
echo "$err_out"
[ "$err_rc" = 0 ] || { echo "it-crud: the error contract has violations (see above)" >&2; exit 1; }

# ── the DOCUMENTED deployment seeding step, on the same disposable instance ──
# `environment/init.py` is named by deployment.md and all three compose files as the
# way to seed a fresh deployment, and NOTHING ran it (this script is the only place
# it can be exercised without Docker).
#
# The config is built from the SHIPPED TEMPLATE (`environment/config.example.json`)
# rather than a bespoke fixture: the template is what the docs tell an operator to
# copy, so a renamed/added field in it would break the documented first run — and
# nothing else would notice. Only the two values that cannot live in a template
# (this instance's URL and token) are injected.
INIT_CFG="$SCRATCH/init-config.json"
HYDRA_IT_BASE_URL="$BASE_URL" HYDRA_IT_TOKEN="$TOKEN" python3 - "$INIT_CFG" <<'PYCFG'
import json, os, sys
cfg = json.load(open("environment/config.example.json"))
cfg["admin_url"] = os.environ["HYDRA_IT_BASE_URL"]
cfg["admin_token"] = os.environ["HYDRA_IT_TOKEN"]
json.dump(cfg, open(sys.argv[1], "w"), indent=2)
print(f"[it-crud] init config from the shipped template: "
      f"{len(cfg.get('providers', []))} provider(s), tenant={cfg.get('tenant', {}).get('id')}")
PYCFG

echo "==> python3 environment/init.py (documented seeding step, shipped template)"
init_out="$(HYDRA_IT_BASE_URL="$BASE_URL" HYDRA_IT_TOKEN="$TOKEN" python3 environment/init.py "$INIT_CFG" 2>&1)"
init_rc=$?
echo "$init_out" | tail -3
[ "$init_rc" = 0 ] || { echo "it-crud: init.py exited $init_rc" >&2; exit 1; }
# "N ok, M warnings": ANY warning means a POST was rejected (duplicates are not
# possible here — the DB is fresh), i.e. the documented step did not do its job.
echo "$init_out" | grep -qE "done: [1-9][0-9]* ok, 0 warnings" \
  || { echo "it-crud: init.py reported warnings (see above)" >&2; exit 1; }
