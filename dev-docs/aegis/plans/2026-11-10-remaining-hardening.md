# Remaining hardening — development detail & plan

- **Status**: proposed; requires @oracle plan gate before implementation
- **Date**: 2026-11-10
- **Baseline**: `9a2118b` (`d5c73f2` shared-DB rewrite + follow-ups)
- **Owner**: scheduler/orchestrator, specialist lanes per phase
- **Authority**: ADR-0004, `dev-docs/cluster.md`, `.slim/deepwork/shared-db-no-raft.md`
- **Out of scope**: raft/Arachne resurrection, SQLite, per-entity cache invalidation, transactional outbox, external config cache

## 1. Baseline and problem statement

Current state after the shared-DB/no-raft rewrite and the review follow-ups:

- PostgreSQL/MySQL compile-time backends; shared DB is the sole config authority; no leader.
- `hydra_lock` + `FOR UPDATE` write serialization; `config_revision` bump in the same transaction.
- `ArcSwap<ConfigData>` cache; Redis `hydra:config:changed` pub/sub + revision-probe poll fallback.
- `ConfigStore` reload API is `reload_local` / `reload_external` / `reload_with(&ReloadGuard)`.
- DB outage now returns 503 `database_unavailable`; config-bus `Lagged` reconciles immediately; `Closed` backs off.
- PG reads use REPEATABLE READ; write transactions bound `lock_timeout`; session guardrails installed.
- PG test provisioning uses a template DB; default cleanup leaks per-test DBs (`HYDRA_TEST_DROP_DBS=1` opt-in).
- CI has per-job timeouts; `sqlx` default features are off; migration parity/build tracking exists.
- Reload duration/rows metrics exist; MySQL `delete_tenant` prefers bound + `.persistent(false)`, retries 1615 via a collation-preserving text fallback.

Verified baseline (2026-11-10):

- `cargo test -p hydra-core`: 228 passed.
- `cargo test -p hydra-server --features server,db-postgres`: 465 passed, 0 failed, 1 ignored (CI hard-gate count).
- Full cluster/usage recipe: PG 569, MySQL 567 passed, 0 failed, 3 ignored.
- clippy `-D warnings` both backends; scripts 313; SQL/migration/CI/docs/claims guards green.
- 3-node cluster, DB-outage, and concurrent random-fanout drills pass.

Remaining risk is concentrated in: **burst reload cost**, **force-reload revision skew**, **Redis health visibility**, a few **DB edge cases**, **handler repetition**, and **security/ops documentation**. None is a known correctness blocker; the plan below is ordered by risk/effort.

## 2. Workstreams

| # | Workstream | Priority | Risk | Effort |
|---|---|---|---|---|
| H1 | Reload coalescing + force-revision consistency | P1 | Medium | 1–1.5d |
| H2 | Redis health in cluster status + observability | P1 | Low | 0.5d |
| H3 | DB edge correctness (tenant update 0-row, api-key lookup, duplicate_external_id) | P1 | Low–Medium | 1d |
| H4 | Handler `WriteCtx` / body-validation scaffolding | P2 | Medium–High churn | 1–2d |
| H5 | Security/ops documentation (DB TLS, Redis ACL, audit-table design) | P2 | Low | 0.5d |
| H6 | Optional dependency/CI hygiene (rand, guard complexity) | P3 | Low | deferred |

## 3. H1 — Reload coalescing + force-revision consistency

### Problem
Every external pub/sub message currently calls `reload_external`; a burst of writes can cause N full `ConfigData` rebuilds per node (`O(N × rows)`). `hydra_config_reload_duration_seconds`/`rows_loaded` now make this measurable. Separately, `reload_impl` uses `new_version = db_version.max(local + 1)`; after a revision rewind + force reload, the local watermark can permanently exceed the shared DB revision, making the version gate and alerts confusing.

