#!/usr/bin/env bash
# T3 regression baseline + live-ClickHouse acceptance gate.
#
# PREREQUISITES (checked below, fail-fast with a precise message):
#   * a live ClickHouse at CH_URL           (default http://127.0.0.1:8123)
#   * a live Redis at HYDRA_TEST_REDIS_URL  (default redis://127.0.0.1:6380)
#
# EXIT-CODE CONTRACT: every step goes through `run`, which judges the command's
# own exit status, and the script exits non-zero if any step failed. This script
# previously had NO failure path at all (no `exit`, every check piped into
# `tail`), so its only real consumer — the live `--ignored` ClickHouse suites —
# ran inside a script that always reported success. `CI` never runs `--ignored`
# (`grep -c -- --ignored .github/workflows/ci.yml` = 0), which made this the only
# carrier of those assertions.
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
export RUSTFLAGS="-D warnings"; export SQLX_OFFLINE=true
CH_URL="${CH_URL:-http://127.0.0.1:8123}"
export CH_URL
REDIS_URL="${HYDRA_TEST_REDIS_URL:-redis://127.0.0.1:6380}"
LOGDIR="$REPO/.acceptance/logs"; mkdir -p "$LOGDIR"
LAST_LOG="$LOGDIR/last.log"; GATE_LOG="$LOGDIR/t3-baseline.log"; : > "$GATE_LOG"
fail=0
declare -a RESULTS=()
step() { echo; echo "########## $* ##########"; }

run() {
  local name="$1"; shift
  step "$name"
  if "$@" >"$LAST_LOG" 2>&1; then
    cat "$LAST_LOG" >> "$GATE_LOG"; tail -18 "$LAST_LOG"
    RESULTS+=("PASS  $name"); echo "RESULT[$name]: OK"
  else
    local rc=$?; cat "$LAST_LOG" >> "$GATE_LOG"; tail -30 "$LAST_LOG"
    RESULTS+=("FAIL  $name (exit $rc)"); echo "RESULT[$name]: FAIL (exit $rc)"; fail=1
  fi
}

step "0. prerequisites"
if curl -fsS --data-binary "SELECT 1" "$CH_URL/" >/dev/null 2>&1; then
  echo "clickhouse reachable at $CH_URL"; RESULTS+=("PASS  0a. clickhouse reachable")
else
  echo "::error::no ClickHouse at $CH_URL — this gate is about the LIVE instance."
  echo "          start it (environment/docker-compose.local.yml) or set CH_URL, then re-run."
  RESULTS+=("FAIL  0a. clickhouse reachable"); fail=1
fi
if python3 - "$REDIS_URL" <<'PY' >/dev/null 2>&1
import socket, sys, urllib.parse
u = urllib.parse.urlparse(sys.argv[1])
s = socket.create_connection((u.hostname or "127.0.0.1", u.port or 6379), timeout=3)
s.close()
PY
then
  echo "redis reachable at $REDIS_URL"; RESULTS+=("PASS  0b. redis reachable")
else
  echo "::warning::no Redis at $REDIS_URL — the cluster-redis halves of this gate will fail"
  RESULTS+=("WARN  0b. redis NOT reachable ($REDIS_URL)")
fi
[ "$fail" = 0 ] || { echo; echo "########## T3 BASELINE ABORTED (prerequisites) ##########"; exit $fail; }

step "usage_record before (live CH)"
echo "CH 行数（跑前）: $(curl -fsS --data-binary "SELECT count() FROM usage_record" "$CH_URL/" 2>/dev/null || echo '<query failed>')"

run "1. 既有 clickhouse_sink 套件（搬迁前基线）" \
  cargo test -p hydra-server --features server,usage-clickhouse --test clickhouse_sink
run "2. 活 CH 的 #[ignore] 用例（真实断言在这里）" \
  cargo test -p hydra-server --features server,usage-clickhouse --test clickhouse_sink -- --ignored --nocapture
run "3. usage_query 的活 CH #[ignore] 用例" \
  cargo test -p hydra-server --features server,usage-clickhouse --test usage_query -- --ignored --nocapture

step "usage_record after (live CH)"
echo "CH 行数（跑后）: $(curl -fsS --data-binary "SELECT count() FROM usage_record" "$CH_URL/" 2>/dev/null || echo '<query failed>')"

run "4. 三特性组合可编译（计划里多条命令依赖它）" \
  cargo check -p hydra-server --features server,cluster-redis,usage-clickhouse --all-targets
run "5. server+cluster-redis 组合（T6 集群用例依赖）" \
  cargo check -p hydra-server --features server,cluster-redis --all-targets

echo; echo "########## SUMMARY ##########"
for r in "${RESULTS[@]}"; do echo "  $r"; done
echo "########## DONE: fail=$fail (logs: $LOGDIR) ##########"
exit $fail
