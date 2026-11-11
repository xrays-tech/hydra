# Final comprehensive remediation — development detail & plan

- **Status**: approved by @oracle (ora-19); executed in waves W1–W4; final gate REVISE P1-1 (F4 live tests) → fixed → final-approved
- **Date**: 2026-11-10
- **Baseline**: `3926e2e`; PG 586/0/3, MySQL 584/0/3, CI hard-gate 476 + core 228, claims 704
- **Authority**: ADR-0004, `dev-docs/aegis/plans/2026-11-10-remaining-hardening.md`, `...-remaining-optional.md`, `dev-docs/ops.md`
- **Goal**: close every remaining *actionable* problem in one batch, then run one global acceptance. Accepted/no-code trade-offs are listed explicitly so “all problems” has a complete ledger.

## 1. Problem inventory and disposition

| # | Problem | Severity | Disposition |
|---|---|---|---|
| F1 | `config_bus` is one detached task; a panic stops pub/sub + poll with no auto-restart | P1 reliability | fix |
| F2 | `admin_audit`: no retention, admin-only, `POST /metrics` audited (method quirk) | P1 ops | fix |
| F3 | DB TLS / Redis auth are docs-only; no runtime enforcement | P1 security ops | fix (opt-in) |
| F4 | no automatic retry for retryable DB lock/deadlock errors; session guardrails hard-coded | P2 robustness | fix |
| F5 | PG test DBs leak by default; local accumulation | P2 test hygiene | fix (tooling) |
| F6 | `main.rs` 1425 / `proxy.rs` 2180; `bootstrap`/`request_filter` decomposition | P2 maintainability | fix |
| F7 | `check_ci_wiring` / `check_gate_entries` duplicate CI parser | P2 tooling | fix or explicit accept |
| F8 | audit/retention/tenant docs; stale historical sections | P2 docs | fix |
| A1 | per-entity invalidation / outbox / external config cache | scale trade-off | accepted (metrics-gated) |
| A2 | `rand` 0.8/0.9 duplicate cannot be removed (transitive pins) | dependency | accepted |
| A3 | no compile-time SQL checking (runtime queries + dual dialect) | accepted risk | accepted (CRUD suite is guard) |
| A4 | single admin token / no RBAC | security feature | accepted (future) |
| A5 | audit SIEM export / tamper-evidence | ops feature | accepted (future) |
| A6 | MySQL integration drill parity / TDengine live test | test coverage | accepted (pre-existing) |

## 2. F1 — config_bus supervisor

- Split `config_bus` into two independently supervised tasks: `pubsub_consumer` and `poll_loop`.
- Add a `supervise(name, factory)` loop: `JoinSet`/`spawn`, catch `JoinError` (panic), log, increment `hydra_config_watch_restarts_total{task="pubsub|poll"}`, restart with 1s backoff; expose the gauge/metric so a dead task is alertable.
- Both tasks update `hydra_config_watch_last_ok_unixtime`; a panic in one does not stop the other.
- Restart factory rules: on restart, re-run `init()`/`subscribe()`, take a **new** `message_rx()`, and do **not** re-spawn `manage_subscriptions()` (one manager per client, ever); restart sequentially after awaiting the dead handle so at most one consumer owns a receiver. A broadcast receiver misses messages during the gap — the poll backstop is the documented correctness bound.
- Minimum test: unit-test `supervise(name, factory)` generically (factory panics once, succeeds on run 2; assert restart counter and revived effect via deadline-poll, never a single sleep). No Redis required.
- Files: `cluster/config_bus.rs`, `admin/metrics.rs`, focused tests. Risk: low/medium.

## 3. F2 — admin audit retention + tenant audit + method strictness

- Migration `0003_admin_audit_tenant.sql` in both dialects: add nullable `tenant_id VARCHAR(255)` (parity guard).
- Tenant audit: wrap at the single `handlers::write` call after the gate; record `actor=<AUTHENTICATED tenant id>` (never the URL claim), `action`, `method`, `path`, `entity_type`, `request_id`, `tenant_id`. Pre-gate refusals (401/403/429) are NOT audited (no actor). Best-effort/log-once/metric, never fails a mutation; no bodies.
- Retention: `HYDRA_ADMIN_AUDIT_RETENTION_DAYS` (default 90; `0` = keep forever) + a daily cleanup task deleting rows older than the cutoff (plain DELETE, no `begin_write`); metric `hydra_admin_audit_deleted_total`; docs.
- Method strictness: `/metrics` accepts GET/HEAD only (405 otherwise) in the `route_inner` branch; exclude `/metrics` from the audit wrapper. HEAD returns the GET `Content-Length` with an **empty body** (do not embed the exposition body) — this touches `admin/handlers.rs::metrics_endpoint` as well.
- Tests: tenant mutation writes tenant_id; old-row retention delete; POST /metrics 405; migration parity; full suites.
- Files: migrations 0003, `db/audit.rs`, `admin/mod.rs`, `admin/handlers.rs`, `tenant_api/mod.rs`, `admin/metrics.rs`, tests, docs.

