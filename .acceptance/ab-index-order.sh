#!/usr/bin/env bash
# Discriminating evidence for the index-ordering fix, at the strength the idle run cannot give.
#
# The idle acceptance drill passed 5/5 BOTH before and after the fix, so it is a regression check. The
# condition that actually produced failures (measured earlier: ~1/3 of runs) is a LOADED machine: this
# drill starts three real raft nodes, kills a leader, and measures an election deadline, so CPU
# contention is what makes a stale head read likely.
#
# Arm A: 5 runs with the machine idle.  Arm B: 5 runs with nproc-1 busy loops running.
# The fix is in place for both (it is committed); this measures whether the ordering holds up under the
# condition that used to break it, and reports which gate fails if it still does.
set -u
cd /home/alex/Projects/hydra
OUT=.cargo-cache/ab-index
mkdir -p "$OUT"
export CARGO_HOME=/home/alex/Projects/hydra/.cargo-cache/home
export RUSTUP_TOOLCHAIN=1.98.0
export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380

run_arm() { # $1 = label, $2 = load (yes/no)
  local label="$1" load="$2" fails=0 i rc
  local -a hogs=()
  if [ "$load" = yes ]; then
    for _ in $(seq 1 $(( $(nproc) - 1 ))); do
      ( while :; do :; done ) &
      hogs+=($!)
    done
    echo "[$label] load: ${#hogs[@]} busy loops for $(( $(nproc) - 1 )) cores"
  fi
  for i in 1 2 3 4 5; do
    timeout 900 python3 integration/test_arachne_control_plane.py >"$OUT/$label-$i.log" 2>&1
    rc=$?
    [ $rc -ne 0 ] && fails=$((fails + 1))
    printf '[%s] run %s: exit=%s %s\n' "$label" "$i" "$rc" \
      "$(grep -oE 'FAILED \([0-9]+\): [^;]*' "$OUT/$label-$i.log" | head -1 | cut -c1-70)"
  done
  for p in "${hogs[@]:-}"; do [ -n "$p" ] && kill "$p" 2>/dev/null; done
  wait 2>/dev/null
  printf '[%s] FAILURES: %s/5\n' "$label" "$fails"
}

echo "=== arm A: idle (5 runs)"
run_arm idle no
echo "=== arm B: loaded (5 runs, $(( $(nproc) - 1 )) busy loops)"
run_arm loaded yes
echo "=== done: logs in $OUT"
