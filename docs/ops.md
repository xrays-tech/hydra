# Hydra — Operations Runbook (v1)

> Audience: SRE / on-call for a single-instance `hydra` deployment.
> Reference: `docs/design.md` (§6 lifecycle, §8 failover/breaker, §11 auth,
> §12 TLS, §13 admin, §15 deploy, §16 security, §17 metrics).

This runbook covers **deployment, configuration, graceful zero-downtime upgrade,
cert rotation, rate-limit tuning, auth-cache invalidation, breaker operations,
troubleshooting**, and the **load baseline** established in wave-6. It is the
single source of truth for ops; the design doc is the source of truth for
behaviour.

---

## 1. Deployment shape (design §15.3)

Hydra ships as a **single static binary** + a `data/` directory (SQLite file +
WAL) + an optional `hydra.toml`. No external database, queue, or cache is
required for v1 (single instance).

```
/opt/hydra/
├── hydra                  # the release binary (self-contained: UI embedded)
├── hydra.toml             # config (NO secrets — token from env)
└── data/
    ├── hydra.db           # SQLite (chmod 0600, §16.2)
    ├── hydra.db-wal
    └── hydra.db-shm
```

Build the release binary:

```bash
cargo build --release --features server
# → target/release/hydra
```

The binary embeds the admin UI at compile time (`include_dir!`), so the
`admin-ui/{index.html,app.js,api-docs.js,style.css}` files are **not** needed on
disk at runtime. The release binary is the only artefact you ship.

### 1.1 Environment variables (single source of truth for runtime knobs)

| Var | Default | Purpose |
|-----|---------|---------|
| `HYDRA_ADMIN_TOKEN` | *(unset)* | **Required.** Admin bearer token (design §13.3). Unset ⇒ admin API denies everything (fail-closed). **Never put this in `hydra.toml`.** |
| `HYDRA_ENCRYPTION_KEY` | *(unset)* | **Required.** Base64 of 32 bytes; AES-256-GCM master key encrypting provider api-keys at rest. Unset ⇒ the binary refuses to start (fail-closed). Generate with `openssl rand 32 \| base64`. Load from an `EnvironmentFile=` (see §1.2); never inline. A matching `HYDRA_ENCRYPTION_KEY_FILE` (raw 32-byte file) is also accepted. |
| `HYDRA_DB_URL` | `sqlite:hydra.db?mode=rwc` | SQLite path. Use `sqlite://./data/hydra.db?mode=rwc` in production. |
| `HYDRA_LISTEN` | `0.0.0.0:8080` | Proxy listener (TLS when any tenant has certs configured, plain TCP otherwise). |
| `HYDRA_ADMIN_ADDR` | `127.0.0.1:8081` | Admin REST + UI + `/metrics` listener. **Bind loopback only** (design §13.3). |
| `HYDRA_USAGE_SINK` | `sqlite` | `sqlite` or `clickhouse`. **Runtime switch — one binary contains BOTH sinks** when built with `--features server,usage-clickhouse` (the release scripts do this), so flipping the sink needs no rebuild. |
| `HYDRA_CLICKHOUSE_URL` | *(unset)* | ClickHouse HTTP endpoint, e.g. `http://hydra-clickhouse:8123` (required when `HYDRA_USAGE_SINK=clickhouse`). **Credentials ARE supported**: use `http://user:pass@host:8123` (sent as HTTP Basic auth) or query params (`?user=&password=`); other query params like `?database=dogress` are passed through verbatim. |
| `RUST_LOG` / `HYDRA_LOG` | `info` | `tracing` env filter. |

> The current binary reads the proxy/admin addresses and DB URL from env (the
> `hydra.toml` shape in design §15.1 is the *target* schema; env vars are the
> *current* mechanism). Prefer env for secrets; `hydra.toml` may carry
> non-secret defaults.

### 1.2 Minimal systemd unit