### Design
1. **Trailing debounce in `cluster/config_bus.rs`** (external path only):
   - On a well-formed message whose version > `store.version()`, mark a pending reload and reset a quiet timer (50 ms).
   - Cap the maximum delay (250 ms) so a continuous stream cannot starve convergence.
   - **Clear the pending flag BEFORE starting the reload**; if another message arrives while that reload is in flight, re-arm the timer. Otherwise the final notification can be swallowed and convergence waits for the poll.
   - On timer fire: `has_newer_revision()` then one `reload_external(false, ReloadSource::PubSub)`.
   - Keep `record_config_watch_ok()` per well-formed message; liveness must not depend on the debounce timer.
   - On reload error, re-arm once or explicitly rely on the poll backstop (log it); do not leave a pending flag stuck.
   - Poll fallback and local writes are unchanged; external reloads still never publish.
   - Add `hydra_config_reload_coalesced_total` (bounded counter) and a focused test that a burst of N notifications produces one reload (or bounded coalescing) and still converges; poll delta remains 0 with a 120 s poll.
2. **Force-revision consistency in `store.rs`**:
   - Force reload remains: if `db_version <= local`, bump the shared revision, then re-read.
   - After the bump/re-read, **adopt `db_version` as the local version** instead of `max(db_version, local+1)`. If the DB was rewound, `?force=1` resets the node to the shared authority rather than preserving a private higher watermark.
   - Log/metric when `local > db_version` before the force reset (e.g. `hydra_config_version_skew` gauge or a warning); document the recovery semantics in `dev-docs/cluster.md`/`ops.md`.
   - **Peer note:** force resets only the receiving node; peers whose local watermark is still above the rewound DB revision remain blind until the DB revision passes them. `ops.md` must say per-node `?force=1` is the recovery runbook, not a fleet-wide one-shot.
   - Add a test: local high, DB revision rewound, force reload → local == DB revision and content is served; a subsequent normal reload is a no-op.
3. **Docs/ops**: document the coalescing window, the force-rewind recovery, and the metrics. No alert on coalescing; duration/rows remain the scaling signal.

### Acceptance
- Burst test: one/few reloads, convergence ≤ 300 ms, pubsub only (poll 0); no relay.
- Rewind test: local == DB after force; no permanent skew.
- Full PG/MySQL suites, config-fanout tests, 3-node drill; clippy/fmt.

## 4. H2 — Redis health in cluster status

### Problem
`GET /api/v1/cluster/status` always reports `redis_reachable: null`; operators cannot distinguish "Redis configured but down" from "no Redis". The config-bus liveness gauge covers only the watcher.

### Design
- Use the concrete `RedisBackend` (it owns the fred pool privately); do **not** add an async trait (`async fn` in trait is not dyn-compatible without boxing, and there is no testability gain).
- Add a bounded `RedisBackend::ping() -> bool` method using the existing pool/command timeout. `AdminState` holds `Option<Arc<RedisBackend>>` (feature-gated), injected by `main.rs` from the existing `redis_backend`.
- `cluster_status` sets:
  - `redis_configured = false`, `redis_reachable = null` when no Redis;
  - `Some(ping)` when configured (bounded by the existing command timeout).
- Add a small unit/integration test with a real Redis and with Redis absent; no status endpoint change for single-node.
- Optional: `hydra_redis_up` gauge updated by the same probe (do not add an alert in this phase).

### Acceptance
- Status reports `true` with Redis, `false` after a cut, `null` when unconfigured.
- Full suites + admin API tests; no new required env.

## 5. H3 — DB edge correctness

### H3.1 `update_tenant` 0-row behavior
- `db::update_tenant` currently returns `Ok(())` even when no row matched. **Do not infer absence from `rows_affected`**: PG reports matched rows, MySQL reports changed rows, so a legitimate no-op `PUT` (same body) would become a 404 only on MySQL.
- Inside the existing write transaction, run a cheap `SELECT 1 FROM tenant WHERE id = ?` probe first; if it misses, return `sqlx::Error::RowNotFound` (admin handler maps it to 404). If it hits, run the UPDATE and return `Ok(())` even when `rows_affected == 0`.
- Test both backends: (a) no-op update of an existing tenant → 200; (b) update of a missing tenant → 404, no panic.

