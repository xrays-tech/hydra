#!/usr/bin/env bash
# Hydra wave-6 §2.4 — real-process load / measurement harness.
#
# WHAT THIS ACTUALLY DOES (the previous header claimed three "verifications" that
# the script never performed: the distribution function was defined and never
# called, the load-tool output was printed and its exit status discarded, and the
# script ended on `echo` so it always exited 0):
#
#   1. OPTIONAL SWRR distribution sampling  — CHECK_DISTRIBUTION=1 (off by
#      default: it costs N sequential curls and needs an upstream that echoes
#      `X-Echo-Instance`). When on, a wrong or missing distribution FAILS.
#   2. RPS / P99 measurement via `oha` (or `wrk`) — a measurement, not an
#      assertion; a FAILING load tool is still a failure.
#   3. Breaker state INSPECTION (`GET /api/v1/breaker`) — printed for the
#      operator; this script does not assert on it.
#
# The DETERMINISTIC gates live elsewhere and are the ones to cite:
#   * `cargo test -p hydra-server --features server --test load_breaker_swrr`
#     (SWRR 3:1 distribution + breaker death/revival, in-process, asserted)
#   * `.acceptance/phase-b-gate.sh` (full gate, real exit codes)
#
# This harness requires a RUNNING hydra instance with a real upstream
# (dev-plan §1 铁律 2: no internal mocks). Environment it cannot set up for you:
#   1. an echo upstream that records which instance served each request
#      (return its bind port in an `X-Echo-Instance` header);
#   2. hydra pointed at that upstream with two providers weighted 3:1.
#
# REPRODUCING THE PUBLISHED BENCHMARK (docs/index.html stat band: 11056 RPS @
# c=25, p99 4.39 ms, 0.3 ms gateway overhead, RSS 18.6 -> 65.4 MiB):
#   * use `C=25` and a run long enough to reach steady state. The `N=1000`
#     default cannot measure a peak — it is a warm-up; use a large N, or
#     `oha -z 8s`.
#   * the upstream must be CONCURRENT. Do NOT point this at the repository's own
#     `integration/mock_llm.py` / `mock_auth.py`: both are single-threaded Python
#     `HTTPServer`s. Measured 2026-09-29 on a 14-core box with the SAME release
#     binary: upstream = `mock_llm.py` ⇒ ~2.4k RPS (that is the mock's ceiling,
#     not the gateway's); upstream = a concurrent Go echo server ⇒ 25.0k RPS at
#     c=25 with p99 1.55 ms, and the gateway's own RSS 21 MiB idle -> 30 MiB
#     under load. A single-threaded upstream measures the mock.
#   * the published figures are therefore conservative on that hardware, and they
#     are hardware-dependent: quote the machine alongside any new number.
#
# USAGE: HYDRA_ADMIN_TOKEN=... CHECK_DISTRIBUTION=1 ./scripts/load_test.sh
#
# SELFTEST_ONLY=1 runs JUST the deterministic self-check of the distribution judge and exits with its
# verdict — no instance, no token, no load tool. It exists because this script is otherwise executed
# by NOTHING AUTOMATED (it needs a live instance and a concurrent echo upstream), so its own judge had
# no machine-checked evidence behind it: round 6 verified those four cases by hand. The local gate now
# runs `SELFTEST_ONLY=1 bash scripts/load_test.sh`, which is the automatable half; the measurement half
# stays manual and that is stated here rather than implied.
set -uo pipefail

# ---- config (override via env) --------------------------------------------
HYDRA_ADMIN_ADDR="${HYDRA_ADMIN_ADDR:-127.0.0.1:8081}"
# `SELFTEST_ONLY=1` never talks to a node, so it must not demand a token — the `:?` below is the
# normal path's early check, and the self-test must stay runnable with no environment at all.
if [[ "${SELFTEST_ONLY:-0}" == "1" ]]; then
  HYDRA_ADMIN_TOKEN="${HYDRA_ADMIN_TOKEN:-unused-selftest-only}"
else
  HYDRA_ADMIN_TOKEN="${HYDRA_ADMIN_TOKEN:?HYDRA_ADMIN_TOKEN must be set}"
fi
PROXY_BASE="${PROXY_BASE:-http://127.0.0.1:8080}"   # tenant domain via Host header
TENANT_DOMAIN="${TENANT_DOMAIN:-loadtest.example.com}"
MODEL="${MODEL:-echo}"
N="${N:-1000}"                                        # request count for distribution check
C="${C:-8}"                                           # concurrency
QUIET="${QUIET:-0}"
CHECK_DISTRIBUTION="${CHECK_DISTRIBUTION:-0}"         # 1 => sample and ASSERT the ratio
EXPECT_RATIO="${EXPECT_RATIO:-3:1}"                   # provider weight ratio to assert
# Absolute tolerance on the minority share: 3:1 ⇒ expected 0.25, accept ±0.10.
RATIO_TOLERANCE="${RATIO_TOLERANCE:-0.10}"
fail=0

