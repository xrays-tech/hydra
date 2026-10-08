#!/usr/bin/env bash
# Round-10 gate. Every command's exit code is captured EXPLICITLY: a previous
# version of this gate piped clippy into `tail`/`grep`, so a red clippy reported
# exit 0 and two lints in my own new tests reached the tree unnoticed.
#
# PRE-FLIGHT (round 193, learned twice): if you added or removed a Rust test, the public page's
# advertised counts are stale and entry 29 (`public claims`) WILL be red — but only after the two
# cargo test entries above it have produced transcripts (~8 min in). Run this FIRST instead:
#
#     cargo fmt --check && node scripts/check_public_claims.cjs --measure --write
#
# ...then start the gate once. Both checkers are cheap; a wasted 40-minute run is not.
# Related, same round: to STOP a running gate, kill the job handle (or the bash pid) — never
# `pkill -f cargo`, whose pattern also matches the command line you typed it in (it killed the
# shell that was about to move the log out of the way).
cd /home/alex/Projects/hydra
export SQLX_OFFLINE=true
export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
export CH_URL=http://127.0.0.1:8123
LOG=".acceptance/round10-gate.log"
: > "$LOG"
declare -a NAMES=() RCS=()
gate() {
  local name="$1"; shift
  echo "########## $name" >> "$LOG"
  echo "########## \$ $*" >> "$LOG"
  "$@" >> "$LOG" 2>&1
  local rc=$?
  NAMES+=("$name"); RCS+=("$rc")
  # The verdict goes to the LOG too, not only to the terminal: the log is truncated at the start of
  # every run, so with stdout-only lines a log that simply STOPS (killed, out of disk, terminal gone)
  # was indistinguishable from one that finished GREEN (round 163 — the evidence chain could not be
  # audited after the fact).
  echo "GATE $name exit=$rc" | tee -a "$LOG"
  printf '########## exit=%s\n' "$rc" >> "$LOG"
}
# FIRST entry: record the source tree, so the verdict at the end can be tied to a revision.
# The round-178 run was GREEN while two judged entries had run BEFORE that round's edits to
# the very files they check; only a human re-reading the transcript could tell.
gate "tree manifest (record)"           node scripts/tree_manifest.cjs --write .acceptance/gate-manifest.txt
gate "fmt --check"                     cargo fmt --check
gate "clippy server (CI check job)"    cargo clippy --workspace --all-targets --features hydra-server/server -- -D warnings
gate "clippy optional (CI job)"        cargo clippy --workspace --all-targets --features hydra-server/server,hydra-server/cluster-redis,hydra-server/usage-clickhouse -- -D warnings
gate "clippy tls-openssl (new CI job)" cargo clippy --workspace --all-targets --features hydra-server/tls-openssl -- -D warnings
# The `alt-features` job's second combination. Documented as a supported build
# ("must build standalone") and it did NOT compile until round 15.
gate "clippy proxy-only (CI alt-features)" cargo clippy --workspace --all-targets --features hydra-server/proxy -- -D warnings
# `set -o pipefail` is mandatory in these two: the transcripts feed the public-
# claims check below, but a plain `cargo test | tee` would report TEE's exit
# code and hide a failing suite — the same swallowing this gate exists to stop.
gate "test hydra-core"                 bash -c 'set -o pipefail; cargo test -p hydra-core 2>&1 | tee .acceptance/round10-gate-core.log'
gate "test hydra-server (server)"      bash -c 'set -o pipefail; cargo test -p hydra-server --features server 2>&1 | tee .acceptance/round10-gate-server.log'
# `--no-fail-fast` is NOT optional here. Without it `cargo test` stops launching
# further test binaries after the first failing one, so the suite reported
# "299 passed" while the real total is 644 — every binary after the failure would
# be silently skipped, which is the same class of lie as the exit-code swallowing
# above (a test that never runs looks a lot like a test that passes).
gate "test optional + live redis/CH"   cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse --no-fail-fast
# The arachne control-plane Rust tests. They are `#![cfg(feature = "arachne")]`, so EVERY other
# entry here (and every CI job except `live-deps`) compiles them EMPTY: "the file exists" never
# meant "it runs", and nothing enforced the difference until 2026-10-08. This entry mirrors the CI
# step of the same name (`.github/workflows/ci.yml`, first step of `live-deps`, before the drills
# because several of these tests bind real loopback ports near the drills').
gate "arachne rust tests (in-process)"  cargo test -p hydra-server --features server,cluster-redis,arachne --test arachne_alive_partition --test arachne_cluster --test arachne_leader_watch --test arachne_store --test arachne_three_nodes --test arachne_cert_fidelity --test arachne_derivation_fidelity
# `#[ignore]`d tests: no CI job ran them before round 15, and the only carrier was
# a script outside version control. This one needs no service.
gate "ignored: listener limitation"     cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse --test boot_listeners -- --ignored
# The ClickHouse end-to-end tests. They run in CI's `live-deps` job (service
# container + `init.sql` applied and asserted); locally they need the CH_URL
# exported above. Leaving them out of the local gate is how the only e2e coverage
# for retry-idempotency went unrun for months.
gate "ignored: clickhouse e2e"          cargo test -p hydra-server --features server,cluster-redis,usage-clickhouse --test clickhouse_sink --test usage_query -- --ignored
# The BINARY UNDER TEST, built once up front — the same command CI's `integration` job runs before
# its first drill (`ci.yml`: `cargo build -p hydra-server --features server --bin hydra`).
# Measured 2026-09-30: 38 entries run a drill that starts `target/debug/hydra`; 9 of them build it
# themselves and the rest run BEFORE the first build of the file (line ~140), so they inherited
# whatever binary the preceding cargo command happened to leave — feature-poor in the round-143
# incident, and simply absent on a fresh checkout. `check_gate_entries.cjs` now enforces the rule
# this entry satisfies: an entry running such a drill must build the binary itself or follow one that
# does. Drills that declare cluster features still build their own (also as in CI).
gate "build the binary under test"      cargo build -p hydra-server --features server --bin hydra
# The compose files are deliverables and nothing validated them.
# Same split as CI: the two official stacks always, the LOCAL one only when its
# (gitignored) env_file is present — `docker compose config` exits 1 for a missing
# env_file, so listing it unconditionally makes this gate red on a clean checkout.
gate "compose config"                   bash -c 'export HYDRA_ADMIN_TOKEN=dummy-admin-token HYDRA_CLUSTER_TOKEN=dummy-cluster-token HYDRA_ENCRYPTION_KEY=ZHVtbXkta2V5LWZvci12YWxpZGF0aW9uLW9ubHkAMDE=; for f in environment/docker-compose.yml environment/docker-compose.cluster.yml; do docker compose -f "$f" config -q || exit 1; done; if [ -f secure/local-test.env ]; then docker compose -f environment/docker-compose.local.yml config -q || exit 1; echo "local compose: validated"; else echo "local compose: SKIPPED (secure/local-test.env absent)"; fi'
# NOTE the name: this entry checks that 23 findings from the 2026-09-17 tenant-api review are still
# QUOTED in that plan's text (plus 3 real subprocess assertions). It does not read code, and it does
# not read THIS plan — named accordingly so nobody reads its exit=0 as "the oracle-remediation fixes
# are in place" (round 163 measured: `grep -c oracle-remediation` in that script = 0).
gate "tenant-api plan markers (text only)" python3 .acceptance/findings-disposition.py
gate "i18n"                            node scripts/check_i18n.js
gate "i18n tests"                      node --test scripts/check_i18n.test.cjs
gate "admin-ui render"                 node --test scripts/admin_ui_render.test.cjs
gate "e2e contracts"                   node scripts/check_e2e_contracts.cjs
gate "e2e contract tests"              node --test scripts/check_e2e_contracts.test.cjs
# A case appended after a suite's top-level `process.exit` is silently dead (rounds 132/137).
gate "test tails (no dead cases)"      node scripts/check_test_tails.cjs
gate "test tail checker tests"         node --test scripts/check_test_tails.test.cjs
# The gate's OWN entries: a drill must not depend on what another entry happened to build.
gate "gate entries build what they run" node scripts/check_gate_entries.cjs
gate "gate entry checker tests"        node --test scripts/check_gate_entries.test.cjs
gate "ci wiring"                       node scripts/check_ci_wiring.cjs
gate "ci wiring tests"                  node --test scripts/check_ci_wiring.test.cjs
# The shared JS masker: a backtick inside a REGEX literal used to blank 74% of a real guard
# suite and hide its exit from check_test_tails (a guard blinded by its own masker).
gate "js masker (regex desync)"        node --test scripts/js_blank.test.cjs
# The shared RUST blanker seven guards depend on: comments/literals/test items, plus a tree-wide
# invariant pin (offsets preserved, brace matching in sync, nothing blanked to nothing).
gate "rust blanker (offsets/items)"    node --test scripts/rust_blank.test.cjs
# The shared recorded-exception algebra (replace-not-merge, unrecorded, stale) that five guards use.
gate "recorded exceptions"             node --test scripts/recorded_exceptions.test.cjs
# docs/index.html is published and advertises an exact Rust test count; the two
# transcripts captured above ARE the measurement. Reverse-falsified: setting the
# page back to 796 turns this entry red (exit 1), and a missing/truncated
# transcript exits 2 rather than passing by default.
gate "public claims"                   node scripts/check_public_claims.cjs --core-log=.acceptance/round10-gate-core.log --server-log=.acceptance/round10-gate-server.log
gate "public claims tests"             node --test scripts/check_public_claims.test.cjs
# The other public claim ("no unwrap / panic / unsafe in production code"), plus
# the crate-root lint that makes "no unsafe" compiler-enforced — including the
# `[[bin]]` target, whose lint `lib.rs`'s inner attribute does not cover.
gate "source purity"                   node scripts/check_source_purity.cjs
gate "source purity tests"             node --test scripts/check_source_purity.test.cjs
# Every env var in the operator runbook's config table must be read somewhere:
# ops.md advertised a `HYDRA_LOG` alias that no code has ever read.
gate "documented env wired"            node scripts/check_documented_env.cjs
gate "documented env tests"            node --test scripts/check_documented_env.test.cjs
gate "usage backend declarations"      node scripts/check_usage_backends.cjs
gate "usage backend tests"             node --test scripts/check_usage_backends.test.cjs
# The table's DEFAULTS, not just the names: a wrong number there misleads every operator
# who sizes a grace period or a timeout from it.
gate "documented defaults"             node scripts/check_documented_defaults.cjs
gate "documented default tests"        node --test scripts/check_documented_defaults.test.cjs
# §9.1 is the alert-rule contract; a metric/label typo there is a rule that can never fire.
gate "alert expressions"               node scripts/check_alert_expressions.cjs
# One owner for fred Pool construction: production AND every test, with a non-None policy.
gate "fred pool single owner"          node scripts/check_redis_pool.cjs
gate "fred pool guard tests"           node --test scripts/check_redis_pool.test.cjs
# CLUSTER_ONLY_ENV is the single owner of "what counts as cluster wiring", and the fallback ERROR
# names the set ones — a name missing from it is a setting dropped in silence (round 193 measured
# seven). R1: every env read under src/cluster/ is listed or recorded; R2: every listed name has a
# reader somewhere.
gate "cluster-only env table"          node scripts/check_cluster_env.cjs
gate "cluster-only env guard tests"    node --test scripts/check_cluster_env.test.cjs
gate "alert expression tests"          node --test scripts/check_alert_expressions.test.cjs
# Every metric name the operator docs mention must exist in the code (three registration shapes).
gate "documented metrics"              node scripts/check_documented_metrics.cjs
gate "documented metric tests"         node --test scripts/check_documented_metrics.test.cjs
# The tenant API's external error contract: code -> HTTP, as documented for integrators.
gate "tenant error codes"              node scripts/check_tenant_error_codes.cjs
gate "tenant error code tests"         node --test scripts/check_tenant_error_codes.test.cjs
# Docker's default 10s stop grace would SIGKILL the ~25s shutdown drain (buffered
# usage lost — measured); every hydra service must set stop_grace_period >= 30s.
gate "compose stop grace"              node scripts/check_compose_grace.cjs
gate "compose grace tests"             node --test scripts/check_compose_grace.test.cjs
# The static reader the two compose guards use for files docker cannot render (in CI
# that is the local stack), so its own behaviour is pinned too.
gate "compose static reader tests"     node --test scripts/compose_static.test.cjs
# Role-aware liveness: an edge serves no admin API (/api/v1/health is 404 there), so
# probing it like a control node would mark every healthy edge unhealthy.
gate "compose healthchecks"            node scripts/check_compose_health.cjs
gate "compose health tests"            node --test scripts/check_compose_health.test.cjs
gate "integration crud (disposable)"   ./integration/run-crud-local.sh
# The probe's own helpers (it reported 21 false "violations" before they were
# pinned); the probe itself runs inside the disposable chain above.
gate "error contract tests"            python3 integration/test_error_contract.py
# HYDRA_TRUSTED_PROXIES: which address the per-IP failure budget keys on (the
# catch-all case was documented backwards until round 75 — see §2bg).
gate "trusted-proxy keying"            python3 integration/test_trusted_proxies.py
# These four drill a node whose sink is NAMED as a mock ClickHouse (`_usage_env.py`), and since
# ADR-0002 D-1 naming it is mandatory — a `server`-only binary (which is what the "build the
# binary under test" entry above leaves behind) refuses to BOOT on that variable, and the drill
# reports exit 2 CANNOT VERIFY rather than a failure. So each builds the production feature set
# itself, exactly like the CI step for the same drill. This is the round-143 lesson applied to
# FEATURES as well as to ordering: a drill must not inherit whatever binary the previous entry
# happened to leave.
gate "tenant API limits"               bash -c 'cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra >/dev/null && python3 integration/test_tenant_api_limits.py'
# The external tenant contract: routes, the 405-vs-404 rule, envelope + trace id, identity,
# idempotency, parameter validation.
gate "tenant API contract"             bash -c 'cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra >/dev/null && python3 integration/test_tenant_api_contract.py'
# §7 boundaries: suspended tenant (recovery path), prefix invalidation silent no-op,
# no tenant_id query parameter, empty window, and the two different error envelopes.
gate "tenant API boundaries"           bash -c 'cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra >/dev/null && python3 integration/test_tenant_api_boundaries.py'
# The SSE path: incremental delivery, the stream idle bound + its §9.1 metric, mid-stream
# death + its metric, and metering of streamed requests (the metering leg is why it needs the
# feature above).
gate "streaming path (SSE)"            bash -c 'cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra >/dev/null && python3 integration/test_streaming_path.py'
# The breaker's documented workflow: trip -> dead-set + skip counter -> manual reset -> recover.
gate "circuit breaker lifecycle"       python3 integration/test_breaker_lifecycle.py
# limit_roles enforcement: count/token ceilings, the documented Retry-After, scoping,
# soft-disable, and the D-11 evidence.
gate "limit_roles enforcement"         python3 integration/test_limit_roles_enforcement.py
# The data-plane catalog: public read, no external auth for a presented key, whitelist,
# online filter, and the two narrowing mechanisms.
gate "model catalog (/v1/models)"      python3 integration/test_model_catalog.py
# Sub-tenant / operator key-prefix steering on the chat path, incl. "delete != revoke".
gate "sub-tenant steering"             python3 integration/test_sub_tenant_steering.py
# The ClickHouse sink's WIRE format, measured against a MOCK ClickHouse (no live database):
# userinfo -> Basic auth, verbatim query-string passthrough, the row as a MASKED JSONEachRow
# body (not param_* — that is the reader's form), a path in the URL trimmed rather than glued
# to the port (the 2026-09-30 fix), and a 500 that never breaks the client. The sink needs the
# `usage-clickhouse` feature compiled in, so build that set first.
gate "clickhouse sink wire format"     bash -c 'cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra >/dev/null && python3 integration/test_clickhouse_sink_wire.py'
# The tenant API's usage READER over ClickHouse (the same mock-only property): §5.3's
# string-encoded counters, the bound `param_*` window, grouped reads as a second query, the
# empty-window `""` -> `as_of: null`, and every unreadable-store shape as an attributed 503
# rather than a zeroed 200.
gate "tenant usage read over CH"       bash -c 'cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra >/dev/null && python3 integration/test_usage_query_wire.py'
# A dead route (never-completing connect) must FAIL OVER, not surface as 502: the connect
# bound turns it into a connect error; the silent-upstream contrast keeps the 502 policy.
gate "upstream connect bound"          python3 integration/test_upstream_connect_bound.py
# What a dead route costs with the SHIPPED defaults (10s/attempt) and that the breaker stops
# it after 5 failures, plus the manual reset lever and the bounded single-provider failure.
gate "dead route cost + breaker"        python3 integration/test_dead_route_cost.py
# The probe half: automatic revival of a recovered provider (~10s), the measured 404 blind
# spot (revive a broken provider ⇒ flap), and 5xx must NOT revive.
gate "breaker probe revival"           python3 integration/test_breaker_probe_revival.py
# The data-plane body contract: cap boundary, the 408 total deadline (length-delimited AND
# chunked), truncated bodies failing closed (a valid-but-short body used to be forwarded), and
# the two other 1 MiB caps with their own codes.
gate "request body contract"           python3 integration/test_request_body_contract.py
# SIGTERM with a request in flight: complete 200, drain+5s exit, the drain as a real bound,
# no listener accepting during the drain, and the buffered usage row persisted.
gate "shutdown drain"                  python3 integration/test_shutdown_drain.py
# The master-key rotation flow end to end (one-shot reseal, report/exit codes, the retired
# key refused, a failed reseal leaving rows untouched) + the switch's own strict vocabulary.
gate "master-key rotation"             python3 integration/test_key_rotation_live.py
# Master-key SOURCES: inline base64 vs the raw key FILE (K8s secret form), their equivalence
# in both directions, the documented file precedence, and every fail-closed mode.
gate "master key sources"              python3 integration/test_master_key_sources.py
# The auth hop against a dead route: bounded 503, no forwarding, attribution, NOT cached,
# refused-port contrast, immediate recovery, and the unselectable fail-open mode.
gate "auth hop (dead route)"           python3 integration/test_auth_hop_failmode.py
# Usage-row loss accounting: the under-billing counter, its reasons, the measured threshold,
# the per-drop trace id, and that recovery stops it.
gate "usage drop accounting"           python3 integration/test_usage_drop_accounting.py
# Who DRAINS an oversized upload: the tenant API answers at the cap (rest left on the wire),
# the data plane drains first; and neither leaves a broken connection behind.
gate "body cap draining"               python3 integration/test_body_cap_drain.py
# A client that resets mid-stream: the upstream is cancelled, the usage is kept, the provider
# is not blamed, and the metric/breaker disagreement is pinned.
gate "client disconnect"               python3 integration/test_client_disconnect.py
# The listener/SNI alert rows evaluated live in four configurations, incl. a real HTTPS
# request with SNI and the no-false-alarm direction.
gate "listener signals"                python3 integration/test_listener_signals.py
# A failing reload (bad row written past the write boundary): gauge, 2xx-with-no-effect trap,
# per-tenant flag, and recovery.
gate "snapshot stale"                  python3 integration/test_snapshot_stale.py
# The two-layer auth verdict cache on a real cluster + Redis (L1, the shared L2, fleet clear).
# `arachne` is required, as in CI: the two nodes are wired with `HYDRA_CLUSTER_PEERS`, and a build
# without the control plane REFUSES to boot on that variable (ADR-0001) — the reason this entry sat
# at exit 2 CANNOT VERIFY while its CI step, which already passed `arachne`, was green.
gate "auth cache layers (L1+L2)"       bash -c 'cargo build -p hydra-server --features server,cluster-redis,arachne,usage-clickhouse --bin hydra >/dev/null && HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_auth_cache_layers.py' 
# What a replica must still know (enabled limit roles, token hashes, sub-tenant routes) and
# what `matching_key` really compares against (the MASKED key — D-15 evidence).
#
# These two entries and `tenant usage read over CH` BUILD THE BINARY THEMSELVES, like the CI steps
# for the same drills. Measured 2026-09-30: they inherited the binary built by the entry above them,
# and a `check_public_claims --measure` running in parallel (it is `cargo test -p hydra-server
# --features server`, which RELINKS `target/debug/hydra` WITHOUT cluster-redis/usage-clickhouse)
# left them testing a feature-poor binary — both reported `CANNOT VERIFY … the binary lacks the
# cluster features` and the gate went RED for a reason unrelated to the code. A drill must never
# depend on what another entry happened to build.
# "replica fidelity" RETIRED 2026-10-05: it drove the edge role and the snapshot channel, both
# deleted by ADR-0001 T4.1 (it could only crash with `UnboundLocalError: standby`). Its coverage moved
# to tests/arachne_derivation_fidelity.rs, tests/arachne_cert_fidelity.rs,
# tests/arachne_three_nodes.rs + the acceptance drill's gate 4, and the D-15 mask semantics are pinned
# by integration/test_limit_roles_enforcement.py. See the CI step's note in .github/workflows/ci.yml.
# The tenant-write drill documents the build precondition now, so the guard's judged count is
# unchanged (4).
# Cluster rate limits: the SHARED count + token windows across two real nodes, the
# hard-coded fail-open under a cut Redis (with its metric), and RECOVERY after the bus
# returns — the leg that caught the missing fred reconnect policy (P1, 2026-09-30).
# `arachne` for the same reason as the auth-cache entry above (the nodes carry
# `HYDRA_CLUSTER_PEERS`), and `leader + edge` in that older sentence is retired wording.
gate "cluster rate limits (shared+FO)"  bash -c 'cargo build -p hydra-server --features server,cluster-redis,arachne,usage-clickhouse --bin hydra >/dev/null && HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_cluster_limits.py'
gate "tenant write publish failure"     bash -c 'cargo build -p hydra-server --features server,cluster-redis,arachne,usage-clickhouse --bin hydra >/dev/null && HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_tenant_write_publish_failure.py'
gate "data plane under failover (acc 2)" bash -c 'cargo build -p hydra-server --features server,cluster-redis,arachne,usage-clickhouse --bin hydra >/dev/null && HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_data_plane_failover_load.py'
gate "admission queue"                python3 integration/test_admission_queue.py
# ops.md §1.4 (backup & restore): the naive `cp` of a live WAL database loses rows,
# `VACUUM INTO` is complete, a stale -wal beside a REPLACED db refuses to boot, and a
# different master key is rejected. Never ran before round 79.
gate "backup / restore runbook"        python3 integration/test_backup_restore.py
gate "tenant SDK vs live node"        python3 integration/test_sdk_live.py
gate "integration proxy e2e"           bash -c 'MOCK_LLM_PORT=19190 MOCK_AUTH_PORT=19191 HYDRA_PROXY_PORT=18092 HYDRA_ADMIN_PORT=18093 python3 integration/e2e_proxy_test.py'
# The documented zero-downtime upgrade (ops.md §2): the handover must keep the data
# port served across the switch (0 refused connections), and `-u` is what makes that
# possible — both were broken until round 68.
gate "upgrade handover"                bash scripts/handover.test.sh
# ops.md §3.1: a PUT makes the new tenant cert live with no restart; SNI picks per
# tenant; an already-open session keeps the cert it negotiated.
gate "cert hot-reload + SNI"           python3 integration/test_cert_reload.py
# ARACHNE CONTROL PLANE (acceptance 1/3/4/5) — the CI step that REPLACED `test_cluster_ha.py`.
# The old entry here ran a drill ADR-0001 deleted (`python3: can't open file
# 'integration/test_cluster_ha.py'`), so for as long as this script kept it, the gate could never
# be green and the Arachne drill ran in CI ONLY. Three real hydra processes, all raft members.
gate "arachne control plane (CI live-deps)" bash -c 'cargo build -p hydra-server --features server,cluster-redis,arachne,usage-clickhouse --bin hydra >/dev/null && HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_arachne_control_plane.py'
# Startup knobs on the REAL binary (needs the cluster feature set + the 6380 test Redis):
# a misspelt HYDRA_REDIS_MODE must refuse to start naming the value (round 191 — it used to
# boot as `single`), and cluster wiring that a non-cluster role will NOT use must be named
# on ALL paths that reach it, including `HYDRA_ROLE` unset (round 192 — that path was silent).
# `arachne` is part of the set since ADR-0001: the drill wires a cluster (`HYDRA_CLUSTER_PEERS`)
# and a build without the control plane REFUSES to boot on that variable — which is exactly how
# this entry went red while CI's copy of the same step already passed `arachne`.
gate "startup knobs (refuse/name)"     bash -c 'cargo build -p hydra-server --features server,cluster-redis,arachne,usage-clickhouse --bin hydra >/dev/null && HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_startup_knobs.py'
# The three SDK `--cluster` entries were RETIRED 2026-10-07, together with the CI steps they
# mirrored: they asserted the pre-ADR-0001 world (`applied` + `event_id`, `202 pending`, "the
# edge") while starting two PLAIN single-node instances, which correctly answer `single_node`.
# Coverage: the live fleet report is `test_auth_cache_layers.py` (above), the SDK's own state
# handling is the three mock SDK suites in the `sdks` CI job, and each SDK against a real node is
# the pair of entries below.
# The CLI against a real node (its own suite is mock-only). Rebuilds dist/ first so the
# drill exercises the CURRENT source; it caught two documented-but-broken commands.
# The tenant TypeScript SDK against a real node (its own suite is mock-only); rebuild
# dist/ first so the drill exercises the CURRENT source.
# The tenant Go SDK against a real node (its own suite is `go test` against mocks); the
# drill compiles a driver against the local SDK and rebuilds nothing in the repo.
gate "tenant Go SDK vs live node"      python3 integration/test_sdk_go_live.py
gate "tenant TS SDK vs live node"      bash -c 'cd tools/hydra-ts && npm run build >/dev/null && cd ../.. && python3 integration/test_sdk_ts_live.py'
gate "admin CLI vs live node"          bash -c 'cd tools/hydra-cli && npm run build >/dev/null && cd ../.. && python3 integration/test_cli_live.py'
gate "cli tests (hydra-admin)"         env npm_config_cache="$PWD/.acceptance/tmp-npm-cache" bash -c "cd tools/hydra-cli && npm test"
# LAST entry: a CODE change after the first entry means the earlier entries judged a revision that
# no longer exists (exit 1); a docs-only change is reported as a note (exit 0).
gate "tree manifest (verify)"           node scripts/tree_manifest.cjs --check .acceptance/gate-manifest.txt

