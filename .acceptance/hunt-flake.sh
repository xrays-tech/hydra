#!/usr/bin/env bash
# Hunt the intermittent `live-deps` failure on one commit: watch the run, re-run it until it
# reproduces, harvest the diagnostics the drills print, and stop.
#
# `gh run rerun` re-executes the SAME commit, so a red is a property of code+runner rather than of a
# new revision — and it never touches the tree (no empty commits, nothing pushed).
#
# The FIRST sample is the run itself: it has to settle before a rerun is even legal (`gh run rerun`
# refuses while a run is in progress — learned the hard way), and if it is red there is nothing to
# rerun.
#
# Exit 0 = caught (log on disk, interesting lines echoed). Exit 1 = not reproduced / harness failure.
set -u
cd /home/alex/Projects/hydra
RID="${1:?usage: hunt-flake.sh <run-id> [max-attempts]}"
MAX="${2:-6}"
OUT=.cargo-cache/flake-hunt
mkdir -p "$OUT"
LOG="$OUT/log.txt"
say() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOG"; }

settle() { # wait for the run to reach `completed`
  for _ in $(seq 1 80); do
    st="$(gh run view "$RID" --json status --jq .status 2>/dev/null)"
    [ "$st" = completed ] && return 0
    sleep 30
  done
  return 1
}

harvest() { # $1 = attempt label
  local attempt="$1"
  local jid jcon
  jid="$(gh run view "$RID" --json jobs --jq '.jobs[] | select(.name=="live-deps") | .databaseId' 2>/dev/null)"
  jcon="$(gh run view "$RID" --json jobs --jq '.jobs[] | select(.name=="live-deps") | .conclusion' 2>/dev/null)"
  say "attempt $attempt: live-deps=$jcon (job $jid)"
  [ "$jcon" = failure ] || return 1
  say "CAUGHT on attempt $attempt — fetching the job log"
  gh api --allow-escape-sequences "/repos/xrays-tech/hydra/actions/jobs/$jid/logs" 2>/dev/null \
    | sed -e 's/\x1b\[[0-9;]*m//g' > "$OUT/failed-$attempt.log"
  say "log: $OUT/failed-$attempt.log ($(wc -c <"$OUT/failed-$attempt.log") bytes)"
  grep -nE "gate A raw body|cannot encode the config tree|the head could not be committed|materialize_retries_total|arabchne|arachne_publish_total|quorum_unavailable|tenant_forbidden|CANNOT VERIFY|FAIL |publish failures" \
    "$OUT/failed-$attempt.log" | tail -50 | tee -a "$LOG"
  return 0
}

say "hunting $RID for up to $MAX samples (each ~12 min)"

for attempt in $(seq 1 "$MAX"); do
  say "attempt $attempt: waiting for the run to settle"
  settle || { say "attempt $attempt never settled — stopping"; exit 1; }
  if harvest "$attempt"; then exit 0; fi
  if [ "$attempt" = "$MAX" ]; then break; fi
  say "attempt $attempt was green; rerunning"
  if ! gh run rerun "$RID" >>"$LOG" 2>&1; then
    say "the rerun was refused — stopping"
    exit 1
  fi
  sleep 20   # let the rerun flip the run back to in_progress before the next settle()
done

say "not reproduced in $MAX samples (rarer than that, or already gone)"
exit 1
