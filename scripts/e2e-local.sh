#!/usr/bin/env bash
# Run the Playwright admin-UI suite against a DISPOSABLE hydra instance.
#
# WHY THIS EXISTS
# The suite was CI-only for its whole life ("needs Chromium + a freshly built and
# seeded instance"), and that is exactly how two cases added in this session
# reached the tree RED: they built their fixture with `api('POST', …)` and omitted
# `created_at`/`updated_at`, which the handler's entity struct requires — a 400 the
# browser-free guards cannot see, because they check selectors, not payloads. The
# first time the suite was ever executed locally it failed immediately.
#
# So: no user data, no port clashes, nothing left behind.
#   * a scratch SQLite file under `.acceptance/` (gitignored) and scratch ports
#     (18080/18081 by default, override with E2E_DATA_PORT / E2E_ADMIN_PORT), so a
#     running `hydra-a/b/c` dev stack is untouched;
#   * the JS runner is installed into `.acceptance/tmp-pw` (gitignored) and reused;
#     browsers come from `~/.cache/ms-playwright`;
#   * the instance is killed on exit (trap), including on Ctrl-C.
#
# VERSION NOTE: CI pins `@playwright/test@1.55.0`, whose Chromium is build 1187. This
# checkout already carries `.acceptance/pw-browsers/chromium-1187`, so the script
# defaults to that exact pair: the local run matches CI and nothing is downloaded.
# Without that directory it falls back to 1.62.0 + the shared cache's chromium-1234.
# Override with `PW_VERSION=…` / `PLAYWRIGHT_BROWSERS_PATH=…` when needed.
#
# Usage:
#   scripts/e2e-local.sh                    # whole suite
#   scripts/e2e-local.sh -g "T2.2c|T2.2d"   # any `playwright test` argument
#   KEEP=1 scripts/e2e-local.sh             # keep the scratch dir for inspection
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SCRATCH=".acceptance/e2e-local"
RUNNER=".acceptance/tmp-pw"
# Version/browser pair, in order of preference:
#   1. the CI pin (1.55.0) with the Chromium build it wants — this checkout already
#      carries `.acceptance/pw-browsers/chromium-1187`, so a local run can match CI
#      exactly;
#   2. otherwise the release matching the shared cache `~/.cache/ms-playwright`
#      (chromium-1234 ⇒ 1.62.0).
if [ -d "$ROOT/.acceptance/pw-browsers/chromium-1187" ]; then
  PW_VERSION="${PW_VERSION:-1.55.0}"
  export PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-$ROOT/.acceptance/pw-browsers}"
  MATCHES_CI=yes
else
  PW_VERSION="${PW_VERSION:-1.62.0}"
  MATCHES_CI=no
fi
ADMIN_PORT="${E2E_ADMIN_PORT:-18081}"
DATA_PORT="${E2E_DATA_PORT:-18080}"
TOKEN="${HYDRA_ADMIN_TOKEN:-dev-admin-token-2026}"
ADMIN_URL="http://127.0.0.1:${ADMIN_PORT}"

die() { echo "e2e-local: $*" >&2; exit 1; }

command -v jq >/dev/null || die "jq is required by tests/e2e/seed.sh"
command -v curl >/dev/null || die "curl is required"

# 1. Runner (installed under the ignored scratch dir, reused afterwards).
installed_version="$("$RUNNER/node_modules/.bin/playwright" --version 2>/dev/null | awk '{print $2}' || true)"
if [ ! -x "$RUNNER/node_modules/.bin/playwright" ] || [ "$installed_version" != "$PW_VERSION" ]; then
  echo "==> installing @playwright/test@${PW_VERSION} into $RUNNER (have: ${installed_version:-none})"
  mkdir -p "$RUNNER"
  printf '{"name":"pw-scratch","private":true}\n' > "$RUNNER/package.json"
  ( cd "$RUNNER" && PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD=1 \
      npm_config_cache="$ROOT/.acceptance/tmp-npm-cache" \
      npm install --no-save --no-package-lock "@playwright/test@${PW_VERSION}" >/dev/null ) \
    || die "npm install failed (offline?)"
fi

# 2. Binary: rebuild when it is missing or older than the newest Rust source.
# The binary EMBEDS its inputs at compile time (`include_dir!` for `admin-ui/**`,
# `sqlx::migrate!` for `migrations/*.sql`), so a UI-only edit leaves a stale binary
# whose `/admin/*` serves the OLD assets — an e2e run after a UI change would then
# be testing code that is not in the tree. Watching only `crates/**/*.rs` (the first
# version of this script) had exactly that blind spot.
# `E2E_SKIP_BUILD=1` uses the existing `target/debug/hydra` as-is: no freshness probe, no rebuild.
# It exists for CALLERS THAT ALREADY BUILT THE BINARY. The local gate builds the binary with
# `cluster-redis`+`usage-clickhouse` at its start and its cluster-drill entries depend on THAT binary,
# while the build below is `--features server` — so without this hatch, running this script inside the
# gate could RELINK a feature-poor binary mid-run and leave later entries testing something that is
# not the tree. That is exactly the round-143 incident, which cost a RED gate for a reason unrelated
# to the code; the hatch is the difference between "wire the e2e suite into the gate" being safe and
# being a fresh copy of that bug.
needs_build=0
if [ "${E2E_SKIP_BUILD:-0}" = 1 ]; then
  [ -x target/debug/hydra ] || die "E2E_SKIP_BUILD=1 but target/debug/hydra does not exist"
  echo "==> E2E_SKIP_BUILD=1: running against the existing target/debug/hydra (no rebuild)"
