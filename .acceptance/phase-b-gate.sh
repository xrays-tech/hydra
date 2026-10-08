#!/usr/bin/env bash
# Phase B gate = the Phase A command set + the real-browser leg.
#
# EXIT-CODE CONTRACT (this is the point of the script): every step runs through
# `run`, which judges the command's OWN exit status, and the script exits with
# the accumulated `fail`. Do NOT reintroduce the `cmd | tail` idiom — the
# pipeline's status is then `tail`'s, which is always 0, so a failing
# clippy/build/test was reported as a pass. That bug made "all gates green" true
# even while `cargo clippy --features ...cluster-redis,...` failed to compile
# (the same command CI runs in its `optional-features` job).
#
# Local deviations forced by the environment (all repo-local, nothing in /tmp,
# which is a fresh tmpfs per command here):
#   * CARGO_HOME      -> $PWD/.cargo-cache/home   (/home is read-only)
#   * npm cache       -> $PWD/.acceptance/npm-cache
#   * PW browsers     -> $PWD/.acceptance/pw-browsers
#   * npm install     -> $PWD/.acceptance/e2e (the repo must NOT grow a root
#                        package.json: the UI has no build step, and CI creates
#                        its own manifest inline) + NODE_PATH so the specs
#                        resolve @playwright/test.
#   * instance DB/pid/log -> $PWD/.acceptance/e2e/run
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
export CARGO_HOME="$REPO/.cargo-cache/home"; export PATH="$CARGO_HOME/bin:$PATH"
export RUSTFLAGS="-D warnings" SQLX_OFFLINE=true
export HYDRA_SHUTDOWN_DRAIN_SECS=20
export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
export npm_config_cache="$REPO/.acceptance/npm-cache"
export PLAYWRIGHT_BROWSERS_PATH="$REPO/.acceptance/pw-browsers"
export NODE_PATH="$REPO/.acceptance/e2e/node_modules"

fail=0
declare -a RESULTS=()
LOGDIR="$REPO/.acceptance/logs"; mkdir -p "$LOGDIR"
GATE_LOG="$LOGDIR/gate.log"; LAST_LOG="$LOGDIR/last.log"
: > "$GATE_LOG"
step() { echo; echo "===== $* ====="; }

# run <label> <cmd...> — real exit code. Output goes to $LAST_LOG (this step)
# and is appended to $GATE_LOG; only the tail reaches stdout.
run() {
  local label="$1"; shift
  step "$label"
  if "$@" >"$LAST_LOG" 2>&1; then
    cat "$LAST_LOG" >> "$GATE_LOG"
    tail -3 "$LAST_LOG"
    RESULTS+=("PASS  $label")
    echo "RESULT[$label]: OK"
  else
    local rc=$?
    cat "$LAST_LOG" >> "$GATE_LOG"
    tail -30 "$LAST_LOG"
    RESULTS+=("FAIL  $label (exit $rc)")
    echo "RESULT[$label]: FAIL (exit $rc)"
    fail=1
  fi
}

run "1a. lockfile fresh"         cargo update --workspace --locked --dry-run
run "1b. fmt"                    cargo fmt --check
run "1c. clippy (server)"        cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
run "1d. release build (server)" cargo build --release --workspace --features hydra-server/server

run "2a. hydra-core tests"       cargo test -p hydra-core

step "2b. dependency firewall"
tree="$(cargo tree -p hydra-core --no-default-features 2>>"$LAST_LOG")"
if echo "$tree" | grep -E ' (tokio|pingora|sqlx|reqwest|hyper)(-[^ ]+)? v'; then
  echo "::error::hydra-core pulls a forbidden I/O dependency"; fail=1
  RESULTS+=("FAIL  2b. dependency firewall")
else
  echo "dependency firewall OK"; RESULTS+=("PASS  2b. dependency firewall")
fi

run "3a. hydra-server tests (server)" cargo test -p hydra-server --features server
echo "server suites ok: $(grep -cE '^test result: ok\.' "$LAST_LOG")"

run "3b. clippy (server+cluster-redis+usage-clickhouse)" \
  cargo clippy --workspace --all-targets --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse -- -D warnings
run "3c. release build (all optional features)" \
  cargo build --release --workspace --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse
