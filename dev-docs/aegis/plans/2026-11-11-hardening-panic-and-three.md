# Hardening batch: production panic surface + usage transport + GreptimeDB pruning + DB write metrics

- **Status**: approved by @oracle (ora-19) with four amendments; executing W1–W5
- **Date**: 2026-11-11
- **Baseline**: `e88cab2`; PG hard gate 503/0, MySQL 500/0, core 228/0, claims 731
- **Goal**: close the remaining production panic surface truthfully, then land the three recommended improvements, then one unified acceptance.

## 0. Truth report on “410 unwrap/expect”

The repo's own purity guard already forbids production `unsafe`, `.unwrap()`, `panic!`, `unreachable!`, `todo!`, `unimplemented!`. Blanking `#[cfg(test)]` items (repo lexer `scripts/rust_blank.cjs`) and test-only modules/files:

| crate | prod `.unwrap()` | prod `.expect(` | `unwrap_or_else` (non-panic) | panic-family |
|---|---|---|---|---|
| hydra-core | 0 | 0 | 0 | 0 |
| hydra-server | 0 | **2** | 28 | 0 |

The 410 was raw text including test bodies. Production actionable work is: **2 registered `expect()`** (boot-only), **12 lock-poison `unwrap_or_else`** sites worth normalizing through `lock_gate`, and a **guard gap** (`assert!`/`assert_eq!` not forbidden; today 0 occurrences). Plan a truth-report note in the plan/ledger rather than deleting test expectations.

## 1. P — production panic surface (Tier 0–3)

- **T0 (keep)**: existing bans stay; 0 today.
- **T1 — remove the 2 production `expect()`**:
  - `boot/listeners.rs:106` `cert_store.expect(...)`: the surrounding boot path already returns `Result`; return a `BackendError`/boot error or fold into the plaintext-only branch. No panic.
  - `proxy/provider_client.rs:131` `.expect("reqwest client … must build")`: change the constructor to return `Result<_, reqwest::Error>` (or `ProxyError`), propagate from `proxy.rs:253` (`HydraProxy::new`) and its boot/service seam. **`crates/hydra-server/src/proxy.rs` is in W1**; `provider_client` is not in `AppState`, so only the 8 `#[cfg(test)]` constructors and the single production call site ripple.
  - Then remove both `EXPECT_SITE_OK` entries, keep the guard's stale-record check green, and sync the advertised `expect()` count in `README.md`/`README.zh-CN.md` to **0**.
- **T2 — normalize 12 lock-poison sites to `crate::lock_gate`** (single owner; behavior identical):
  - `cluster/config_bus.rs:336`, `db/pool.rs:363,380,389`, `store.rs:703,721,896`, `usage/backends/clickhouse/mod.rs:151,193,198,214,219`.
  - **Blocker first**: `lib.rs` gates `lock_gate` on `any(db, usage-tdengine, usage-greptimedb)`; add `usage-clickhouse` (its feature does not imply `db`) or a clickhouse-only build fails to compile. Keep `lib.rs`'s own definition as the only `unwrap_or_else(PoisonError::into_inner)`.
- **T3 — tighten the guard (no new scanner)** in `scripts/check_source_purity.cjs`:
  - forbid `assert!`/`assert_eq!`/`assert_ne!` in production (`debug_assert*` stays allowed); 0 migrations today;
  - tighten `\.unwrap\(\)`→`\.unwrap\b`, `\.expect\(`→`\.expect\b` to catch latent turbofish/whitespace forms;
  - add fixtures to `scripts/check_source_purity.test.cjs` for both, including a `debug_assert!` NON-match case (the assert rule must not fire inside `debug_assert!(`).
  - sync README prose, not just the count: `README.md`/`README.zh-CN.md` currently name the two expect sites and assert "2 panic sources"; after T1 they must say zero and drop the site list.
- Acceptance: purity guard green with **0** registered expects; clickhouse-only, tdengine-only, greptimedb-only, db builds all compile; full suites unchanged.

## 2. U — shared usage HTTP transport (stage 1 only)