## 4. F3 — runtime security enforcement (opt-in)

- `HYDRA_DB_REQUIRE_TLS` (default `0`): when `1`, parse `HYDRA_DB_URL` and require PostgreSQL `sslmode=require|verify-ca|verify-full` or MySQL `ssl-mode`/`sslmode` (both keys accepted, case-insensitive) = `REQUIRED|VERIFY_CA|VERIFY_IDENTITY`; refuse startup otherwise with a precise message. Pure predicate + unit tests; docs recommend `1` in production.
- `HYDRA_REDIS_REQUIRE_AUTH` (default `0`): when `1`, require a non-empty password component in `HYDRA_REDIS_URL`; `rediss://` TLS is not conflated with auth. (ACL/publish restriction is server-side policy; the URL check is the enforceable part.)
- Files: `db/pool.rs` (URL predicate), `redis/mod.rs` (URL predicate), `main.rs` (boot checks), tests, docs. Risk: low.

## 5. F4 — write retry and tunable guardrails

- `begin_write` retries lock acquisition on `is_retryable` ∪ {PG `55P03` lock_not_available} up to 3 times with bounded jitter (the `55P03` case is the primary PG contention path; do not change the global classifier). A mutation that fails after acquisition still surfaces 503 `storage_busy`. Worst-case added write latency ≈ 3 × lock_timeout (~15s); document it. Do not wrap arbitrary mutation bodies.
- Env-tunable session guardrails with safe clamps and a `LOCK_TIMEOUT ≤ STATEMENT_TIMEOUT` sanity guard: `HYDRA_DB_LOCK_TIMEOUT_MS` (default 5000), `HYDRA_DB_STATEMENT_TIMEOUT_MS` (default 60000), `HYDRA_DB_IDLE_TX_TIMEOUT_MS` (default 60000), `HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS` (default 10); docs/guards.
- Generic MySQL 1615 retry remains narrow (documented); do not expand unless a statement trips it.
- Files: `db/pool.rs`, tests, docs.

## 6. F5 — test DB hygiene

- Add `scripts/clean-test-dbs.cjs` that shells out (dependency-free `child_process`) to `psql`/`mysql` CLIs to drop `hydra_test_%` / `hydra_tmpl_%` databases from `HYDRA_TEST_DATABASE_URL`, using session-terminate/`FORCE` parity with `db/testing.rs`. If CLIs are unavailable, fail with setup instructions (no silent skip). A `hydra-cli` subcommand is an acceptable alternative.
- Keep `HYDRA_TEST_DROP_DBS=1`; optionally default it to `1` in CI-only cleanup steps, not in the parallel test run. No per-test lifecycle rewrite (accepted cost).

## 7. F6 — file decomposition

- `main.rs` → `src/boot/{mod,env,database,state,fanout,listeners}.rs`: pure env parsers, DB boot, state assembly, Redis fanout wiring, listener setup. Preserve the Redis-before-store/watcher-after-load order, cfg gates, and public behavior. **Approved.**
- `proxy.rs::request_filter` phase split: **deferred** to the accepted ledger (hottest path, 2,180 lines, pure structure, worst value/risk in a one-shot batch).
- Acceptance: full suites + drills; net reduction in the two files; clippy/fmt.
- Risk: high churn; if the plan gate judges it too risky, defer explicitly.

## 8. F7 — CI guard single-source

- Extract the workflow/step parsing into `scripts/lib/ci_steps.cjs` consumed by `check_ci_wiring.cjs` and `check_gate_entries.cjs`; keep their `.test.cjs` suites green. If the refactor threatens the guards, record the duplication as accepted instead.
- Acceptance: both guards green on the real tree.

## 9. F8 — docs and ledger

- Document F1–F7 behavior, retention/tenant audit, TLS/Redis auth flags, retry semantics, and the test-DB cleanup script.
- Clean remaining live-false design.md/HANDOFF sections if any; mark A1–A6 as accepted/deferred with triggers.
- Run all docs/claims guards.

### 9.1 Execution outcomes (W1–W4)

