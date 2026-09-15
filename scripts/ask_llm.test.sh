#!/usr/bin/env bash
# TDD test for scripts/ask_llm.sh (P2-7) + admin-ui/app.js dead code (F-9).
#
#   - With no LLM_API_KEY env, the script must exit NON-ZERO and NOT send a
#     request (the "==> POST …" line is printed immediately before curl, so its
#     absence proves no request was issued).
#   - The dead `sessionStorage.removeItem("hydra-admin-ok")` line must be gone
#     from admin-ui/app.js.
#
# Hermetic: LLM_API_URL is pointed at a dead local port so the CURRENT (broken)
# script — which still has a default key — cannot hit the real gateway.
#
# Run:  bash scripts/ask_llm.test.sh
set -euo pipefail
cd "$(dirname "$0")/.."

SCRIPT="scripts/ask_llm.sh"
DEAD_URL="http://127.0.0.1:1/v1/chat/completions"   # port 1 = closed, fails fast

failures=0
assert() { # name cond detail
  local name="$1" cond="$2" detail="${3:-}"
  if [[ "$cond" == "1" ]]; then echo "PASS  $name"
  else echo "FAIL  $name  -> $detail"; failures=$((failures+1)); fi
}

# --- Test 1: no LLM_API_KEY -> non-zero exit, no request, clear error ---
rc=0
out="$(LLM_API_URL="$DEAD_URL" env -u LLM_API_KEY bash "$SCRIPT" "hello" 2>&1)" || rc=$?

if grep -q "==> POST" <<<"$out"; then sent=1; else sent=0; fi
assert "no LLM_API_KEY: exits non-zero"   "$([[ $rc -ne 0 ]] && echo 1 || echo 0)" "rc=$rc"
assert "no LLM_API_KEY: no request sent"  "$([[ $sent -eq 0 ]] && echo 1 || echo 0)" "output=$out"
assert "no LLM_API_KEY: error names the var" "$(grep -q 'LLM_API_KEY' <<<"$out" && echo 1 || echo 0)" "output=$out"

# --- Test 2: dead sessionStorage line removed from app.js ---
if grep -q 'sessionStorage.removeItem("hydra-admin-ok")' admin-ui/app.js; then removed=0; else removed=1; fi
assert "app.js: dead hydra-admin-ok line removed" "$removed"

echo
if [[ $failures -eq 0 ]]; then echo "E3 (ask_llm + dead-code): ALL PASSED"; exit 0
else echo "E3 (ask_llm + dead-code): $failures FAILED"; exit 1; fi
