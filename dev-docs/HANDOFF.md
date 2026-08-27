# Hydra — Development Handoff & Archive

**Archive date:** 2026-08-09
**HEAD:** `fe5b183` — `feat: harden provider-key display (no plaintext via admin) + mid-stream error metric`
**Status:** Production-ready (evaluation report verdict **9.2/10**, up from 7.8/10).
**Purpose:** This document is the starting point for the next development round — it captures the architecture as it now stands, what was built/verified this round, where things live, and what is deliberately deferred.

---

## 0. Quick start

```bash
# Build (release, all features)
cargo build --release --features server

# Run (master key is REQUIRED — provider keys are AES-256-GCM encrypted at rest)
export HYDRA_ADMIN_TOKEN=...           # admin Bearer token (required)
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # 32-byte base64 (required, fail-closed)
HYDRA_DB_URL=sqlite:hydra.db?mode=rwc  ./target/release/hydra     # or ~/.cargo/global-target/release/hydra
# (note: this project's shell wrapper `rtk` uses a global target dir at ~/.cargo/global-target)

# Test (offline sqlx cache — no DATABASE_URL needed)
SQLX_OFFLINE=true cargo test -p hydra-core                       # 114 tests
SQLX_OFFLINE=true cargo test -p hydra-server --features server   # 173 tests
SQLX_OFFLINE=true cargo clippy --features server --workspace --all-targets -- -D warnings
```

---

## 1. What Hydra is

A high-performance LLM routing gateway in **Rust + Pingora 0.8**. Routes OpenAI-compatible client
traffic to upstream providers. Two crates, strict layering:

- **`hydra-core`** — pure domain logic (router, SWRR load-balancer, circuit breaker, sliding-window
  rate limiter, SSE usage parser, config model + validation). **Dependency firewall** (CI-enforced):
  only `serde`/`serde_json`/`memchr`/`bytes`/`sha2`. No `tokio`/`pingora`/`sqlx`. All state driven by
  an injected `Instant` (deterministic tests). `#![forbid(unsafe_code)]`.
- **`hydra-server`** — the Pingora proxy shell: terminate-mode `request_filter`, SQLite (sqlx) +
  ClickHouse sinks, reqwest upstream client, per-tenant TLS SNI, admin REST + UI + Prometheus
  metrics, external auth cache, **provider-key encryption (crypto module)**, **per-provider bounded
  admission queue (admission module)**.

Both crates: `#![forbid(unsafe_code)]`, **zero** `unwrap`/`panic`/`unsafe` in production `src/`
(all confined to `#[cfg(test)]`).

---

## 2. Architecture as it now stands (this round's additions in **bold**)

```
crates/hydra-core/src/    (pure)
  model.rs     domain types incl. Provider{ max_concurrency, max_queue_depth, queue_wait_timeout_ms }  ← NEW
  config.rs    ConcurrencyPolicy + resolve_policy + validate (admission policy resolution)             ← NEW
  router.rs / swrr.rs / breaker.rs / limit.rs / sse.rs / rewrite.rs (mask_key: 前十+中星+后四) / auth.rs   ← mask format NEW

crates/hydra-server/src/
  main.rs        bootstrap: load master key (fail-closed), build AdmissionControl, wire AppState + AdminState
  proxy.rs       request_filter → failover loop; admission.acquire() before send; §7 breaker boundary; mid-stream metric
  proxy/
    admission.rs   AdmissionControl(DashMap<provider,ProviderGate>) Semaphore+timeout; Permit RAII; snapshot()  ← NEW MODULE
    limiter.rs / breaker_wrap.rs / config.rs (default_concurrency_policy) / provider_client.rs / ctx.rs / peer.rs
  crypto.rs       KeyProvider trait + StaticKeyProvider (AES-256-GCM); KMS slot reserved               ← NEW MODULE
  db.rs           sqlx compile-time query!/query_as! (33 queries); encrypt-on-write/decrypt-on-read for provider keys
  store.rs        ArcSwap hot-reload ConfigStore (holds key_provider)
  http.rs         AuthCache (SHA-256 client keys)
  sink.rs         batched SQLite + ClickHouse usage sink
  tls.rs          per-tenant SNI cert callback
  admin/          mod.rs (routes incl. GET /api/v1/concurrency) / handlers.rs (always-mask provider keys) /
                  metrics.rs (17+ Prometheus metrics incl. 6 admission + mid_stream_errors) / static_files.rs (embedded UI)

migrations/  0001_init · 0002_usage_metrics · 0003_provider_key_encryption · 0004_provider_concurrency
.sqlx/       offline query cache (33 query-*.json) — committed; CI builds with SQLX_OFFLINE=true
```