`src/usage/backends/*/transport.rs` is 2421 lines with byte-identical `percent_decode`/`url_encode`/`env_millis`/Basic auth/framing/read-loop/`response_body`/`dechunk`/caps. Add `src/usage/http.rs` owning **framing/auth/read/limits only**:

```rust
pub(crate) struct Endpoint { host, port, connect_timeout, io_timeout }
pub(crate) struct BasicAuth { user, pass }
pub(crate) struct Request<'a> { path_and_query, host_header, auth, content_type, extra_headers, body }
pub(crate) enum Overflow { FlagTruncated, Error }
pub(crate) struct ReadLimits { max_header_bytes, max_body_bytes: Option<usize>, overflow }
pub(crate) struct Response { status_line, status: Option<u16>, body, truncated }
pub(crate) async fn exchange(ep, req, lim) -> Result<Response, String>;
```

- Keep every backend's **public `send`/parse signature unchanged** (so `mod.rs` call sites and W4 do not conflict); only the internals delegate.
- Preserve invariants exactly: CH loose `" 200 "` + total 64 KiB + truncation flag; TD total 4 MiB + `Err` overflow + ignore status; G header/body caps + truncation flag + 2xx gate. Do not unify `is_ok_status` or the envelope classifiers.
- Base64: move TD's dependency-free base64 into `http.rs` and have CH/G use it; drop `dep:base64` from `usage-clickhouse`/`usage-greptimedb` (base64 remains enabled independently by `db`/crypto). If this churn is judged risky, keep per-feature deps and stage the base64 move.
- **Cap accounting (recorded)**: G's `ReadLimits` is the *split* form (`max_header_bytes` + `max_body_bytes: Some(64 KiB)`), so `MAX_GREPTIMEDB_RESPONSE` is the **body** cap and the status line/headers no longer eat into it — the effective body budget grows by the header-block size (Δ = header bytes) relative to a total-cap reading. CH (`max_body_bytes: None`, total 64 KiB) and TD (total 4 MiB) keep their total-cap accounting. `usage::http` tests pin both forms, including that split headers do not shrink the body cap.
- **Stage 2 (TD+G URL-parser dedupe) deferred** to the ledger (95% identical but distinct error sites).
- Tests: consolidate the moved unit tests into `http.rs`; keep one contract test per backend (success/envelope/URL parser).

## 3. G — GreptimeDB time-index pruning

- Switch the reader window at `greptimedb/mod.rs:578-581` to `ts >= <since_ms> AND ts < <until_ms>` using the existing `epoch_millis` (`mod.rs:478-531`); keep `max(created_at) AS last_seen` and `substr(created_at,1,10)` day grouping. Drop the now-unneeded string escapes for bounds, keep `tenant_id` escaping.
- Equivalence: `created_at` is second-resolution and `ts` is derived from it, so the predicate is semantically identical for canonical rows; document that a non-canonical `created_at` (writer fallback `ts=0`) is invisible to a `ts` window (same class as the old string window truncation).
- Live validation: bare integer vs `TIMESTAMP` in `v1.3.0`; if implicit cast is rejected, the fallback is `TIMESTAMP '<canonical created_at>'` (a datetime literal, not `'<ms>'`), still constant/index-usable; the old string predicate is the last resort. Pruning evidence must be concrete: `EXPLAIN [ANALYZE]` showing a range scan on `ts`, or a measured scan delta — not an assertion.
- Partition: run a **live measurement** of `PARTITION ON COLUMNS (tenant_id)` / `(ts)` (syntax is a sibling clause, range rules only; existing tables are unaffected by `CREATE TABLE IF NOT EXISTS`). Adopt `tenant_id` only if it parses on v1.3.0 and prunes a representative query; otherwise record the measured decision in the ledger (no ALTER exists for repartition).
- Update wire/live tests and `dev-docs/usage-backends.md`/`ops.md`; fix the now-false plan line “`ts` is not used for window predicates”.

**Outcome (W4, measured against a live `greptime/greptimedb:v1.3.0` at `127.0.0.1:4400`, 2026-11-11):**