```ini
[Unit]
Description=Hydra LLM Gateway
After=network.target

[Service]
Type=simple
User=hydra
Group=hydra
WorkingDirectory=/opt/hydra
Environment=HYDRA_ADMIN_TOKEN=__set_via_environment_file__
Environment=HYDRA_ENCRYPTION_KEY=__set_via_environment_file__
Environment=HYDRA_DB_URL=sqlite:///opt/hydra/data/hydra.db?mode=rwc
Environment=HYDRA_LISTEN=0.0.0.0:8080
Environment=HYDRA_ADMIN_ADDR=127.0.0.1:8081
Environment=HYDRA_USAGE_SINK=sqlite
Environment=RUST_LOG=info,hydra=info
# graceful: let in-flight requests drain
KillSignal=SIGQUIT
ExecStart=/opt/hydra/hydra
Restart=on-failure
RestartSec=2
NoNewPrivileges=true
ProtectSystem=strict
ProtectHome=true
PrivateTmp=true
ReadWritePaths=/opt/hydra/data

[Install]
WantedBy=multi-user.target
```

Load the secret from an `EnvironmentFile=` owned by root (`chmod 600`); never
inline it.

### 1.3 SQLite file permissions (§16.2)

Provider api-keys are stored **AES-256-GCM encrypted at rest** (`api_key_ciphertext` /
`api_key_nonce` / `key_version` columns; see `hydra-server::crypto`). They are decrypted
to plaintext only in memory, at the DB boundary, to inject verbatim into upstream
requests. The master key (`HYDRA_ENCRYPTION_KEY`, §1.2) is the primary protection — a
stolen DB file alone is useless without it. Filesystem hardening remains valuable as
defense-in-depth (the encrypted blobs still should not leak):

```bash
install -d -m 0700 -o hydra -g hydra /opt/hydra/data
install -m 0600 /dev/null /opt/hydra/data/hydra.db  # before first start
chown hydra:hydra /opt/hydra/data/hydra.db
```

For higher assurance use SQLCipher or full-disk encryption (design §16.2). The
admin API always returns masked provider keys; `?reveal=1` is accepted as a
no-op for backward-compat but never reveals plaintext (§16.2).

---

## 2. Graceful zero-downtime upgrade (design §15.3)

Pingora has built-in socket handover via `SIGQUIT` (graceful shutdown of the old
process) + `hydra -u` (the new process inherits the listening socket from the
old). In-flight requests on the old process finish; new connections go to the
new process.

```text
        ┌──────────────────────────────────────────────┐
        │  old hydra (pid A) listening on :8080/:8081  │
        └──────────────────────────────────────────────┘
            │
            │  1. operator: kill -SIGQUIT <pid A>
            │     → old process stops accepting, drains
            │
            │  2. operator: hydra -u   (upgrade mode)
            │     → new process asks the old one for the
            │       listening FD via the upgrade socket
            │
            ▼
        ┌──────────────────────────────────────────────┐
        │  new hydra (pid B) listening on :8080/:8081  │
        │  old process exits once all responses flush  │
        └──────────────────────────────────────────────┘
```

### 2.1 Procedure

```bash
# 1. Ship the new binary to /opt/hydra/hydra.new
# 2. Atomically swap:
mv /opt/hydra/hydra.new /opt/hydra/hydra

# 3. Tell systemd (or your supervisor) to upgrade. With Pingora's built-in
#    upgrade, the equivalent is:
kill -SIGQUIT $(pidof hydra)        # old process: graceful drain
hydra -u &                          # new process: inherit socket
```

If you supervise with systemd and want it to manage the upgrade, set
`KillSignal=SIGQUIT` (see §1.2) so a normal `systemctl restart` sends SIGQUIT;
then chain `ExecStartPost`/`ExecReload` as appropriate for your wrapper.

### 2.2 Caveats

- **Upgrade socket path**: Pingora's upgrade socket (`upgrade_sock`) must be on
  a path writable by both old and new processes. In containers with a read-only
  rootfs, mount a small tmpfs at the upgrade-sock path (design wave-6 §6 risk
  note). If the path is not writable, the new process will fail to take over
  the port with `address already in use`.
- **Config drift across upgrade**: the new process re-reads `hydra.toml`/env on
  boot. If you changed env vars, set them before step 2.
- **DB schema**: SQLite migrations run on boot. A forward-only migration is
  safe during upgrade (the old process keeps its connection; the new process
  opens a fresh pool and runs migrations). A backward-incompatible migration
  blocks rollback — keep the previous binary until you're confident.

### 2.3 Verifying an upgrade (smoke)