curl_admin() {  # curl_admin <method> <path> [json-body]
  local method="$1" path="$2" body="${3:-}"
  if [[ -n "$body" ]]; then
    curl -sS -X "$method" "http://${HYDRA_ADMIN_ADDR}${path}" \
      -H "Authorization: Bearer ${HYDRA_ADMIN_TOKEN}" \
      -H "content-type: application/json" -d "$body"
  else
    curl -sS -X "$method" "http://${HYDRA_ADMIN_ADDR}${path}" \
      -H "Authorization: Bearer ${HYDRA_ADMIN_TOKEN}"
  fi
}

echo "==> hydra load/correctness harness"
echo "    admin:   http://${HYDRA_ADMIN_ADDR}"
echo "    proxy:   ${PROXY_BASE}  (Host: ${TENANT_DOMAIN})"
echo "    model:   ${MODEL}   N=${N}  C=${C}  CHECK_DISTRIBUTION=${CHECK_DISTRIBUTION}"

  # Judge an instance distribution from a `uniq -c` table (`<count> <name>` lines).
#
# The MINORITY share is the smallest per-instance share, not the last row's: with
# `sort -rn` the last row happens to be the smallest for two instances, but for
# three or more it is not, so the old `c[NR]/s` was wrong for any fleet larger
# than the two-provider fixture it was written against.
distribution_verdict() {
  awk -v ratio="$2" -v tol="$3" '
    { c[NR]=$1; s+=$1; if (min == "" || $1 < min) min = $1 }
    END {
      if (NR < 2) { print "SKIP one instance only"; exit }
      split(ratio, r, ":")
      tot = r[1] + r[2]
      expected = (r[2] < r[1] ? r[2] : r[1]) / tot
      share = min / s
      lo = expected - tol; hi = expected + tol
      if (share < lo || share > hi)
        printf "FAIL minority share %.3f outside [%.3f, %.3f] (expected %.3f)\n", share, lo, hi, expected
      else
        printf "PASS minority share %.3f within [%.3f, %.3f] (expected %.3f)\n", share, lo, hi, expected
    }' "$1"
}

# Self-check: run the REAL judge on inputs whose answers are known, so a bug in it
# cannot pass unnoticed. (The previous "self-check" fed the judge a different input
# SHAPE than production did, so it proved nothing about the production path.)
distribution_verdict_selfcheck() {
  local dir; dir="$(mktemp -d)"
  printf '    750 9001\n    250 9002\n' > "$dir/pass"
  printf '    900 9001\n    100 9002\n' > "$dir/fail_skewed"
  printf '    500 9001\n    500 9002\n' > "$dir/fail_flat"
  # UNSORTED on purpose, chosen so the two logics DISAGREE: the minimum share is
  # 0.100 (outside [0.15, 0.35] ⇒ FAIL), while the LAST row is 0.320 (inside ⇒
  # PASS). A case where both land outside the band would "pass" for the wrong
  # reason — the first version of this fixture did exactly that.
  printf '    100 9001\n    580 9002\n    320 9003\n' > "$dir/three"
  local rc=0 v
  v="$(distribution_verdict "$dir/pass" "3:1" 0.1)";  [[ "$v" == PASS* ]] || { echo "selfcheck: 3:1 split must PASS, got: $v" >&2; rc=1; }
  v="$(distribution_verdict "$dir/fail_skewed" "3:1" 0.1)"; [[ "$v" == FAIL* ]] || { echo "selfcheck: 9:1 split must FAIL, got: $v" >&2; rc=1; }
  v="$(distribution_verdict "$dir/fail_flat" "3:1" 0.1)";   [[ "$v" == FAIL* ]] || { echo "selfcheck: 1:1 split must FAIL, got: $v" >&2; rc=1; }
  v="$(distribution_verdict "$dir/three" "3:1" 0.1)";       [[ "$v" == FAIL* ]] || { echo "selfcheck: three instances must compare the SMALLEST share, got: $v" >&2; rc=1; }
  rm -rf "$dir"
  return "$rc"
}


# ---- 0. the judge's self-check, runnable WITHOUT an instance ---------------
# Placed BEFORE the health probe: the self-check needs no node, so a gate can run it anywhere, and a
# health failure must not be able to hide a broken judge (the normal path re-runs it below, before any
# measurement is trusted).
if [[ "${SELFTEST_ONLY:-0}" == "1" ]]; then
  if distribution_verdict_selfcheck; then
    echo "SELFTEST ONLY: the SWRR distribution judge passed all four known-input cases"
    exit 0
  fi
  echo "FAILED: the SWRR distribution judge failed its own self-check." >&2
  exit 1
fi

# ---- 0. sanity: admin health ----------------------------------------------
health="$(curl_admin GET /api/v1/health 2>/dev/null)" || health=""
echo "    health:  ${health}"
if ! echo "${health}" | grep -q '"status":"ok"'; then
  echo "FAILED: hydra not healthy at http://${HYDRA_ADMIN_ADDR}" >&2
  exit 1
fi