| Item | Outcome | Evidence / files |
|---|---|---|
| F1 | **Done.** `config_bus` runs two independently supervised tasks (`pubsub_consumer`, `poll_loop`); `supervise(name, factory)` catches `JoinError`, logs, increments `hydra_config_watch_restarts_total{task="pubsub\|poll"}` and restarts after a fixed 1 s backoff. A panic in one leaves the other running; the poll loop remains the correctness backstop for a missed broadcast. | `crates/hydra-server/src/cluster/config_bus.rs`, `admin/metrics.rs` |
| F2 | **Done.** Migration `0003_admin_audit_tenant` in both dialects adds nullable `tenant_id` (parity guard green). Tenant writes are audited after the token gate around `handlers::write` with the AUTHENTICATED tenant id (never the URL claim); pre-gate refusals are not audited. Retention `HYDRA_ADMIN_AUDIT_RETENTION_DAYS` (default 90, `0` forever) runs a daily plain `DELETE`, counted on `hydra_admin_audit_deleted_total`. `/metrics` accepts GET/HEAD only (405 otherwise), is excluded from the audit wrapper, and HEAD returns the GET `Content-Length` with an empty body. | migrations `postgres/0003`, `mysql/0003`; `db/audit.rs`, `admin/handlers.rs`, `admin/mod.rs`, `tenant_api/mod.rs`, tests |
| F3 | **Done (opt-in).** `HYDRA_DB_REQUIRE_TLS` refuses startup unless the DB URL carries a verifying TLS mode; `HYDRA_REDIS_REQUIRE_AUTH` refuses unless `HYDRA_REDIS_URL` has a non-empty password. Both default off; docs recommend `1` in production. | `boot/database.rs`, `boot/mod.rs`, `db/pool.rs`, `redis/mod.rs`, tests |
| F4 | **Done.** `begin_write` retries lock acquisition on `is_retryable ∪ {PG 55P03}` up to 3 attempts with bounded jitter; only acquisition is retried, never a mutation body. Session guardrails are env-tunable with clamps and the `LOCK_TIMEOUT ≤ STATEMENT_TIMEOUT` sanity rule: `HYDRA_DB_LOCK_TIMEOUT_MS` (5000), `HYDRA_DB_STATEMENT_TIMEOUT_MS` (60000), `HYDRA_DB_IDLE_TX_TIMEOUT_MS` (60000), `HYDRA_DB_LOCK_WAIT_TIMEOUT_SECS` (10). Worst-case added write latency ≈ `3 × lock_timeout` (~15 s) before `503 storage_busy`. A caller-guardrails seam plus an attempt probe make the loop testable; live tests `begin_write_retries_a_real_pg_lock_timeout_then_succeeds` (real PG `55P03`) and `begin_write_retries_a_real_mysql_lock_wait_timeout_then_succeeds` (real MySQL `1205`) assert ≥2 attempts and that the write lands. | `db/pool.rs`, `crates/hydra-server/tests/db_write_lock.rs`, `dev-docs/ops.md`, `dev-docs/deployment.md` |
| F5 | **Done.** `scripts/clean-test-dbs.cjs` drops `hydra_test_%` / `hydra_tmpl_%` databases via the `psql`/`mysql` CLIs (session-terminate/`FORCE` parity with `db/testing.rs`), reading `HYDRA_TEST_DATABASE_URL`; if a CLI is missing it fails with setup instructions instead of skipping silently. `HYDRA_TEST_DROP_DBS=1` is unchanged. | `scripts/clean-test-dbs.cjs`, `scripts/clean-test-dbs.test.cjs` |
| F6 | **Done.** `main.rs` is now a 20-line entry point; bootstrap lives in `src/boot/{mod,env,database,state,fanout,listeners}.rs`, preserving Redis-before-store and watcher-after-load ordering, cfg gates and public behavior. `proxy.rs::request_filter` remains deferred (A7). | `crates/hydra-server/src/main.rs`, `src/boot/*` |
| F7 | **Done.** Workflow/step parsing is single-sourced in `scripts/lib/ci_steps.cjs`, consumed by both `check_ci_wiring.cjs` and `check_gate_entries.cjs`; both suites stay green. | `scripts/lib/ci_steps.cjs`, `scripts/check_ci_wiring.cjs`, `scripts/check_gate_entries.cjs` |
| F8 | **Done.** Docs cover retention/tenant audit, `/metrics` methods, TLS/Redis auth flags, retry semantics and the test-DB cleanup script; the four F4 knobs have first-column rows in `ops.md` and `deployment.md`; prose-only test knobs are recorded in the env guard. | `dev-docs/ops.md`, `dev-docs/deployment.md`, `scripts/check_documented_env.cjs` |