# The two in-repo scripts that NOTHING automated ran (measured 2026-10-01: neither the gate nor
# ci.yml invoked `scripts/e2e-local.sh` or `scripts/load_test.sh`). Both are wired here at the END so
# neither can disturb the cluster drills above:
#   * the browser suite runs against the binary the gate built (E2E_SKIP_BUILD=1 — see the script's
#     header: its own build is `--features server` and would RELINK a feature-poor binary, the
#     round-143 incident), on explicit ports so it cannot collide with `run-crud-local.sh`, which
#     defaults to the same 18080/18081;
#   * the load harness runs only its deterministic self-check (SELFTEST_ONLY=1): the measurement half
#     needs a live instance plus a concurrent echo upstream and stays a manual step.
gate "load harness selftest"           bash -c 'SELFTEST_ONLY=1 bash scripts/load_test.sh'
gate "admin-ui e2e (browser, 18180)"   bash -c 'E2E_SKIP_BUILD=1 E2E_DATA_PORT=18180 E2E_ADMIN_PORT=18181 bash scripts/e2e-local.sh'
echo
echo "==================== GATE SUMMARY ===================="
overall=0
{
  echo
  echo "==================== GATE SUMMARY ===================="
} >> "$LOG"
for i in "${!NAMES[@]}"; do
  printf "%-38s exit=%s\n" "${NAMES[$i]}" "${RCS[$i]}"
  printf "%-38s exit=%s\n" "${NAMES[$i]}" "${RCS[$i]}" >> "$LOG"
  if [ "${RCS[$i]}" != "0" ]; then
    overall=1
  fi
done
VERDICT="OVERALL=$([ "$overall" = 0 ] && echo GREEN || echo RED)"
echo "$VERDICT"
# The SAME line ends the log, so "did this run finish, and how?" is answerable from the log alone.
{
  echo "$VERDICT"
  echo "entries=${#NAMES[@]}"
  echo "GATE COMPLETE"
} >> "$LOG"
echo "log: $LOG"
# The verdict must be RETURNED, not merely printed. The first version of this
# script ended with an `echo`, so it exited 0 even when the summary said RED —
# a gate wired into anything (CI, a pre-commit hook, `&&`) would have passed
# forever. That is the exact failure mode this whole effort exists to remove.
exit "$overall"