---

## 3. Feature inventory — what's built (this round's work marked ★)

**Original (pre-review):** terminate-mode proxy, SWRR + circuit breaker + sliding-window limiter,
external auth cache, SQLite + ClickHouse sinks, per-tenant TLS, admin REST + UI + Prometheus, hot-reload.

**★ This round:**
1. **Provider-key encryption at rest** (P1-4) — AES-256-GCM; `KeyProvider` trait (`StaticKeyProvider`
   now, `KmsKeyProvider` reserved); DB stores ciphertext/nonce/key_version; `db.rs` is the encrypt/
   decrypt boundary; fail-closed boot via `HYDRA_ENCRYPTION_KEY` / `_FILE`; migration 0003 (hard
   cutover). Design: client keys stay SHA-256-only (unchanged).
2. **Per-provider bounded admission queue** (P1-7) — Envoy `max_requests`+`max_pending_requests`
   pattern; `tokio::Semaphore` + `acquire_owned()` + `timeout` (FIFO-fair bounded wait); opt-in per
   provider (`None`/`0` ⇒ Passthrough no-op, zero default regression); wired into the failover loop;
   **§7 boundary: admission errors never trip the breaker**; 503+Retry-After only when all candidates
   exhausted; migration 0004; 6 metrics; `GET /api/v1/concurrency` inspect endpoint. See
   **`dev-docs/design-admission-queue.md`** (the authoritative design).
3. **sqlx compile-time SQL checking** (P1-6) — all 33 queries are `query!`/`query_as!`; `.sqlx/`
   offline cache; `SQLX_OFFLINE=true` in CI (SQL drift fails the build).
4. **Admin API no longer returns plaintext keys** (P1-5) — always masked (`前十 + 中星 + 后四`);
   `?reveal=1` is now a no-op.
5. **Mid-stream error observability** (P2-9) — `hydra_mid_stream_errors_total{provider}`; doc note
   that mid-stream failures don't retry (inherent to streaming).
6. **Ops hardening** (P0) — docker-compose healthchecks (hydra/clickhouse/mock-tenant) +
   `depends_on: service_healthy`; Dockerfile env fix (`HYDRA_DATABASE_URL` → `HYDRA_DB_URL`) + curl;
   CI hard gate (`cargo test -p hydra-server --features server`, was `continue-on-error`).
7. **Misc** (P2-8/P2-10) — `now_iso8601()` emits RFC3339 (was `t<unix_secs>`); CI `Cargo.lock` drift check.

---

## 4. Verified properties (this round)

| Property | Evidence |
|---|---|
| Provider keys NOT in DB file | raw SQLite grep of canary = **0 hits**; `api_key` column DROPPED; ciphertext/nonce/key_version stored |
| Fail-closed boot | hydra refuses to start without `HYDRA_ENCRYPTION_KEY` (clear error) |
| No plaintext via admin API | runtime check: 4 paths (collection ±reveal, single ±reveal) all `plaintext_leak=0`, return masked `sk-provide*********mock` |
| §7 breaker boundary | `admission_wait_timeout_does_not_trip_breaker` test: `fail_count` stays 0 |
| Default (Passthrough) no regression | `default_no_concurrency_config_is_passthrough_200`; load smoke ~10.5k RPS @ c=25 ≈ pre-change baseline |
| Compile-time SQL drift caught | injecting `BOGUS_COL` fails `SQLX_OFFLINE=true cargo build`; revert restores |
| Test suite green | core **101**, server **159**; clippy `--all-targets -D warnings` clean; admission tests stable ×3 runs |