run "3d. hydra-server tests (all optional features)" \
  cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse

step "3e. all-features suite totals (from step 3d's own run)"
allfeat_passed=$(grep -E '^test result: ok\.' "$LAST_LOG" | awk '{s+=$4} END {print s+0}')
allfeat_ignored=$(grep -E '^test result: ok\.' "$LAST_LOG" | awk '{s+=$8} END {print s+0}')
echo "all-features: ${allfeat_passed} passed / ${allfeat_ignored} ignored"
if grep -qE '^test result: FAILED' "$LAST_LOG"; then
  echo "::error::a suite reported FAILED"; fail=1; RESULTS+=("FAIL  3e. no suite reported FAILED")
elif [ "$allfeat_passed" -eq 0 ]; then
  echo "::error::no suite reported results — the run did not execute tests"; fail=1
  RESULTS+=("FAIL  3e. no suite reported results")
else
  RESULTS+=("PASS  3e. ${allfeat_passed} passed / ${allfeat_ignored} ignored, 0 failed")
fi

run "4a. i18n consistency"       node scripts/check_i18n.js
run "4b. i18n checker tests"     node --test scripts/check_i18n.test.cjs
run "4c. ask_llm shell checks"   bash scripts/ask_llm.test.sh

step "5. real-browser leg: start the NEW binary, seed, Playwright"
RUN="$REPO/.acceptance/e2e/run"; mkdir -p "$RUN"
if [ -f "$RUN/hydra.pid" ]; then kill "$(cat "$RUN/hydra.pid")" 2>/dev/null || true; sleep 1; fi
# NOTE: never `pkill -f target/release/hydra` here — the pattern also matches the
# shell that is running this script (its command line carries the heredoc that
# created it), which kills the gate itself. Kill by pid file only; if an unknown
# process still holds the port, the new instance exits and the readiness guard
# below fails loudly instead of silently validating a stale binary.
sleep 1
rm -f "$RUN/e2e.db" "$RUN/e2e.db-shm" "$RUN/e2e.db-wal"
(
  cd "$RUN"
  nohup env HYDRA_ADMIN_TOKEN=dev-admin-token-2026 HYDRA_ADMIN_ADDR=127.0.0.1:18081 \
    HYDRA_LISTEN=127.0.0.1:18080 \
    HYDRA_DB_URL="sqlite://$RUN/e2e.db?mode=rwc" \
    HYDRA_ENCRYPTION_KEY=MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY= \
    "$REPO/target/release/hydra" >"$RUN/hydra.log" 2>&1 &
  echo $! > "$RUN/hydra.pid"
)
ready=0
for _ in $(seq 1 60); do
  kill -0 "$(cat "$RUN/hydra.pid")" 2>/dev/null || { echo "::error::hydra exited"; tail -30 "$RUN/hydra.log"; fail=1; break; }
  curl -fsS -H "Authorization: Bearer dev-admin-token-2026" \
    http://127.0.0.1:18081/api/v1/health >/dev/null 2>&1 && { ready=1; break; }
  sleep 1
done
if [ "${ready:-0}" = "1" ]; then
  RESULTS+=("PASS  5a. instance ready")
else
  echo "::error::hydra never became ready"; tail -30 "$RUN/hydra.log"; fail=1
  RESULTS+=("FAIL  5a. instance ready")
fi

run "5b. seed fixtures" env HYDRA_ADMIN_ADDR=127.0.0.1:18081 HYDRA_ADMIN_TOKEN=dev-admin-token-2026 ./tests/e2e/seed.sh

# pushd (not a subshell): `fail`/`RESULTS` must survive this step.
pushd "$REPO/.acceptance/e2e" >/dev/null
run "5c. Playwright (real browser)" env HYDRA_BASE=http://127.0.0.1:18081 HYDRA_ADMIN_TOKEN=dev-admin-token-2026 \
  npx playwright test --config="$REPO/playwright.config.cjs"
popd >/dev/null

echo
echo "===== PHASE B GATE SUMMARY ====="
for r in "${RESULTS[@]}"; do echo "  $r"; done
echo "===== PHASE B GATE RESULT: fail=$fail (logs: $LOGDIR) ====="
exit $fail