```bash
# Before: continuous low-RPS probe through the proxy.
hey -z 60s -c 4 https://acme.example.com/v1/chat/completions ...

# During: run the upgrade. The probe must show zero non-2xx from connection
# resets and a brief (sub-second) pause as the new process binds.

# After: GET /api/v1/health → 200, /metrics → counter continuity.
```

---

## 3. Certificate rotation (design §12.1, W4b)

Downstream TLS certs are configured **per tenant** via the `tenants` table
(`cert_file`, `cert_key` — absolute paths or relative to `data/`). Hydra keeps a
single `ArcSwap`'d map of `(sni_host → (cert, key))` resolved from the snapshot
and consults it in the SNI cert callback.

### 3.1 Updating a tenant's cert (hot — no restart)

```bash
# 1. Drop the new cert/key on disk.
install -m 0600 acme.2026.crt /opt/hydra/data/certs/acme.crt
install -m 0600 acme.2026.key /opt/hydra/data/certs/acme.key

# 2. PUT the tenant (pointing cert_file/cert_key at the new files):
curl -X PUT http://127.0.0.1:8081/api/v1/tenants/t1 \
  -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" \
  -H "content-type: application/json" \
  -d '{"id":"t1","name":"Acme","domain":"acme.com","auth_url":"https://auth.acme.com/v","cert_file":"data/certs/acme.crt","cert_key":"data/certs/acme.key","enabled":true,"created_at":"x","updated_at":""}'
```

The PUT triggers `ConfigStore::reload_all()` and the W4b cert-reload contract
re-resolves every cert path from the fresh snapshot. **New** TLS handshakes use
the new cert; **existing** connections are unaffected (they keep the cert they
negotiated).

### 3.2 Verifying

```bash
# New connection should present the new cert (check notAfter / fingerprint):
echo | openssl s_client -connect acme.example.com:443 -servername acme.com 2>/dev/null \
  | openssl x509 -noout -dates -fingerprint -sha256
```

### 3.3 Forcing a manual reload (without changing data)

```bash
curl -X POST http://127.0.0.1:8081/api/v1/reload \
  -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" -d '{}'
```

`POST /api/v1/reload` re-runs `reload_all()` and re-resolves certs. Returns the
new snapshot counts (tenants/providers/models/keys/certs). On a fatal reload
error the old snapshot + old certs are retained and the endpoint returns 400
(design §5.3).

---

## 4. Rate-limit tuning (design §10, §15.1 `[limit]`)

Limits are configured as **roles** in the `limit_roles` table. Each role carries
`matching_*` dimensions (any `NULL` = match-all on that dimension), a
`limit_count` and/or `limit_token` ceiling, and a `window` (`m` / `h` / `d`).
The matcher selects roles for a request; the most restrictive surviving role
applies.

### 4.1 Common recipes

```bash
# Per-tenant, requests per minute:
curl -X POST .../api/v1/limit-roles -H "Authorization: Bearer $T" -d '{
  "id":"r-acme-rpm","name":"Acme 600/min","matching_tenant":"t-acme",
  "matching_key":null,"matching_model":null,"matching_provider":null,
  "limit_count":600,"limit_token":null,"window":"m","enabled":true,"created_at":""
}'

# Per-(tenant,model) token-per-day ceiling:
curl -X POST .../api/v1/limit-roles -H "Authorization: Bearer $T" -d '{
  "id":"r-acme-gpt4-tpd","name":"Acme gpt-4 1M tok/day",
  "matching_tenant":"t-acme","matching_model":"gpt-4",
  "limit_count":null,"limit_token":1000000,"window":"d","enabled":true,"created_at":""
}'
```

### 4.2 Tuning notes