### H3.2 `user_api_key_delete` full scan
- `tenant_api::handlers::user_api_key_delete` currently calls `list_user_api_keys` to verify ownership before a scoped delete. Add a scoped DB helper that checks **both the path user and the tenant**: `SELECT 1 FROM user_api_key k JOIN hydra_user u ON u.id = k.user_id WHERE k.id = ? AND k.user_id = ? AND u.tenant_id = ?` (or an equivalent `EXISTS` with all three predicates).
- Fold the check and the delete into the SAME write transaction so there is no check-then-delete TOCTOU; keep `deleted == 0 → 404` as the race backstop. The route's 404 contract is "no such key **for this user**"; do not widen it to "some user of this tenant".
- Test: cross-tenant key id → 404; same-tenant but different user's key id → 404; own key → deleted.

### H3.3 `duplicate_external_id` contract drift
- The tenant error table says 409, but the insertion path emits 400 in at least one case (`scripts/check_tenant_error_codes.cjs` DRIFT; the intra-bulk duplicate at `tenant_api/handlers.rs:1223`). Canonicalize to **409**, matching the table and the existing `duplicate_name` intra-bulk precedent (`handlers.rs:1459-1460`). Update the handler and documentation; keep the caveat that a client depending on 400 is a breaking change. Do not change `duplicate_name` semantics.

### Acceptance
- Focused tests for all three; `check_tenant_error_codes` no DRIFT; full PG/MySQL suites; clippy.

## 6. H4 — Handler `WriteCtx` / body-validation scaffolding

### Problem
`admin/handlers.rs` (~3.1k) and `tenant_api/handlers.rs` (~2.6k) repeat body read/parse/validate/reload tails and thread `Option<&ReloadGuard>` + `trace_id` through many signatures; one `#[allow(clippy::too_many_arguments)]` remains.

### Design (incremental, mechanical)
1. Introduce `WriteCtx<'a> { guard: Option<&'a ReloadGuard<'a>>, trace_id: &'a str }` (or an owned equivalent) and pass it as one argument to admin/tenant write handlers.
2. Introduce `async fn with_parsed_body<T>(session, ctx, f: impl FnOnce(T) -> Fut)` for the repeated body read/JSON parse/error shape; keeps validation at the edge.
3. Update call sites in bounded batches; do not change response contracts or error codes.
4. Remove the remaining `too_many_arguments` allow only if the signature genuinely drops below the lint threshold; do not add new allows.

### Acceptance
- No behavior change; all existing admin/tenant tests, full suites, clippy `-D warnings`.
- Net reduction in handler line count and duplicated body/parse/reload blocks; report before/after.
- This phase is intentionally separate because it is high-churn; it must not be mixed with H1–H3 commits.

## 7. H5 — Security/ops documentation

- **DB TLS**: mandate `sslmode=verify-full` (PostgreSQL) / `ssl-mode=VERIFY_IDENTITY` (MySQL) in the production deployment checklist; the DB holds every tenant, key hash, and sealed secret. Document that the compose credentials are local-only.
- **Config-channel Redis ACL**: `hydra:config:changed` is a trusted channel; a spoofed message can only trigger a DB-authoritative reload, but it is a read-amplification vector. Document that the channel must be publish-restricted (ACL) and Redis internal-only.
- **Audit table (design only)**: append-only `admin_audit` (who/what/when, redacted) written in the existing serialized write transaction; future outbox option. Do not implement in this plan unless separately approved.
- **MySQL `rows_affected` semantics**: document changed-vs-matched divergence (no-op UPDATE may bump on PG only; harmless no-op reload).

### Acceptance
- `dev-docs/ops.md`/`deployment.md`/`cluster.md` updated; docs guards and stale-term checks pass. No code behavior change.

## 8. H6 — Optional/deferred

