# Remaining optional hardening — development detail & plan (H6)

- **Status**: proposed; requires @oracle gate before implementation
- **Date**: 2026-11-10
- **Baseline**: `4f91bcf` (H1–H5 complete; PG 580 / MySQL 578, CI hard-gate 470 + core 228, claims 698)
- **Authority**: `dev-docs/aegis/plans/2026-11-10-remaining-hardening.md` (H1–H5), ADR-0004, `dev-docs/ops.md`
- **Scope**: close the remaining optional/deferred review items; no new architecture

## 1. Items

| # | Item | Priority | Risk | Effort |
|---|---|---|---|---|
| H6.1 | Tenant API `WriteCtx`/body-helper dedupe (mirror H4) | P1 | Medium churn | 0.5–1d |
| H6.2 | Direct Rust test for admin `400 invalid_json` | P1 | Low | 0.25d |
| H6.3 | `rand` dependency normalization | P2 | Medium | 0.5d |
| H6.4 | `write_tenant(is_create=false)` missing-row probe | P2 | Low | 0.25d |
| H6.5 | `cluster_api::reload` uses the `WriteCtx` shape | P3 | Low | 0.25d |
| H6.6 | `admin_audit` table implementation (design accepted in H5) | P2 | Medium | 0.5–1d |
| H6.7 | CI guard single-source reduction | P3 | Medium tooling | 0.5–1d |
| H6.8 | `main::bootstrap` / `proxy::request_filter` splits | P3 | High churn | 1–2d |

## 2. Designs and acceptance

### H6.1 Tenant API scaffolding
- Add the tenant-API equivalent of the admin `WriteCtx` (`guard` + `trace_id`) and a `read_json_body` helper in `tenant_api/mod.rs`/`handlers.rs`, reusing the same 1 MiB cap/413/400 envelopes.
- Migrate the tenant write handlers (`user-groups`, `users`, `api-keys`, `billing`) mechanically; keep read-only handlers unchanged.
- Acceptance: no route/status/JSON change; focused tenant tests + full suites; net line reduction reported.

### H6.2 `invalid_json` test
- Add an admin API test posting malformed JSON to a migrated mutation route; assert 400 `invalid_json`, the trace id, and connection reuse (mirror the 413 drain test).
- Acceptance: test present and discriminating; `admin_api` suite green.

### H6.3 `rand` — resolved as accepted no-op (evidence recorded)
- Correct measure is `cargo tree -d -p hydra-server --features server,db-postgres`; the bare `cargo tree -d` shows nothing because `rand` is feature-gated.
- Verified at `4f91bcf`: **rand 0.8.7 + 0.9.5** (and a lock-only 0.10.2). 0.8.7 is pulled transitively by `pingora-core/cache`, `fred 10`, `sqlx-postgres`, `sqlx-mysql`; a direct-pin bump to 0.9 cannot remove it (and would not even remove a copy — it would move the direct edge). Therefore: **no code change**, record this evidence. Wave 3 is removed.

### H6.4 `write_tenant` probe
- For `is_create=false`, run the same existence probe as `update_tenant` inside the write transaction; missing id → `RowNotFound`. Keep the create path unchanged.
- **Handler mapping is required**: `admin/handlers` routes `write_tenant` errors through `db_err_resp`; `classify_db_err` maps `RowNotFound` to 500. Map `RowNotFound`→404 on the WRITE error before `db_err_resp`, so the pre-existing 404 (today produced by the post-commit `get_tenant`) stays 404. Add a focused missing-PUT test on BOTH dialects.
- Acceptance: DB test (missing id → RowNotFound) and handler test (missing PUT → 404, not 500); create path unchanged.

### H6.5 `cluster_api::reload`
- Pass `&WriteCtx` to `cluster_api::reload` instead of `(Option<&ReloadGuard>, trace_id)`; keep the response contract.
- Acceptance: focused `/reload` tests + full suites; no behavior change.