else
  [ -x target/debug/hydra ] || needs_build=1
  if [ "$needs_build" = 0 ]; then
    newest_src=$(find crates admin-ui Cargo.toml \
        \( -name '*.rs' -o -name '*.toml' -o -name '*.sql' -o -name '*.js' -o -name '*.html' -o -name '*.css' \) \
        -newer target/debug/hydra -print -quit 2>/dev/null)
    if [ -n "$newest_src" ]; then
      needs_build=1
      echo "==> rebuilding: $newest_src is newer than target/debug/hydra (the binary embeds admin-ui/** and migrations/*.sql)"
    fi
  fi
  if [ "$needs_build" = 1 ]; then
    echo "==> building target/debug/hydra"
    cargo build -p hydra-server --features server --bin hydra >/dev/null || die "cargo build failed"
  fi
fi

# 3. Scratch state.
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"

# 4. Disposable instance.
echo "==> playwright ${PW_VERSION} (matches the CI pin: ${MATCHES_CI}); browsers: ${PLAYWRIGHT_BROWSERS_PATH:-~/.cache/ms-playwright}"
echo "==> starting hydra on ${ADMIN_URL} (data plane 127.0.0.1:${DATA_PORT})"
# `HYDRA_USAGE_SINK=none` is required since ADR-0002 D-1 (there is no default) and it belongs
# INSIDE the assignment chain: an earlier version put `export HYDRA_USAGE_SINK=none` on its own
# line in the middle of the chain, which TERMINATED it — the four assignments above became a bare
# command and the launch ran with the DEFAULT listen address (0.0.0.0:8080 rather than the port
# printed on the line above), so the suite died on "address already in use" and a CI run would
# have exercised a different port than the one it announced.
HYDRA_ADMIN_TOKEN="$TOKEN" \
HYDRA_ADMIN_ADDR="127.0.0.1:${ADMIN_PORT}" \
HYDRA_LISTEN="127.0.0.1:${DATA_PORT}" \
HYDRA_DB_URL="sqlite://$ROOT/$SCRATCH/e2e.db?mode=rwc" \
HYDRA_USAGE_SINK=none \
HYDRA_ENCRYPTION_KEY="${HYDRA_ENCRYPTION_KEY:-MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=}" \
RUST_LOG="${RUST_LOG:-warn}" \
  ./target/debug/hydra > "$SCRATCH/hydra.log" 2>&1 &
HYDRA_PID=$!
cleanup() {
  kill "$HYDRA_PID" 2>/dev/null || true
  wait "$HYDRA_PID" 2>/dev/null || true
  [ "${KEEP:-0}" = 1 ] || rm -rf "$SCRATCH"
}
trap cleanup EXIT INT TERM

ready=no
for _ in $(seq 1 60); do
  kill -0 "$HYDRA_PID" 2>/dev/null || { echo "--- hydra exited ---"; tail -20 "$SCRATCH/hydra.log"; die "hydra did not stay up"; }
  if curl -fsS -H "Authorization: Bearer ${TOKEN}" "${ADMIN_URL}/api/v1/health" >/dev/null 2>&1; then ready=yes; break; fi
  sleep 1
done
[ "$ready" = yes ] || { tail -20 "$SCRATCH/hydra.log"; die "hydra not ready after 60s"; }

# 5. Fixtures.
echo "==> seeding"
HYDRA_ADMIN_ADDR="127.0.0.1:${ADMIN_PORT}" HYDRA_ADMIN_TOKEN="$TOKEN" ./tests/e2e/seed.sh >/dev/null \
  || { tail -20 "$SCRATCH/hydra.log"; die "seed.sh failed"; }

# 6. The suite.
echo "==> playwright test $*"
NODE_PATH="$ROOT/$RUNNER/node_modules" HYDRA_BASE="$ADMIN_URL" HYDRA_ADMIN_TOKEN="$TOKEN" \
  "$RUNNER/node_modules/.bin/playwright" test --config=playwright.config.cjs "$@"
rc=$?
[ "$rc" = 0 ] || { echo "--- last 30 lines of the instance log ---"; tail -30 "$SCRATCH/hydra.log"; }
exit "$rc"
