#!/usr/bin/env bash
# Tenant-API baseline gate.
#
# EXIT-CODE CONTRACT: the `run` helper judges each command's OWN exit status and
# the script exits non-zero if any step failed. The previous version printed
# `RESULT[...]: FAIL` and still exited 0 — so "门禁通过" was not a statement
# about anything. It also ran a bare `cargo tree` as the "core-firewall" step:
# `cargo tree` prints a tree and exits 0 whenever the tree is non-empty, so that
# step could never fail. The firewall is a grep over the tree (as CI does it).
set -uo pipefail
export RUSTFLAGS="-D warnings"
export SQLX_OFFLINE=true
export HYDRA_TEST_REDIS_URL="redis://127.0.0.1:6380"   # 本地只有 6380 映射到宿主；CI 是 6379
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO"
LOGDIR="$REPO/.acceptance/logs"; mkdir -p "$LOGDIR"
LAST_LOG="$LOGDIR/last.log"; GATE_LOG="$LOGDIR/tenant-api-gate.log"; : > "$GATE_LOG"
fail=0
declare -a RESULTS=()
step() { echo; echo "########## $* ##########"; }

run() {
  local name="$1"; shift
  step "$name"
  if "$@" >"$LAST_LOG" 2>&1; then
    cat "$LAST_LOG" >> "$GATE_LOG"; tail -12 "$LAST_LOG"
    RESULTS+=("PASS  $name"); echo "RESULT[$name]: OK"
  else
    local rc=$?; cat "$LAST_LOG" >> "$GATE_LOG"; tail -30 "$LAST_LOG"
    RESULTS+=("FAIL  $name (exit $rc)"); echo "RESULT[$name]: FAIL (exit $rc)"; fail=1
  fi
}

run "fmt --check"    cargo fmt --check
run "clippy-server"  cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
run "core-tests"     cargo test -p hydra-core
run "server-tests"   cargo test -p hydra-server --features server

step "core-firewall"
tree="$(cargo tree -p hydra-core --no-default-features 2>&1)"
if echo "$tree" | grep -E ' (tokio|pingora|sqlx|reqwest|hyper)(-[^ ]+)? v'; then
  echo "::error::hydra-core pulls a forbidden I/O dependency"; fail=1
  RESULTS+=("FAIL  core-firewall")
else
  echo "core-firewall OK (no tokio/pingora/sqlx/reqwest/hyper)"; RESULTS+=("PASS  core-firewall")
fi

echo; echo "########## SUMMARY ##########"
for r in "${RESULTS[@]}"; do echo "  $r"; done
echo "########## BASELINE DONE: fail=$fail (logs: $LOGDIR) ##########"
exit $fail