- **Implemented**: the reader now emits `WHERE tenant_id = '<escaped>' AND ts >= <since_ms> AND ts < <until_ms>`
  via the existing `epoch_millis`; `max(created_at) AS last_seen` and `substr(created_at,1,10)` day
  grouping are unchanged; the two bound string escapes were removed. A non-canonical bound is a
  refusal (`UsageQueryError::Decode`), never a widened window. Equivalence and the `ts=0` fallback
  visibility are documented in the backend module header and `usage-backends.md` §5.6.
- **Bare integer accepted**: v1.3.0 accepts bare epoch-ms integers interchangeably with
  `TIMESTAMP '…'` literals (both returned `count=2` for the same window), so the planned fallback to
  `TIMESTAMP '<canonical created_at>'` was **not** needed.
- **Pruning evidence** (`EXPLAIN ANALYZE`): the `ts` window's `SeqScan` carries
  `"time_range": "TimestampRange{[1794355200000ms,1794358800000ms)}"`; the old `created_at` string
  predicate carries the unbounded `"time_range": "TimestampRange{{[#,#)}}"` and filters row-by-row in
  `FilterExec`. Concrete, not asserted — the command is in `usage-backends.md` §5.6.
- **Partition experiment — not adopted (ledger-only; no in-place repartition)**: empty rule lists
  (`PARTITION ON COLUMNS (tenant_id) ()` / `(ts) ()`) parse but create ONE region
  (`partition_count:{"count":1}`) ⇒ no pruning. The documented multi-checkpoint form
  (`(<col> < 'm', <col> < 't', …)`) is **rejected** on v1.3.0 (`Checkpoint … is overlapped` /
  `is not covered`). Only the complementary two-rule form
  `PARTITION ON COLUMNS (tenant_id) (tenant_id < 'm', tenant_id >= 'm')` parses, and it does prune a
  representative query to one of two regions (pushed `tenant_id < 'm' OR tenant_id IS NULL` vs
  `tenant_id >= 'm'`). Kept out of the DDL anyway: a fixed two-bucket lexicographic split at `'m'`
  is arbitrary for real tenant ids (all can land in one bucket) and would change the canonical DDL
  shape for no reliable benefit, while the `ts` range already prunes. Canonical DDL and its
  assertions are therefore unchanged.
- **Docs**: `usage-backends.md` matrix/§5.5/§5.6 and `ops.md` updated; the false
  “`ts` is not used for window predicates” line in `2026-11-11-greptimedb-backend.md` corrected.

## 4. M — DB write retry observability

Instrument only `begin_write_loop` in `db/pool.rs` (both `begin_write` and the test seam funnel through it), never `begin_write_once` on top of the existing probe:

- `hydra_db_write_retries_total{outcome="retry"|"exhausted"}` (retry scheduled at `pool.rs:459-470`; exhausted when the retryable error leaves at `:471` with `attempt == BEGIN_WRITE_MAX_ATTEMPTS`);
- `hydra_db_write_acquire_seconds` histogram (wrap `begin_write_once` at `:457`; new buckets);
- `hydra_db_write_waiters` gauge (acquire-only): acquire at loop entry `:454`, and use an **RAII guard** whose `Drop` decrements (repo precedent `FailPersistGuard`, `pool.rs:740-748`) so a panic/drop mid-acquisition cannot leak the gauge; never tied to `Tx` lifetime.
- Register in `admin/metrics.rs` with the existing register/record patterns; add a `record_*` unit test. Keep the always-compiled probe statics untouched; do not assert absolute Prometheus values in live tests (use deltas or direct `record_*` tests).
- Docs: `ops.md` key-series/alert row; if an alert expression is added, the alert guard requires the metric and every label key to resolve.

## 5. Waves / ownership / dependencies