# The judge is exercised on known inputs BEFORE it is trusted with measurements:
# a judge that mis-measures is worse than no judge, and the earlier "self-check"
# fed it a different input SHAPE than production did, so it proved nothing.
if ! distribution_verdict_selfcheck; then
  echo "FAILED: the SWRR distribution judge failed its own self-check." >&2
  exit 1
fi

# ---- 1. OPTIONAL SWRR distribution ----------------------------------------
# Counts which provider served each request via the upstream's X-Echo-Instance
# header. A missing header is a FAILURE when this check is on: it means the
# upstream is not the instrumented one, so nothing was actually measured.
if [[ "$CHECK_DISTRIBUTION" == "1" ]]; then
  echo
  echo "==> [1] SWRR distribution (sampling ${N} requests, expect ${EXPECT_RATIO})"
  counts_file="$(mktemp)"; counts_uniq="$(mktemp)"
  trap 'rm -f "$counts_file" "$counts_uniq"' EXIT
  sampled=0
  for _ in $(seq 1 "$N"); do
    inst="$(curl -sS -D - -o /dev/null \
              -H "Host: ${TENANT_DOMAIN}" -H "content-type: application/json" \
              -d "{\"model\":\"${MODEL}\"}" \
              "${PROXY_BASE}/v1/chat/completions" 2>/dev/null \
              | awk 'tolower($1) ~ /^x-echo-instance:$/ {print $2}' | tr -d '\r')" || true
    if [[ -n "$inst" ]]; then echo "$inst" >> "$counts_file"; sampled=$((sampled+1)); fi
  done
  echo "    sampled with X-Echo-Instance: ${sampled}/${N}"
if [[ "$sampled" -eq 0 ]]; then
    echo "FAILED: no response carried X-Echo-Instance — nothing was measured." >&2
    echo "        point the harness at the instrumented echo upstream (see header)." >&2
    fail=1
  else
    # The verdict input is the `uniq -c` output — `<count> <name>` lines — NOT the
    # raw list of names. Feeding it names made `$1` the instance NAME: with
    # numeric-looking ports it silently computed nonsense (750x9001 + 250x9002
    # reported "minority share 0.001", i.e. a FAIL for a CORRECT 3:1 split), and
    # with non-numeric names awk died on a division by zero. The old code even
    # printed the `uniq -c` table and then judged a different file.
    sort "$counts_file" | uniq -c | sort -rn | tee "$counts_uniq" | sed 's/^/    /'
    verdict="$(distribution_verdict "$counts_uniq" "$EXPECT_RATIO" "$RATIO_TOLERANCE")"
    echo "    ${verdict}"
    [[ "$verdict" == PASS* ]] || fail=1
  fi
else
  echo
  echo "==> [1] SWRR distribution: SKIPPED (set CHECK_DISTRIBUTION=1 to sample+assert)."
  echo "    deterministic assertion: cargo test -p hydra-server --features server --test load_breaker_swrr"
fi

# ---- 2. RPS / P99 measurement ---------------------------------------------
echo
echo "==> [2] RPS / P99 (measurement only — not an assertion)"
if command -v oha >/dev/null 2>&1; then
  echo "    running ${N} requests via oha (C=${C})…"
  if ! oha -n "$N" -c "$C" -q 0 --no-tui \
      -H "Host: ${TENANT_DOMAIN}" -H "content-type: application/json" \
      -m POST -d "{\"model\":\"${MODEL}\"}" \
      "${PROXY_BASE}/v1/chat/completions"; then
    echo "FAILED: oha exited non-zero" >&2; fail=1
  fi
elif command -v wrk >/dev/null 2>&1; then
  echo "    running wrk (use oha for richer stats)…"
  if ! TENANT_DOMAIN="$TENANT_DOMAIN" MODEL="$MODEL" \
      wrk -t"$C" -c"$C" -d10s -s /dev/stdin "${PROXY_BASE}/v1/chat/completions" <<'LUA'
    wrk.method = "POST"
    wrk.headers["Host"] = os.getenv("TENANT_DOMAIN")
    wrk.headers["content-type"] = "application/json"
    wrk.body = '{"model":"' .. os.getenv("MODEL") .. '"}'
LUA
  then
    echo "FAILED: wrk exited non-zero" >&2; fail=1
  fi
else
  # Previously this "skipped" silently and the script still exited 0, so a run
  # with no load tool at all looked the same as a successful one.
  echo "FAILED: neither oha nor wrk is installed — no RPS measurement happened." >&2
  echo "        install oha (https://github.com/hatoo/oha), or set" >&2
  echo "        SKIP_LOAD_MEASUREMENT=1 to accept a run without it." >&2
  [[ "${SKIP_LOAD_MEASUREMENT:-0}" == "1" ]] || fail=1
fi

# ---- 3. breaker inspection (observation, deliberately not asserted) --------
echo
echo "==> [3] breaker state (inspect only — deterministic breaker assertions are in load_breaker_swrr)"
curl_admin GET /api/v1/breaker
echo

echo
if [[ "$fail" == "0" ]]; then
  echo "==> done: every executed check passed"
else
  echo "==> done: FAILURES reported above" >&2
fi
exit "$fail"
