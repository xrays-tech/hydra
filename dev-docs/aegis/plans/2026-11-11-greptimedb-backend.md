# GreptimeDB usage backend — development detail & plan

- **Status**: approved by @oracle (ora-19) with four amendments; implemented W1–W3; final gate REVISE P1-1 (O(n) chunker) → fixed → **APPROVED**
- **Date**: 2026-11-11
- **Baseline**: `fc77d9a`; PG 503/0, MySQL 500/0, core 228/0, claims 731
- **Authority**: ADR-0002, `dev-docs/usage-backends.md`, `dev-docs/ops.md`
- **Goal**: add `greptimedb` as a first-class `HYDRA_USAGE_SINK` value with the same writer+reader contract as `clickhouse` and `tdengine`, without touching `main.rs`, the engine, or other backends.

## 1. Scope

- New Cargo feature `usage-greptimedb`.
- New module `crates/hydra-server/src/usage/backends/greptimedb/{mod.rs,transport.rs}`.
- One registry row in `usage/mod.rs` (after `clickhouse`).
- Env: `HYDRA_GREPTIMEDB_URL`, `HYDRA_GREPTIMEDB_CONNECT_TIMEOUT_MS` (2000), `HYDRA_GREPTIMEDB_IO_TIMEOUT_MS` (5000), `HYDRA_GREPTIMEDB_QUERY_TIMEOUT_MS` (5000). URL form `http://user:pass@host:4000/<db>`; DB defaults to `hydra_usage`; table `usage_record`.
- Tests, docs, guards, optional compose service. No `main.rs`/engine/other-backend edits.
- `https://` is refused at open (no TLS transport), same policy as ClickHouse, with an actionable error.

## 2. Wire protocol (GreptimeDB v1.3, verified by research)

- `POST /v1/sql?db=<db>`; `Content-Type: application/x-www-form-urlencoded`; body `sql=<statement>`; Basic auth from URL creds; `X-Greptime-Timezone: UTC`; optional `X-Greptime-Timeout`.
- SELECT: `{"output":[{"records":{"schema":{"column_schemas":[{"name":...}]},"rows":[[...]],"total_rows":N}}],"execution_time_ms":N}`.
- INSERT/DDL: `{"output":[{"affectedrows":N}],"execution_time_ms":N}`.
- Error: non-2xx + `{"code":N,"error":"..."}`. Treat non-2xx as failure; also treat a 2xx body containing a top-level `code`/`error` as failure (a 200 is not automatically success).
- No HTTP bind parameters: tenant/since/until are escaped literals; grouping expressions are a whitelist only.
- SELECT response cap: **64 KiB** (ClickHouse parity, `transport::MAX_GREPTIMEDB_RESPONSE`); over-cap/truncated ⇒ `UsageQueryError::ResultTooLarge(64*1024)`. INSERT body cap is separate (~3.5 MiB).
- Batch insert: chunk at 512 rows **or when the URL-encoded form body would exceed 3.5 MiB** (measure the final encoded body, not raw SQL), whichever first.

## 3. DDL / schema / idempotency

Bootstrap synchronously in `open` (fail-fast like TDengine):

```sql
CREATE DATABASE IF NOT EXISTS <db>;
CREATE TABLE IF NOT EXISTS usage_record (
  tenant_id STRING,
  provider_id STRING,
  model_key STRING,
  client_api_key STRING,
  user_id STRING,
  user_group_id STRING,
  api_key_id STRING,
  status_code UINT32,
  tokens_in UINT64,
  tokens_out UINT64,
  cache_hit_tokens UINT64,
  latency_ms UINT64,
  forward_latency_ms UINT64,
  ttft_ms UINT64,
  upstream_host STRING,
  err STRING,
  trace_id STRING,
  created_at STRING,
  ts TIMESTAMP TIME INDEX,
  PRIMARY KEY (tenant_id, trace_id)
) WITH ('append_mode' = 'false', 'ttl' = '90d');
```

- `created_at` stays the canonical fixed-width `YYYY-MM-DDTHH:MM:SSZ` string (day grouping uses `substr(created_at,1,10)` and `max(created_at)` is the reported `as_of`).
- `ts` is derived from `created_at` (epoch ms) and **is** the read window predicate:
  `ts >= <since_ms> AND ts < <until_ms>`, which makes the TIME INDEX prune the scan. **This line
  previously said `ts` was "not used for window predicates"; that is now false** (W4, 2026-11-11 —
  see the hardening plan §3 and `dev-docs/usage-backends.md` §5.6). A non-canonical `created_at`
  writes `ts=0` and is invisible to a real window (write-side fallback, documented in the backend
  module).