- **Windows are sliding** (in-memory counters GC'd every 30 s). A `429` returns
  `Retry-After` reflecting the remainder of the current window.
- **`limit_token`** only applies when the upstream returns a `usage` object
  (streaming needs `stream_options.include_usage` for OpenAI; design §9.4).
- **Soft-disable a role**: `enabled=false` (still listed but not matched).
- **Multi-instance limitation (v1)**: counters are per-process. Multi-instance
  deployments need Redis-backed counters (v2 candidate, §16.6).

---

## 5. Auth-cache invalidation (design §11.7 / §13.2)

Hydra caches `sha256(api_key) → verdict` per `(tenant, key)` with a TTL (default
allow 5 min / deny 30 s). The tenant auth service can force a re-check by
invalidating entries:

```bash
# Invalidate specific keys for a tenant:
curl -X DELETE http://127.0.0.1:8081/api/v1/auth/cache \
  -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" \
  -d '{"tenant_id":"t-acme","api_keys":["sk-aaa","sk-bbb"]}'
# → {"invalidated":2,"tenant_id":"t-acme"}

# Invalidate ALL keys for a tenant (e.g. suspected key compromise):
curl -X DELETE .../api/v1/auth/cache -d '{"tenant_id":"t-acme"}'
```

Response: `{ "invalidated": N, "tenant_id": "..." }`. The next request for an
invalidated key re-hits the tenant `auth_url`. The `hydra_auth_cache_size` gauge
is refreshed after mutation (§17).

> **Security trade-off**: within the cache window, a key revoked by the tenant
> side can still pass. Shorten `[auth] allow_ttl_secs` or call invalidate
> proactively on tenant-side revocation (design §16.1).

---

## 6. Circuit-breaker operations (design §8.4)

A provider enters the **dead-set** after `threshold` (default 5) **consecutive**
failures. Dead providers are excluded from candidate selection (router §7.1
step 4). A background probe task pings `GET {endpoint}/v1/models` every
`probe_interval` (default 10 s) with a 1.5 s timeout; on success it revives the
provider (clears the streak + dead-set). A bare TCP connect is the fallback when
HTTP probing itself errors.

### 6.1 Inspect / force reset

```bash
# List dead providers:
curl http://127.0.0.1:8081/api/v1/breaker -H "Authorization: Bearer $T"
# → { "dead": ["p-openai-failover"] }

# Force-reset a provider (e.g. after a known fix, before the probe catches up):
curl -X DELETE http://127.0.0.1:8081/api/v1/breaker/p-openai-failover \
  -H "Authorization: Bearer $T"
# → { "reset": "p-openai-failover", "was_dead": true, "dead": [] }
```

### 6.2 `status = -1` semantics (design §8.4)

`provider_model.status` is `1` (online) / `0` (manually offline) / `-1`
(probe-offline). The candidate builder only includes `status == 1`. **Today**
the breaker keeps `dead` purely in memory (no write amplification on the hot
path); the optional slow-cycle task that would mirror `dead → status=-1` into
the DB is a v1 stretch item. Treat the **admin API `breaker` dead-set** as the
authoritative "live" view, and `status` as a human override.

### 6.3 Probe strategy & tuning

- The HTTP probe considers **any HTTP response** (even a 401/429) as "host
  alive" — only connection-level failures count as "still dead". This is
  intentional: a 401 from `/v1/models` means the upstream is up, just
  unauthenticated for that path.
- A bare TCP connect is the fallback when the HTTP probe itself errors (DNS,
  TLS, timeout). Use this when you can't expose `/v1/models`.
- Tune `[breaker] threshold` lower (e.g. 3) for aggressive failover; raise it
  (e.g. 10) if your upstream has bursty errors. `probe_interval` shorter than
  your mean-time-to-recover shortens the dead window at the cost of probe load.

---

## 7. ~~⚠️ `retry_after_connect`~~ — duplicate-billing risk (**已删除，见 terminate-mode**)

> **此配置项已在 terminate-mode 重写中删除。** 以下内容保留作为历史参考。
>
> Terminate-mode（当前实现）的故障转移是一个**简单 `for candidate in candidates { try send; on fail continue; }` 循环**：全 body 已缓存（`Bytes`），重放零成本（`Bytes::clone` O(1)）。失败时 `breaker.on_failure` + `record_retry("terminate_loop")`，成功则 `breaker.on_success`。
>
> 不再有 `retry_after_connect` 配置、不再有 `upstream_bytes_seen` / `body_too_large` 守卫、不再有 Pingora 的 `set_retry` / `fail_to_connect` / `error_while_proxy` 钩子。详见 `docs/design-change-terminate-mode.md` §4.3。

> **READ THIS BEFORE ENABLING.** This is the single most dangerous knob in
> Hydra. ~~（已删除）~~

```toml
[failover]
retry_after_connect = false   # DEFAULT — safe
```

### What it does

When `true`, Hydra retries a request on the **next candidate** if the upstream
errors **after the TCP/TLS connection was established** but **before any byte of
the response body was seen** (`upstream_bytes_seen == 0`).

### Why it's dangerous

Streaming LLMs spend **seconds** between "connection accepted" and "first byte"
(prompt processing). During that window:

- The upstream may have **already processed and billed** the request.
- A network blip (RST, idle timeout) then triggers a retry.
- The retried request is sent to a different provider instance and **billed
  again**.

This produces **double billing for a single user request**. There is no way for
Hydra to know whether the upstream billed during that silent window.

### When you might accept the risk

- Your upstream bills only on **completed** responses (not on prompt receipt),
  **and**
- You can tolerate rare double-counts in exchange for higher availability on
  mid-stream connection drops.

### Default and recommendation

- **Default is `false`** (safe; no retry after connect). Keep it that way unless
  you have explicitly accepted the duplicate-billing risk.
- The second guard (`upstream_bytes_seen == 0`) prevents the *catastrophic*
  case of retrying after streaming has begun, but the prompt-window case above
  is still billable.
- **`body_replayable`** is a third guard: bodies larger than `[proxy]
  max_request_body` (soft cap) are not buffered for replay, so retry is
  disabled for them regardless of this flag (§8.5).

---

## 8. `[proxy] max_request_body` vs failover (**已更新为 terminate-mode**)

> Terminate-mode 读取**全请求体**（不再有 stream-through 的"软上限禁用重放"机制）。当前仅保留 **`max_request_body_hard`**（硬上限）作为防护；`max_request_body`（软上限）/ `body_too_large` / `error_while_proxy` 的 `body_replayable` 守卫**均已删除**。未来如需限制全 body 缓冲内存，可加 `max_body` → 415（未来增强）。

Two body caps ~~interact with failover~~ （terminate-mode 下只剩硬上限）:

| Cap | Default | Effect when exceeded |
|-----|---------|----------------------|
| ~~`max_request_body` (soft)~~ | ~~8 MiB~~ | **已删除（terminate-mode 不使用）**：terminate-mode 读全 body，故障转移用 `Bytes::clone` O(1) 重放，无"软上限禁用重放"机制。 |
| `max_request_body_hard` | 32 MiB | **413 Payload Too Large** immediately, connection closed (`set_keepalive(None)`, §6.7). 在 `request_filter` 全 body 读取循环中检测。 |

**Trade-off**（terminate-mode）：~~更大的软上限意味着更多请求可以安全故障转移（有利于可用性）~~ **不再适用**——全 body 已缓存，所有候选都能 O(1) 重放。内存占用 = 并发请求数 × 平均 body 大小（500 并发 × 2MB avg ≈ 1GB）。如需降低内存峰值，调低 `max_request_body_hard`（超过即 413）。

> ~~H2 paths are truly zero-copy on the forward leg; H1 paths incur one kernel copy per chunk (Pingora core limitation, design §8.5).~~ **（已废弃）** Terminate-mode 放弃 kernel-level 零拷贝（body 经 userspace buffer 传给 reqwest），但保留"零 JSON 往返"（body 字节未被 serde 处理）。详见 `docs/design-change-terminate-mode.md` §5。

---

## 9. Observability (design §17, implemented W5)

- **`/metrics`** (self-hosted, no sidecar): Prometheus exposition on the admin
  port. Key series: `hydra_requests_total`, `hydra_request_duration_seconds`,
  `hydra_upstream_duration_seconds`, `hydra_retries_total`,
  `hydra_tokens_total`, `hydra_auth_decisions_total`, `hydra_auth_cache_size`,
  `hydra_breaker_dead`, `hydra_breaker_state_transitions_total`,
  `hydra_limit_rejected_total`, `hydra_sni_host_mismatch_total`,
  `hydra_route_errors_total`, `hydra_mid_stream_errors_total`.
- **Tracing**: structured logs via `tracing` (`RUST_LOG`). Every request carries
  an `X-Hydra-Trace-Id` echoed to the client and logged end-to-end.
- **Admin UI**: `http://<admin_addr>/admin/` — same-origin, in-memory token
  prompt. Useful for incident inspection (breaker dead-set, health, manual
  reload, key reveal with audit log).

> **Mid-stream failures are not retried.** Streaming responses that fail AFTER
> the `200` + first byte are sent cannot be retried (sent bytes cannot be
> unsent); the connection is closed, the failure is counted in
> `hydra_mid_stream_errors_total{provider}`, and it still feeds the circuit
> breaker.

---

## 10. Troubleshooting

### 10.1 SNI / Host mismatch (§12.3, `hydra_sni_host_mismatch_total`)

A TLS request where the **SNI** (TLS layer) does not match the **Host** /
`:authority` (HTTP layer) is suspicious (domain-fronting). Hydra increments
`hydra_sni_host_mismatch_total` and falls back to the **Host** header for tenant
resolution. If you see sustained non-zero counts:

- A client behind a CDN/front is domain-fronting (often benign, sometimes
  policy violation).
- A misconfigured client is sending the wrong SNI.
- A cert was installed against the wrong domain.

Inspect via the admin UI **Health** tab + grep logs for
`target="hydra::tls"` `sni_host_mismatch`.

### 10.2 macOS SSE flush caveat (Pingora Issue #841)

> **Local-dev only.** On **macOS**, streaming (SSE/chunked) responses may not
> flush incrementally to the client — they can buffer until the stream ends.
> This is a known Pingora issue (#841) and **does not affect Linux
> production**. For local SSE testing, use a Linux container
> (`docker run --rm -it debian:slim …`) or a Linux VM. Do not chase this as a
> bug in your Hydra code.

### 10.3 Admin API returns 401 for everything

- `HYDRA_ADMIN_TOKEN` is unset on the server → fail-closed (design §13.3). The
  startup log explicitly warns: *"admin REST API bound but HYDRA_ADMIN_TOKEN is
  unset — all admin requests will be denied"*.
- You're sending the wrong scheme. Hydra expects `Authorization: Bearer
  <token>` exactly (case-insensitive on `bearer`). Basic auth is not supported.
- The admin listener is bound to loopback (`127.0.0.1:8081`) and you're
  reaching it via the proxy port (`:8080`) — the proxy doesn't serve `/api/v1`.

### 10.4 Provider never receives traffic

Check, in order:

1. **Breaker dead-set** (`GET /api/v1/breaker`) — is the provider listed? If so,
   force-reset or wait for the probe.
2. **`weight`** — is it `0`? Weight 0 = soft-disabled (§7.2).
3. **`status`** — is the model `status == 1`? `0`/`-1` are excluded from
   candidates.
4. **TenantModel gate** — does the tenant have a `tenant_models` mapping? **No
   mapping ⇒ all models allowed (default-open, §7.1)**. Mapping present but the
   model absent → 403 `model_not_allowed`.
5. **TenantProvider** — does the tenant have access to the provider? Empty
   intersection → `no_available_provider`.
6. **api_key** — does the provider have at least one key? No key → filtered
   out.

The admin UI surfaces all of these; `hydra_route_errors_total{reason=…}` tells
you which gate is firing in aggregate.

### 10.5 SQLite is locked / busy

`PRAGMA busy_timeout = 5000` is set on init. If you still see `database is
locked`, you have contention from a second writer (e.g. another `hydra`
instance against the same file, or an external script). v1 is single-instance;
do not point two `hydra` processes at the same SQLite file.

### 10.6 Upgrade fails with "address already in use"

The new process could not take over the listening socket from the old one.
Causes:

- The upgrade socket path is not writable (container with read-only rootfs;
  mount a tmpfs there — wave-6 §6).
- You started the new process with `-u` while the old one was already gone.
- The old process was killed with `SIGKILL` (not `SIGQUIT`) and didn't hand off
  the socket. Always use `SIGQUIT` for graceful drain.

---

## 11. Load baseline (wave-6 §2.4)

The wave-6 load harness lives in `scripts/load_test.sh` (orchestrates `oha`
against a running instance with a mock upstream) and `crates/hydra-server/
tests/load_breaker_swrr.rs` (a Rust integration test that asserts SWRR weight
distribution and breaker-under-failure avoidance without external tools).

### 11.1 Recorded baseline (single-instance, dev box)

> Replace with your own numbers from `scripts/load_test.sh` on the production
> host. These are reference figures from the wave-6 dev environment against a
> local `wiremock` upstream (so the upstream, not Hydra, is the bottleneck —
> expect higher RPS against a real LLM endpoint on a tuned host).

| Scenario | RPS | P99 | Notes |
|----------|-----|-----|-------|
| SWRR 3:1 distribution (echo upstream) | — | — | Distribution matches weights within ±2% over 1000 req (see `load_breaker_swrr.rs`). |
| Breaker under failure | — | — | Dead upstream receives 0 requests after `threshold`; revives on probe. |
| Auth-cache hit (cached allow) | *measure* | *measure* | Sub-ms added latency; no upstream auth call. |
| Auth-cache miss (wiremock auth) | *measure* | *measure* | One extra round-trip to `auth_url`. |

Run `scripts/load_test.sh` against a staging instance to populate the numeric
cells for your environment; record them here as the v1 regression baseline.

### 11.2 Memory / leak check

The wave-6 harness holds concurrent SSE streams and watches RSS. AuthCache and
limiter windows are GC'd by background tasks (every 30 s for the limiter;
TTL-expiry sweep for the cache). RSS should be flat under sustained load; a
rising trend indicates either a real leak (file an issue) or a cache whose TTL
is longer than the test window.

---

## 12. v1 boundaries (what NOT to expect)

- **Single instance is the default, not the ceiling.** The default single-node
  mode remains one process with a local SQLite; **cluster mode is now
  implemented** (Redis-backed, see §13): multi-instance with shared rate-limit
  counters, shared circuit breaker, shared auth cache L2 and a leader-lease
  failover is available via `HYDRA_ROLE=leader|edge` — no longer a v2 backlog
  item (design §16.6 updated).
- **Single static admin token.** No RBAC, no token rotation (v2, §16.6 / §13.3).
- **No web UI auth** beyond the in-memory token prompt. The UI is a power-user
  tool; for fleet management use the REST API.
- **SQLite is the only bundled config store.** ClickHouse is supported as an
  optional usage sink (mandatory in cluster mode); PostgreSQL/MySQL are not
  supported for the config DB.

For the remaining v2 backlog see design §16.6.

---

## 13. Cluster mode operations (design §20 / docs/cluster.md)

Cluster mode is opt-in (`HYDRA_ROLE=leader|edge`) with **Redis as the only
external dependency** (K8s/k3s-agnostic, self-sustaining). The authoritative
reference is **[`cluster.md`](cluster.md)** — env table, shared-state
map, Redis failure matrix, deploy manifests, failover drill and the live
acceptance record (§5.1). This section is the runbook-level index.

### 13.1 Build

```bash
cargo build --release --features server,cluster-redis,usage-clickhouse
# single-node builds stay feature-free: cargo build --release --features server
```

### 13.2 Minimal leader pair + edge (compose)

```bash
cd environment
export HYDRA_ADMIN_TOKEN=admin-secret
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # SAME on every node
docker compose -f docker-compose.cluster.yml up -d --scale hydra-edge=2
curl -H "Authorization: Bearer admin-secret" http://localhost:8081/api/v1/tenants
```

k3s / k8s manifests and bare-metal systemd live in `docs/cluster.md` §4.

### 13.3 Cluster environment variables (quick map)

| Variable | Notes |
|---|---|
| `HYDRA_ROLE` | `leader` / `edge`; unset = single-node (unchanged behavior) |
| `HYDRA_REDIS_URL` / `HYDRA_REDIS_MODE` | backbone; `single` wired, sentinel/cluster fail-fast |
| `HYDRA_CLUSTER_TOKEN` | shared control-channel token (all nodes) |
| `HYDRA_CONTROL_URL` / `HYDRA_PUBLIC_URL` | active control endpoint (snapshot polling) / this node's registered URL. `HYDRA_CONTROL_URL` is **not** the admin-mutation forward target — a standby forwards writes to the ACTUAL lease holder, resolved live from the registry (self-forward/mutual-forward loop guards; see `docs/cluster.md` §5.2) |
| `HYDRA_ADMIN_TOKEN` | required on leaders, shared cluster-wide |
| `HYDRA_ENCRYPTION_KEY` | master key, identical fleet-wide |
| `HYDRA_USAGE_SINK=clickhouse` | mandatory in cluster mode (+ `HYDRA_CLICKHOUSE_URL`) |
| `HYDRA_LEADER_LEASE_MS` / `HYDRA_CONTROL_POLL_MS` | 15000 / 1000 defaults |

### 13.4 Failover drill

```bash
for p in 8081 8082; do curl -s -o /dev/null -w "port $p: %{http_code}\n" localhost:$p/healthz/leader; done
docker compose -f docker-compose.cluster.yml stop hydra-control-a   # kill the active
curl -s localhost:8082/healthz/leader    # → 200 after ≤ ~20s (measured 11–18s)
docker compose -f docker-compose.cluster.yml start hydra-control-a  # rejoins as standby
```

Edges and standbys follow the new active automatically (registry rotation +
lease-aware rotation); a rejoining leader rebuilds its replica from the current
active (no shared volume). Admin writes on a standby are forwarded to the
actual lease holder (registry-resolved, with a forward-once loop guard) — you
can point the admin UI at ANY leader candidate, including one whose
`HYDRA_CONTROL_URL` points at itself. Full checklist: `docs/cluster.md` §5.

### 13.5 Redis outage behavior

Data plane keeps serving (last-known-good snapshot + local caches). Election is
**fail-closed**: a leader that cannot renew demotes immediately (writes stop)
until Redis recovers. See `docs/cluster.md` §3 for the full matrix.

### 13.6 Known limitations (as of the 2025-08 acceptance)

- Disabled `limit_role` / `provider_key_binding` rows are not carried in config
  snapshots (`build_config` keeps enabled rows only) — after a failover they are
  lost from replicas and must be re-created.
- `HYDRA_FAILOVER_GRACE_MS` is documented but not wired; `HYDRA_BREAKER_QUORUM`
  and `HYDRA_RATE_LIMIT_FAIL_MODE` use in-code defaults.
- Redis sentinel/cluster deployment modes fail fast (single mode wired).
---

## 14. GitHub pull fails: `GnuTLS recv error (-110)` (HTTPS over unstable links)

> **Finalized fix (2026-08, verified on dev box + test server).** Symptom:
> `git pull`/`git fetch` from GitHub over HTTPS intermittently dies with
> `GnuTLS recv error (-110): The TLS connection was non-properly terminated`.
> Root cause (test server `172.16.48.71`): `github.com:443` is **intermittently
> dropped at the TCP layer** by the network (probes: 3/3 TCP fails then OK;
> `github.com:22`/`:80` and `api.github.com:443` always reachable) — git
> client config cannot fix L3/L4 drops. **Decision: use SSH for GitHub on the
> test server** (port 22 is stable and the host key is registered), keep the
> HTTP/1.1 config below as an HTTPS fallback.
>
> **Test-server record (2026-08-27):** global HTTP/1.1 config applied; the
> hydra checkout `/opt/ru_deployer/src/hydra/main` origin switched to
> `git@github.com:xrays-tech/hydra.git`; verified with a real fetch
> (`7af8a17..a4440a4`) + 8/8 consecutive fetches over SSH.

### 14.1 Apply (simplest fix — no side effects, covers every repo)

```bash
# 1. Force HTTP/1.1 (avoids the HTTP/2 multiplexing disconnect bug — the
#    single most effective lever), bump the send buffer, and disable the
#    low-speed abort so brief network dips do not kill the transfer:
git config --global http.version HTTP/1.1
git config --global http.postBuffer 1048576000   # 1 GiB
git config --global http.lowSpeedLimit 0          # 0 = check disabled
git config --global http.lowSpeedTime 999999

# 2. Verify the values took effect:
git config --global --list | grep -i '^http'
```

### 14.2 Verify it is really using HTTP/1.1

```bash
GIT_CURL_VERBOSE=1 git fetch origin 2>&1 | grep -E 'ALPN|HTTP/[0-9.]+ [0-9]{3}'
# Expect: "ALPN: server accepted http/1.1" and "HTTP/1.1 200 OK" lines.
# Stability smoke: run the fetch in a loop until you are confident:
for i in $(seq 1 8); do git fetch origin >/dev/null 2>&1 && echo "$i OK" || echo "$i FAIL"; done
```

### 14.3 If it still recurs (escalation ladder)

1. **Switch origin to SSH** (bypasses HTTPS/TLS entirely — most robust for
   automation; requires a GitHub SSH key on the host):
   `git remote set-url origin git@github.com:xrays-tech/hydra.git`
2. **Shallow fetch** when the history is large: `git fetch --depth=1 origin main`
   (later `git fetch --unshallow` on a good link).
3. **Force IPv4**: `git fetch -4 origin main`.
4. **MTU tuning** (physical-link packet loss): `sudo ip link set dev <iface> mtu 1360`.