| Wave | Content | Depends on | Files |
|---|---|---|---|
| W0 | this plan | — | **Oracle gate** |
| W1 | P T1+T2+T3 (panic surface + guard) | — | `lib.rs`, `boot/listeners.rs`, `proxy/provider_client.rs`, **`proxy.rs`**, `store.rs`, `db/pool.rs`, `cluster/config_bus.rs`, `usage/backends/clickhouse/mod.rs`, `scripts/check_source_purity.cjs`+test, READMEs |
| W2 | M (DB write metrics) | W1 (shared `pool.rs`) | `db/pool.rs`, `admin/metrics.rs`, `ops.md` |
| W3 | U stage 1 (shared transport) | W1 | `src/usage/http.rs` (new), three `transport.rs`, `Cargo.toml`, tests |
| W4 | G (GreptimeDB pruning + partition measure) | W1; W3 only if signatures change (design keeps them stable) | `usage/backends/greptimedb/mod.rs`, tests, `usage-backends.md`, `ops.md` |
| W5 | unified acceptance + Oracle final | W2–W4 | — |

W2/W3/W4 are file-disjoint and may run in parallel after W1. If W3 cannot keep the `send` signatures stable, sequence W3 before W4 (both touch `greptimedb/mod.rs`).

## 6. Acceptance

- Purity: 0 production unwrap/panic/assert; 0 registered expects; guard fixtures green; READMEs say 0.
- `-only` feature builds are **compile+clippy** (`usage-clickhouse`/`usage-tdengine`/`usage-greptimedb`), not full tests (the wire tests need the db-gated `common::setup_pool`); full tests run under the standard hard-gate feature sets.
- Full PG/MySQL hard-gate suites green **and re-measured from logs**; claims/README counts are refreshed from those measurements (W1/W2/W4 add/reshape tests, so the old 731 is not expected to stay).
- Shared transport: byte-identical request shapes on the wire for all three backends (wire tests assert exact request bytes; caps/overflow/truncation semantics preserved).
- GreptimeDB live drill: `ts` window prunes (EXPLAIN/scan check), totals/grouped/dedup still correct; partition decision recorded.
- DB metrics: retry/exhausted/waiters/acquire series appear on `/metrics`; live retry tests still pass; no double counting.
- Scripts 333/333; documented metrics/env/defaults, alert, migration, CI, claims guards green.
- Oracle final sign-off.

## 7. Risks / decisions

1. **T2 cfg widening** is load-bearing for clickhouse-only builds; verify each feature in isolation.
2. **U stage 1** must not unify success/overflow policy; stage 2 stays deferred.
3. **G `ts` window** changes an existing read path; live test plus equivalence argument required; keep the old string predicate only if live cast fails.
4. **Partition** is a measured decision, not a default; no in-place repartition exists.
5. **M** counters are process-global; tests assert deltas/direct records, not absolutes.

## 8. Outcome (W1–W4 measured, 2026-11-11)

- **P (panic surface)**: production `.unwrap()` / `panic!` / `assert!` = 0; production `.expect()` = **0** (both former sites return a boot error / `Result`); 12 lock-poison sites normalized through `lock_gate`; purity guard forbids production assert-family and `expect_err`; READMEs synced to 0.
- **U (shared transport)**: `usage/http.rs` owns framing/auth/caps/deadlines; CH/TD/G policies preserved (CH total 64 KiB + truncation; TD 4 MiB + Err + envelope; G split caps + 2xx gate). TD's base64 is the single owner; `dep:base64` dropped from usage features. Stage 2 URL-parser dedupe deferred.
- **G (GreptimeDB)**: reader window is `ts >= since_ms AND ts < until_ms` (existing `epoch_millis`); non-canonical bounds are a `Decode` refusal. Live `EXPLAIN ANALYZE` shows `TimestampRange{[1794355200000ms,…)}` vs the old unbounded `created_at` scan. Partition experiment rejected (two-bucket tenant split parses/prunes but adds no operational value; `ts` already prunes; no in-place repartition).
- **M (DB metrics)**: `hydra_db_write_retries_total{outcome="retry"|"exhausted"}`, `hydra_db_write_acquire_seconds`, `hydra_db_write_waiters` (RAII, acquisition-only) registered with docs/alert rows; no double count with the probe statics.
- **Acceptance**: core 228/0; PG hard gate 547/0; MySQL 544/0; live GreptimeDB test PASS; clippy `-D warnings` for all five feature sets; purity clean; scripts 336/336; all guards green; claims re-measured 731→**775** (228 core + 547 server). Oracle final: **APPROVED — commit** (with P2 `expect_err`/cap-wording folded).