- `append_mode=false` (default) + primary key + time index gives merge/dedup on retry: the same `(tenant_id, trace_id, ts)` re-inserted folds instead of double-counting. `ttl='90d'` bounds storage.
- **Risk / fallbacks (live-gated)**: (a) `PRIMARY KEY (tenant_id, trace_id)` + separate `ts TIME INDEX` is the canonical GreptimeDB shape; if the live server rejects it, fall back to `PRIMARY KEY (tenant_id, trace_id, ts)` (same dedup semantics). (b) `TIMESTAMP` is the documented SQL type (millisecond precision by default); if the live server rejects it, try `TIMESTAMPMILLISECOND`. Record whichever is measured in this plan.
- **`trace_id` is load-bearing for dedup**: it must be non-empty. The writer derives a deterministic fallback key (FNV-1a over the row fields) when an empty `trace_id` arrives, writes that into the `trace_id` column, and unit-tests it; an empty key would otherwise fold same-millisecond records of one tenant.
- `err` avoids the reserved-word risk of `error` (TDengine precedent).

## 4. Writer

- Use the shared `run_channel_sink`/`drain_on_drop` engine, like TDengine/ClickHouse.
- Multi-row `INSERT INTO usage_record (cols) VALUES (...),(...)`; `NULL` for absent optionals; single-quote escaping (`'`→`''`); numeric counters as integers.
- `created_at` → epoch ms parser: pure Rust (days-from-civil or `chrono` already in `runtime`), unit-tested round-trip for leap years/epoch.
- Batch retry is idempotent via the PK+time-index merge; no batch token (HTTP SQL has none). Document the residual: a retry after the merge window / server TTL behaves like any append store.

## 5. Reader

- Totals: `count(*) AS requests`, `sum(tokens_in)`, `sum(tokens_out)`, `sum(cache_hit_tokens)`, `sum(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS errors`, `max(created_at) AS last_seen`, `WHERE tenant_id = '<escaped>' AND ts >= <since_ms> AND ts < <until_ms>` (bare epoch-ms bounds on the TIME INDEX; the tenant stays an escaped literal).
- Grouped: whitelist expressions: `model_key`, `provider_id`, `coalesce(user_id,'')`, `coalesce(user_group_id,'')`, `coalesce(api_key_id,'')`, `substr(created_at,1,10)`; alias `group_key` (not `key`).
- Decoder: schema-name → index from `output[0].records.schema.column_schemas`; counters accept JSON numbers or numeric strings; `null` aggregate over an empty window = 0 (TDengine precedent); key `null` → `""`; `last_seen` string normalized via `hydra_core::tenant_api::normalize_as_of`; any shape drift is `Decode`, never zero.
- `source()` returns `"greptimedb"`.

## 6. Files / touch points

- `crates/hydra-server/Cargo.toml`: `usage-greptimedb = ["dep:base64"]` (base64 already enabled via `db`; declared for clarity).
- `crates/hydra-server/src/usage/backends/mod.rs`: module + feature gate.
- `crates/hydra-server/src/usage/mod.rs`: one `&backends::greptimedb::DESCRIPTOR` row; fix the stale "engine has exactly ONE user (ClickHouse)" comment.
- `crates/hydra-server/src/usage/backends/greptimedb/mod.rs`: descriptor/open/sink/query/decoder/DDL; `transport.rs`: URL/HTTP/form encoding/response cap.
- `crates/hydra-server/tests/greptimedb_sink.rs`: wire mock (raw TCP) + DDL/INSERT parity; `tests/greptimedb_transport.rs` or inline unit tests. **Do not write the string literal `"greptimedb"` in files outside `src/usage/`** (the kind-literal guard walks `tests/`); reach the descriptor/constants through `hydra_server::usage::backends::...`.
- `crates/hydra-server/src/lib.rs`: widen `lock_gate` to `any(feature = "db", feature = "usage-tdengine", feature = "usage-greptimedb")` (comment updated).
- `#[ignore]` live test in `greptimedb/mod.rs` (env `HYDRA_TEST_GREPTIMEDB_URL`), like TDengine; not in CI by default.
- `scripts/check_usage_backends.cjs`: add `greptimedb` to `KIND_LITERALS_OFF_LIMITS`; update real-tree expected list in `.test.cjs` to `clickhouse, greptimedb, none, tdengine`; document `requires` rows in `ops.md` (so no candidate exception needed).
- `dev-docs/usage-backends.md`: matrix row + URL/SQL notes; `dev-docs/ops.md` + `deployment.md`: env rows/defaults; `environment/docker-compose.local.yml` optional `greptimedb` service (image `greptime/greptimedb:v1.3.0`, HTTP 4000, healthcheck `/health`) with a `HYDRA_USAGE_SINK=greptimedb` example; do not change the default stack.
- CI: add `usage-greptimedb` to the optional-features compile/clippy/test matrix (PG and MySQL variants). No live-deps step (TDengine parity: live test is `#[ignore]`).
- `usage-greptimedb` is standalone, **not implied by `server`**: PG/MySQL hard-gate counts and public claims (731) do not move; the optional-features job is where it runs.