### H6.6 `admin_audit` implementation
- Add one migration per dialect (`0002_admin_audit.sql`) with an append-only table: `id`, `actor`, `action`, `method`, `path`, `entity_type`, `request_id`, `created_at`; redacted (no secrets, no bodies). DDL is expected to be dialect-divergent (PK/auto-increment); `check_migration_parity` checks the file sets/versions.
- Router-level best-effort insert (accepted design; not in the write transaction): an audit-schema problem must never fail a mutation, and the router already has method/path/trace_id/status in scope. On insert failure, log ONCE per process/backoff and increment a metric; never per-request error spam. Hook it in the admin router after a successful mutation; tenant-side audit is a later extension.
- Add `hydra_admin_audit_write_total{result}` (registered, catalogued, docs/metrics guard green); no retention/export/index in this phase.
- Acceptance: mutation inserts a row; read-only/failed mutations do not fail; no secret/body stored; migration parity guard green; full suites.

### H6.7 CI guard single-source — DEFERRED (accepted)
- The overlap is real but small (`check_ci_wiring.cjs` ~10 workflow refs vs `check_gate_entries.cjs` 2), and each guard has its own passing `.test.cjs`. A single-parser refactor risks the very guards that protect CI for zero behavior gain. Record the duplication as accepted; revisit only if the parsers drift.

### H6.8 `bootstrap`/`request_filter` splits — DEFERRED (accepted)
- `main.rs`/`proxy.rs` are large but stable; the H1–H5 trigger was "defer until a feature forces a change", and H6 changes nothing structural there. Take the churn only when a feature forces it.

## 3. Waves and gates

| Wave | Content | Depends on | Gate |
|---|---|---|---|
| 0 | This plan | — | **Oracle plan approval** |
| 1 | H6.2 (admin test) + H6.4 (db/tenant) + H6.5 (admin/cluster_api) | plan | H6.1 gate (batch) |
| 2 | H6.1 (tenant scaffolding) | wave 1 | code review / tests |
| 3 | H6.3 (rand) | wave 2 (tenant handlers touched) | code review / tests |
| 4 | H6.6 audit table | wave 3 | Oracle/H6 gate |
| 5 | H6.7 (optional) | wave 4 | docs/guard check |
| 6 | H6.8 (optional) | wave 5 | full drills |

## 4. Validation matrix

- `cargo test -p hydra-core`; `cargo test -p hydra-server --features server,db-postgres` (CI hard-gate count; update public claims if tests change).
- Full PG/MySQL cluster recipes; `cargo clippy --all-targets -- -D warnings` both; `cargo fmt --all --check`.
- `node --test scripts/*.test.cjs`; SQL/migration/CI/error/test-tail/purity/docs/metrics/alert/claims guards.
- Cluster/fanout/outage drills on PG; MySQL at least full suite.
- `git diff --check`.

## 5. Risks / decisions

1. **H6.3 rand**: the transitive graph may keep 0.8 regardless; the plan allows "no code change, accepted" rather than a cosmetic version bump that adds another copy.
2. **H6.6 audit**: router-level best-effort insert is not atomic with the write; acceptable for operator audit, explicitly documented. If atomicity is required, move the insert into the serialized write transaction per handler — larger.
3. **H6.1 churn**: tenant handlers are 2.6k lines; migrate in bounded handler groups, no mixed call shapes.
4. **H6.7/H6.8**: explicitly optional; Oracle may accept the duplication/size rather than take the churn.

## 5.1 Still-open ledger (trigger-gated, not H6)
- **Per-entity invalidation / transactional outbox / external config cache**: not needed while `hydra_config_reload_duration_seconds`/`hydra_config_rows_loaded` show the global snapshot within the write-rate budget. Revisit only if reload cost exceeds that budget. Recorded here so "remaining optional" is complete.

## 6. Success criteria

- H6.1–H6.6 complete with gates; H6.7/H6.8 either complete or explicitly accepted as deferred with rationale.
- All existing gates stay green; no new P0/P1.
- Public claims re-measured after any test-count change.