**F4 error-mapping decision (narrow):** PostgreSQL `55P03 lock_not_available` is added to
`is_lock_contention` (the error→HTTP owner) so it is answered as a transient **`503
storage_busy`**, never a 500 — but it stays OUT of the global `is_retryable` classifier.
Outside `begin_write`, `55P03` can also mean a statement/tooling misuse that retrying would
not fix, so widening the global classifier would destabilise every other call site. This is
deliberately a one-call-site retry (`begin_write` acquisition) plus one mapping predicate.

## 10. Accepted / no-code (ledger)

- **A1** per-entity invalidation/outbox/external cache: trigger = `hydra_config_reload_duration_seconds`/`hydra_config_rows_loaded` exceeding the write-rate budget. H1 coalescing is the current mitigation.
- **A2** rand duplicate: 0.8.7 transitively pinned by pingora/fred/sqlx; 0.9.5 via cf-rustracing; no direct-pin change removes it. Evidence recorded.
- **A3** compile-time SQL: runtime `query_as` + dual-backend CRUD/drill suite is the guard; `dialect::sql` guard covers placeholder rewriting.
- **A4** RBAC/multi-admin token: future feature; current single `HYDRA_ADMIN_TOKEN` + network restrictions.
- **A5** audit SIEM export/tamper-evidence: future; retention is implemented in F2.
- **A6** MySQL drill parity/TDengine live test: pre-existing coverage limits, not regressions.
- **A7** `proxy.rs::request_filter` phase split: deferred (pure structure, hottest path, no behavior change); revisit when a proxy feature forces a touch.

## 11. Waves, ownership, gates

| Wave | Content | Parallelism | Acceptance |
|---|---|---|---|
| W0 | this plan | — | **Oracle plan approval** |
| W1 | F1 (config_bus) + F4 (db/pool) | disjoint files | focused + full suites |
| W2 | F2 (audit) + F3 (security runtime) | F2 handlers/tenant; F3 boot/db/redis | migrations/tests/docs |
| W3 | F6 (main/proxy) + F5/F7 (tooling) | F6 code; F5/F7 scripts | full suites + guards |
| W4 | F8 docs + accepted ledger | — | docs/claims guards |
| W5 | **global acceptance** | — | full PG/MySQL, clippy, scripts, migration parity, claims, cluster/fanout/outage |
| W6 | **Oracle final sign-off** | — | final gate REVISE P1-1 → live tests added → **APPROVED** |

### 9.2 Final acceptance (measured)

- PG hard gate: 503 passed / 0 failed; MySQL: 500 / 0; core: 228 / 0; total public claims 731 (228 core + 503 server).
- clippy `-D warnings`: db-postgres, db-mysql, cluster-redis, usage-clickhouse; `cargo fmt --check`/`git diff --check` clean.
- Script suites 333/333; all guards green; public claims verified from measured logs; migration parity 3/3.
- Drills PASS: `test_shared_db_cluster.py`, `test_shared_db_random_fanout.py`, `test_shared_db_outage.py`.
- Oracle final verdict: P1-1 fixed with live PG/MySQL retry tests; P2-1/P2-2/P2-3 folded in; **APPROVED — commit**.

## 12. Global acceptance matrix

- `cargo test -p hydra-core`; `cargo test -p hydra-server --features server,db-postgres` (CI hard-gate count; update public claims).
- Full PG/MySQL cluster recipes; clippy both; fmt.
- `node --test scripts/*.test.cjs`; SQL/migration/CI/error/test-tail/purity/docs/metrics/alert/claims guards.
- Cluster/fanout/DB-outage drills on PG; MySQL full suite.
- New focused tests: supervisor restart (generic factory), tenant audit row + pre-gate refusal not audited, retention delete + `0=off`, DB/Redis URL predicates (both MySQL `ssl-mode`/`sslmode` spellings), begin_write retry for PG `55P03` and MySQL `1205`, `/metrics` POST 405 + not audited, HEAD empty body, F5 session-terminate parity.
- `git diff --check`.

## 13. Risks / decisions

1. **F6 churn**: main/proxy splits are pure structure; if W3 risks the green baseline, defer explicitly rather than half-split.
2. **F4 retry scope**: retry only lock acquisition inside `begin_write`; do not replay arbitrary mutation bodies (avoids double side effects).
3. **F3 opt-in flags**: default off for compatibility; production docs recommend on. The code cannot detect “production”.
4. **F2 retention**: default 90 days; `0` disables; cleanup is a plain DELETE, not coupled to config writes.
5. **F1 supervisor**: restart loops must be bounded/backoff; a permanently panicking task must not hot-spin.
6. **F7**: if the single-parser refactor is risky, accept the duplication with a ledger note.