## 7. Tests / acceptance

- Unit: URL parsing (default db, creds, reject https/bad scheme), form/body encoding, escaping, epoch-ms parser, response/error classification, decoder (numbers/strings/null/empty key), DDL/INSERT parity.
- Wire: raw-TCP mock asserting `POST /v1/sql?db=...`, Basic auth, form body, multi-row INSERT, SELECT shapes, 4 MiB cap.
- Live: `docker run greptime/greptimedb:v1.3.0 standalone start`; run `#[ignore]` test for DDL + insert + dedup-on-retry + totals/grouped read; report results.
- Registry/engine consistency + `check_usage_backends` + `check_documented_env/defaults/metrics` + scripts suite.
- `cargo test -p hydra-server --features server,usage-greptimedb,db-postgres` (and db-mysql compile/clippy); full PG/MySQL suites; fmt/clippy.

## 8. Wave plan

| Wave | Content | Owner |
|---|---|---|
| W0 | this plan | **Oracle gate** |
| W1 | backend code + Cargo + registry + wire/unit tests | fixer A |
| W2 | docs/guards/compose + script expected list | fixer B (after W1) |
| W3 | live GreptimeDB container drill + full acceptance | orchestrator |

## 9. Risks / decisions

1. **PK/time-index + type spelling**: live test gates `PRIMARY KEY(tenant_id, trace_id)` and `TIMESTAMP`; fallbacks documented in §3 and must be recorded if used.
2. **HTTP SQL throughput**: documented as not the high-throughput path (gRPC is future); same class as TDengine/ClickHouse HTTP.
3. **No TLS**: refuse `https://` at open; terminate TLS in a proxy or add a transport later. SELECT cap is 64 KiB; INSERT cap is encoded-body ~3.5 MiB.
4. **No bind params**: strict escaping + whitelist only; tenant id is escaped, never interpolated as an identifier.
5. **TTL fixed 90d**: matches the audit default; make it an env knob later if needed.
6. **Compose**: optional service only; the default image build does not gain the feature.

## 10. Measured outcomes (real GreptimeDB v1.3.0, 2026-11-11)

- Canonical DDL accepted: `ts TIMESTAMP TIME INDEX`, `PRIMARY KEY (tenant_id, trace_id)`, `append_mode=false`, `ttl=90d` (no fallback needed).
- Two identical INSERTs of one row => `SELECT count(*)` = 1, `sum(tokens_in)` = 10: retry dedup is real.
- SELECT shape: `output[0].records.{schema.column_schemas,rows}` with Int64/UInt64 JSON numbers and String `last_seen`; `coalesce/substr/CASE/max(created_at)/group_key` all work.
- Live `#[ignore]` test PASSES: DDL bootstrap, multi-row insert, retry-dedup, totals+grouped read, empty-window zero, refused-statement failure.
- O(n) exact encoded-body chunker (512 rows / 3.5 MiB) after Oracle P1-1; P2-1 current-thread guard, P2-2 bracketed IPv6, P2-3 body-cap-after-headers folded in.
- Verification: PG+`usage-greptimedb` **539/0**; MySQL+`usage-greptimedb` **536/0**; core **228/0**; clippy `-D warnings` both DBs; scripts **333/333**; guards green; public claims stay **731** (feature standalone).