> **Process note:** the orchestrator independently re-verified every specialist's work this round and
> caught a build-breaking scope bug (`main.rs:284 admission.clone()`) that a fixer had reported as
> "compilation: clean". Always re-run `cargo build` + full `cargo test` after delegated code changes.

---

## 5. Configuration (operator-facing)

Required env (fail-closed without): `HYDRA_ADMIN_TOKEN`, `HYDRA_ENCRYPTION_KEY` (base64 32B) or
`HYDRA_ENCRYPTION_KEY_FILE` (raw 32B). See `README.md` / `README.zh-CN.md` / `dev-docs/ops.md` §1.2.

Per-provider admission policy (opt-in, all `None` ⇒ unlimited):
```
max_concurrency      # weak upstream 4-8 / mid 16-32 / big provider 64-128 or None
max_queue_depth      # 8-64 (0 ⇒ fail-fast on cap)
queue_wait_timeout_ms # 1000-5000
```

---

## 6. Deliberately deferred (next-round candidates)

| Item | Why deferred | Trigger to revisit |
|---|---|---|
| **Admission P1.1 — client-disconnect cancellation** | @librarian confirmed pingora 0.8 terminate mode exposes **no** clean client-gone signal (private socket stream; `watch_h2_stream_reset` is PR #911, H2-only). RAII `Permit` already self-heals (releases slot on write error), so waste is transient. | When pingora ships PR #911 (H2), OR if `hydra_queue_wait_seconds`/queue-drop metrics show real waste. |
| **Admission P1.2 — per-tenant queue reservation** | Cross-tenant fairness (Envoy priority levels); only matters under multi-tenant contention on one weak upstream. | When production metrics show a single tenant monopolizing a capped provider's queue. |
| **Admission P2 — per-(provider,model) / per-(provider,key) granularity; Redis multi-instance counter** | Finer-grained limits + shared state for multi-instance deployments. | When single-instance caps are insufficient or Hydra runs >1 replica. |
| **sqlx `query!` for dynamic queries** | A few queries use dynamic SQL (column lists / `IN` lists) and stay runtime-checked. | If dynamic-SQL queries proliferate. |

---

## 7. Key documents

- `dev-docs/evaluation-report.html` — production-readiness report (verdict 9.2/10, self-contained HTML).
- `dev-docs/design-admission-queue.md` — the admission queue design (approved P0, opt-in; the spec).
- `dev-docs/design.md` — original architecture/design doc.
- `dev-docs/ops.md` — SRE runbook (env table, systemd, healthchecks, §1.3 encryption, §9 observability).
- `README.md` / `README.zh-CN.md` — overview + config + deploy.

---

## 8. Resume hints for the next round

- **Read first:** `dev-docs/design-admission-queue.md` (admission), `dev-docs/ops.md` (ops), this file.
- **The admission queue is fully wired and opt-in** — to use it, set the 3 fields on a `provider` row;
  nothing else to do. Size `max_concurrency` to the upstream's *measured SSE concurrency* (load-test it).
- **Crypto boundary:** any new persisted secret should go through `crypto::KeyProvider` (seal on write,
  open on read) at the `db.rs` boundary, never in `hydra-core`.
- **SQL changes:** after any migration or query edit, re-run `cargo sqlx prepare --workspace --features db`
  (with a migrated throwaway DB) and commit the refreshed `.sqlx/`. CI builds with `SQLX_OFFLINE=true`.
- **Verification discipline:** never trust a delegated agent's "compiles/clean" claim — re-run
  `cargo build --features server` + full `cargo test` + targeted runtime checks (encryption grep,
  reveal-mask, fail-closed) yourself.
- **`rtk` quirk:** this machine's `rtk` wrapper builds to `~/.cargo/global-target/` (global target dir),
  not `./target/`. A plain `cargo build` (no `rtk`) builds to `./target/`. Don't test a stale binary —
  check mtime + `strings <binary> | grep <marker>` after building.
