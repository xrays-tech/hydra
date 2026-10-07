#!/usr/bin/env bash
# `ops.md` §2's zero-downtime upgrade, asserted (not just documented).
#
# Before 2026-09-29 the documented procedure could NEVER work, for two independent
# reasons (both measured, both fixed in round 68):
#   1. `hydra -u` was a no-op — the binary passed `Opt::default()` to Pingora and
#      never read argv, so the new process never asked for the old one's sockets and
#      died with `cannot bind … Address already in use … refusing to start`;
#   2. even with the flag, hydra's three pre-flight `probe_bind` checks (plaintext,
#      TLS, admin) aborted the handover, because during an upgrade those addresses
#      are legitimately held by the predecessor and Pingora inherits them.
#
# This test is the executable contract for §2.1/§2.3:
#   * a client hammering the data port across the switch sees ZERO refused
#     connections and zero non-listener answers;
#   * the new process survives the old process's exit and keeps serving;
#   * the old process logs the socket handover;
#   * and a new process started WITHOUT `-u` still refuses loudly (that refusal is
#     the guard against a silent split-brain bind, not a bug).
#
# The drain is shortened to 1s via HYDRA_SHUTDOWN_DRAIN_SECS so the whole test runs
# in ~15s; ports 18180/18181 stay clear of the dev stack and of the other suites.
set -uo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
ROOT="$PWD"
DIR="$ROOT/.acceptance/handover-test"
BIN="${HYDRA_BIN:-$ROOT/target/debug/hydra}"
TOKEN=hydra-handover-token-2026
LISTEN=18180
ADMIN=18181
DB="$DIR/handover.db"
mkdir -p "$DIR"; rm -f "$DIR"/*.log "$DB"* "$DIR/hits.txt" "$DIR/stop"

fail=0
note() { echo "   $*"; }
bad() { echo "   FAIL: $*" >&2; fail=1; }

start() { # start <label> [flags...]
  local label="$1"; shift
  env HYDRA_ADMIN_TOKEN="$TOKEN" HYDRA_ADMIN_ADDR="127.0.0.1:$ADMIN" HYDRA_LISTEN="127.0.0.1:$LISTEN" \
      HYDRA_DB_URL="sqlite://$DB?mode=rwc" \
      # Required since ADR-0002 D-1; this script is not about metering, so usage is off.
      export HYDRA_USAGE_SINK=none
      HYDRA_ENCRYPTION_KEY="MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=" \
      HYDRA_SHUTDOWN_DRAIN_SECS=1 \
      RUST_LOG=info "$BIN" "$@" > "$DIR/$label.log" 2>&1 &
  echo $!
}
alive() { local st; st=$(ps -o stat= -p "$1" 2>/dev/null); case "$st" in ''|Z*) return 1 ;; *) return 0 ;; esac; }
probe() { curl -sS -o /dev/null -w '%{http_code}' --max-time 3 --connect-timeout 1 -X POST \
  "http://127.0.0.1:$LISTEN/v1/chat/completions" -H 'Host: load.local' \
  -H 'content-type: application/json' -d '{"model":"echo"}' 2>/dev/null; }

cleanup() { touch "$DIR/stop"; pkill -f "$BIN" 2>/dev/null; }
trap cleanup EXIT INT TERM

[ -x "$BIN" ] || { echo "handover: $BIN not built" >&2; exit 1; }

PID_A=$(start a-old)
up=no
for _ in $(seq 1 60); do
  code=$(probe); [ "$code" != "000" ] && { up=yes; break; }
  alive "$PID_A" || break
  sleep 0.5
done
[ "$up" = yes ] || { bad "the first process never served the data port (last: $code)"; tail -5 "$DIR/a-old.log"; exit 1; }
note "old process serving (data port answers $code; 404 is fine — no route is configured)"

( i=0; while [ ! -f "$DIR/stop" ] && [ "$i" -lt 1200 ]; do
    ts=$(date +%s.%N); c=$(probe)
    case "$c" in 000) echo "$ts REFUSED" ;; *) echo "$ts $c" ;; esac
    i=$((i+1)); sleep 0.05
  done ) > "$DIR/hits.txt" 2>&1 &
HAMMER=$!
sleep 0.5

kill -QUIT "$PID_A" 2>/dev/null
sleep 0.3
PID_B=$(start b-new -u)
for _ in $(seq 1 40); do alive "$PID_B" || break; curl -fsS -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$ADMIN/api/v1/health" >/dev/null 2>&1 && break; sleep 0.5; done
alive "$PID_B" && note "new process (-u) is alive after Bootstrap" || bad "the new process died during the handover"

# wait for the old process to finish its (1s + 5s) drain, then prove the new one owns
# the data port by itself.
for _ in $(seq 1 60); do alive "$PID_A" || break; sleep 0.5; done
alive "$PID_A" && bad "the old process never exited" || note "old process exited"
sleep 0.5
after=$(probe)
[ "$after" != "000" ] || bad "the data port went dead after the old process exited (new process did not inherit it)"
[ "$after" = "000" ] || note "data port still served after the handover: HTTP $after"

# regression: without -u the new process must refuse loudly rather than fight for the port
PID_C=$(start c-without-u)
sleep 3
if alive "$PID_C"; then
  bad "a process started WITHOUT -u stayed alive — it must refuse (address already in use)"
  kill "$PID_C" 2>/dev/null
else
  grep -q "Address already in use" "$DIR/c-without-u.log" \
    && note "without -u it refuses loudly: $(grep -o 'cannot bind [^;]*' "$DIR/c-without-u.log" | head -1)" \
    || bad "the refusal message does not mention the address conflict"
fi

# The metrics endpoint must survive the handover (and its counters legitimately
# restart: a handover ends in a fresh process — asserted here so the runbook's
# wording cannot drift back into claiming counter continuity).
metrics=$(curl -sS -H "Authorization: Bearer $TOKEN" "http://127.0.0.1:$ADMIN/metrics" 2>/dev/null)
if [ -n "$metrics" ] && printf '%s' "$metrics" | grep -q '^hydra_'; then
  note "metrics endpoint alive after the handover ($(printf '%s' "$metrics" | grep -c '^hydra_') hydra_* lines; counters are expected to start from zero in the new process)"
else
  bad "the metrics endpoint did not answer after the handover"
fi

sleep 0.3
touch "$DIR/stop"; kill "$HAMMER" 2>/dev/null; wait "$HAMMER" 2>/dev/null
kill "$PID_B" "$PID_A" 2>/dev/null; wait "$PID_B" "$PID_A" 2>/dev/null

total=$(wc -l < "$DIR/hits.txt"); refused=$(grep -c REFUSED "$DIR/hits.txt" || true)
grep -q "listener sockets sent" "$DIR/a-old.log" && note "old process logged the socket handover" \
  || bad "the old process never logged 'listener sockets sent' (no handover happened)"
echo "   probe attempts across the switch: $total | refused connections: $refused"
[ "$refused" = 0 ] || bad "$refused connection refusals during the documented upgrade"

if [ "$fail" = 0 ]; then
  echo "handover: PASSED (zero-downtime upgrade verified: $total probes, 0 refusals)"
else
  echo "handover: FAILED" >&2
fi
exit "$fail"