- **`rand` normalization** (0.8 direct vs 0.9/0.10 transitive): requires touching proxy/crypto/admin/tenant call sites (`thread_rng` → `rng`); do only with a dedicated lane and full tests. Not a correctness issue.
- **CI guard complexity**: `check_ci_wiring.cjs` is a second CI parser; generating step lists from one source is a larger tooling project, not required.
- **`main::bootstrap` / `proxy::request_filter` splits**: pure structural work; defer until a feature forces a change.
- **Per-entity invalidation / outbox / external config cache**: explicitly out of scope; revisit only if reload metrics show the global snapshot cannot meet the write-rate budget.

## 9. Phase order, dependencies, gates

| Phase | Content | Depends on | Oracle gate |
|---|---|---|---|
| 0 | This plan | — | **plan approval required** |
| 1 | H1 reload coalescing + force consistency | Phase 0 | yes (before H2) |
| 2 | H2 Redis health | Phase 1 | yes |
| 3 | H3 DB edge correctness | Phase 2 | yes |
| 4 | H4 handler `WriteCtx` | Phase 3 | yes; can run after H5 in parallel only if files are disjoint |
| 5 | H5 security/ops docs | Phase 3 | docs guard + Oracle note |
| 6 | H6 optional | — | only on explicit request |

Parallelism:
- H1 (`store.rs`, `cluster/config_bus.rs`, metrics/tests) and H2 (`admin/cluster_api.rs`, `admin/mod.rs`, `redis/mod.rs`, `main.rs`) are sequential because both touch observability/metrics; H1 first.
- H3 (`db/tenant.rs`, `db/identity.rs`, tenant handlers) is independent of H4; can run in parallel if H4 only touches admin/tenant handler scaffolding (avoid same files).
- H5 docs can run in parallel with H3/H4.
- H4 is high-churn; do not parallelize with any handler change from H2/H3.

## 10. Validation matrix (every phase)

- `cargo test -p hydra-core`
- `cargo test -p hydra-server --features server,db-postgres` (CI hard-gate count; update public claims if tests change)
- Full PG/MySQL cluster recipes with real services:
  - `server,cluster-redis,usage-clickhouse,db-postgres`
  - `server,cluster-redis,usage-clickhouse,db-mysql`
- `cargo clippy --all-targets -- -D warnings` both
- `cargo fmt --all --check`
- `node --test scripts/*.test.cjs`; `check_dialect_sql`, `check_migration_parity`, `check_ci_wiring`, `check_gate_entries`, `check_source_purity`, `check_tenant_error_codes`, docs/metrics/alert guards, public claims
- Integration drills: `test_shared_db_cluster.py`, `test_shared_db_outage.py`, `test_shared_db_random_fanout.py` on PG; MySQL at least the outage/shared-DB smoke
- `git diff --check`

## 11. Risks / open decisions

1. **Coalescing latency vs convergence**: 50 ms quiet window + 250 ms cap adds latency; the poll backstop and local writes are unaffected. If the cap is hit continuously, the reload still occurs within 250 ms. Confirm this is acceptable.
2. **Force rewind semantics**: adopting `db_version` after a rewind intentionally rolls the local watermark down. This is the only way to restore the version gate; document it as the recovery path. If the user prefers a monotonic local version, reject H1.2 and keep a documented skew instead.
3. **`duplicate_external_id` 409 vs 400**: plan chooses 409 to match the table; this is a tenant-visible contract change. Confirm no client depends on 400.
4. **H4 churn**: handler refactor is large and easy to conflict with feature work; if time/risk is high, keep it as a separate PR after H1–H3.
5. **Redis health probe**: a PING on every status request is bounded by the 500 ms command timeout; the admin endpoint is low-frequency. No background health task is added.
6. **Audit table**: design-only in H5; implementation would be a separate approved phase (schema + write hook + retention).

## 12. Success criteria

- Burst writes no longer cause unbounded per-node full reloads; measurable coalescing and convergence.
- Force recovery from a revision rewind leaves no permanent local/DB skew.
- Cluster status answers Redis reachability.
- The three DB edge cases have tests and consistent contracts.
- Handler scaffolding is smaller without behavior changes.
- Security/ops docs state the DB TLS/Redis ACL requirements.
- All existing gates remain green; no new P0/P1 from the phase Oracle gates.
