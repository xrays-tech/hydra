# Hydra — Operations Runbook (v1)

> Audience: SRE / on-call for a single-instance `hydra` deployment.
> Reference: `dev-docs/design.md` (§6 lifecycle, §8 failover/breaker, §11 auth,
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
| `HYDRA_LISTEN` | `0.0.0.0:8080` | Proxy **plaintext** listener. Always bound — the listener topology is derived from configuration only, never from whether tenants have certs (see `dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`). |
| `HYDRA_TLS_LISTEN` | *(unset)* | Optional proxy TLS listener, e.g. `0.0.0.0:8443`. **Setting this is what enables HTTPS** — per-tenant certificates are then selected by SNI. Unset with tenant certs present ⇒ the certs are NOT served (logged as an error + `hydra_listener_misconfig_total`); set but the address cannot be bound ⇒ plaintext keeps serving and the failure is logged + counted. Must differ from `HYDRA_LISTEN`. |
| `HYDRA_ADMIN_ADDR` | `127.0.0.1:8081` | Admin REST + UI + `/metrics` listener. **Bind loopback only** (design §13.3). |
| `HYDRA_USAGE_SINK` | `sqlite` | `sqlite` or `clickhouse`. **Runtime switch — one binary contains BOTH sinks** when built with `--features server,usage-clickhouse` (the release scripts do this), so flipping the sink needs no rebuild. |
| `HYDRA_CLICKHOUSE_URL` | *(unset)* | ClickHouse HTTP endpoint, e.g. `http://hydra-clickhouse:8123` (required when `HYDRA_USAGE_SINK=clickhouse`). **Credentials ARE supported**: use `http://user:pass@host:8123` (sent as HTTP Basic auth) or query params (`?user=&password=`); other query params like `?database=dogress` are passed through verbatim. |
| `RUST_LOG` / `HYDRA_LOG` | `info` | `tracing` env filter. |
| `HYDRA_TENANT_API` | `on` | Master switch for the tenant API on the data plane (`/tenant/…`). `off`/`0`/`false` ⇒ the prefix is not intercepted at all and the process behaves exactly as before the API existed. |
| `HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS` | `2000` | How long `auth/cache/invalidate` waits for the fleet to confirm before answering `202` with `lagging`. |
| `HYDRA_TENANT_API_RATE_LIMIT_PER_MIN` | `60` | Per-tenant cap on AUTHORISED requests (429 beyond it) — counts every authenticated request the node accepts for the tenant, including ones it then rejects with 4xx/5xx (only the 403 URL-tenant mismatch is exempt). The amplification budget: one tenant's credential must not be able to spend other tenants' availability. |
| `HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN` | `10` | Cap on FAILED authentications, per **source IP** and per **token digest** independently. |
| `HYDRA_TENANT_API_LOCKOUT_SECS` | `900` | Once a dimension exceeds its failure budget it is locked for this long. **The lockout is consulted only on the authentication-failure path**: a request with a valid token is never refused by it, so a locked-out guesser **can** distinguish `429` (wrong token) from `200` (right token) — the no-validity-oracle property is deliberately given up (marginal guessing value ≈ 0 at ≥16-char tokens, and the `403` URL-tenant-mismatch channel already exposes validity). |
| `HYDRA_TRUSTED_PROXIES` | *(unset)* | Comma-separated **IP or CIDR** allowlist of reverse proxies whose `X-Forwarded-For` is trusted (IPv4/IPv6; a bare IP is treated as /32 or /128). Unset/empty = trust nobody (use the socket peer IP — the conservative default). When the peer is trusted, the per-IP limiter reads **all** `X-Forwarded-For` header lines (in order) and keys on the **rightmost** address that is not itself a trusted proxy, falling back to the peer when there is no `X-Forwarded-For` / all entries are trusted proxies / any entry is invalid. **A malformed entry fails startup.** Misconfiguration risk: trusting a proxy that does not strip inbound `X-Forwarded-For` lets a client forge it and rotate its per-IP bucket, effectively disabling the per-IP dimension. |
| `HYDRA_TENANT_API_INVALIDATE_PER_MIN` | `10` | Per-tenant invalidation cap (429 beyond it). Each one fans out to every node and re-hits the tenant's `auth_url` from all of them. |
| `HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS` | `31` | E3 window ceiling. Not cosmetic: the ClickHouse table's key leads with `created_at`, so a wide window scans every tenant's rows in it. |
| `HYDRA_TENANT_CONFIG_WRITE_PER_MIN` | `60` | Per-tenant cap on **tenant self-service config writes** (sub-tenant / route CRUD, §5.5 v2), enforced on the **leader** (`429 too_many_requests` beyond it). A missing / zero / unparseable value falls back to 60 (never 0, which would refuse every write). Anti-DoS only; see the failover-reset note in §5.5. |
| `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` | `300` | Ceiling on an **allow** entry's TTL, including one a tenant asked for via `expires_in`. Bounds how long a revoked key can keep working on a node that missed the invalidation. Fails startup on a non-positive value. |
| `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` | `5000` | Deadline for an E3 read, independent of the writer's `HYDRA_CLICKHOUSE_IO_TIMEOUT_MS`. |

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
allow 5 min / deny 30 s). Exception (2026-09-09): **402 insufficient-balance
denials are never cached** — balance is fast-changing, and a cached 402 would
degrade into a 401 within the deny TTL (design §11.3). The tenant auth service
can force a re-check by invalidating entries:

```bash
# Invalidate specific keys for a tenant:
curl -X DELETE http://127.0.0.1:8081/api/v1/auth/cache \
  -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" \
  -d '{"tenant_id":"t-acme","api_keys":["sk-aaa","sk-bbb"]}'
# → {"invalidated":2,"tenant_id":"t-acme"}

# Invalidate ALL keys for a tenant (e.g. suspected key compromise):
curl -X DELETE .../api/v1/auth/cache -d '{"tenant_id":"t-acme"}'
```

Response (2026-09-17): the flat `invalidated`/`tenant_id` body gained
`checked` / `scope` / **`fleet`**, and the `published: bool` field is GONE:

```jsonc
{"invalidated":2,"checked":2,"tenant_id":"t-acme","scope":"keys",
 "fleet":{"state":"applied","nodes_total":3,"nodes_applied":3,
          "lagging":[],"event_id":"1737-0","waited_ms":41}}
```

`fleet.state` is **`applied`** (200 — every live node confirmed), **`pending`**
(202 — published, not all confirmed; `lagging` names the laggards, or is **empty** when `nodes_total=0` because the node could not enumerate the live fleet), **`single_node`**
(200 — this node is the whole data plane) or **`unavailable`** (503 — the channel
exists but did not answer; **the fleet was NOT told**).

`published` was removed because it could not be false when there was no
invalidation stream: it defaulted to `true` and only flipped on a publish error, so
a single-node build asserted a broadcast that never happened. Never treat a 202 as
"done" — retry or investigate the named `lagging` nodes. The next request for an
invalidated key re-hits the tenant `auth_url`. The `hydra_auth_cache_size` gauge
is refreshed after mutation (§17).

> **Security trade-off**: within the cache window, a key revoked by the tenant
> side can still pass. Shorten `[auth] allow_ttl_secs` or call invalidate
> proactively on tenant-side revocation (design §16.1).

> 租户侧的完整对接说明（字段表、错误码、边界、排障）在
> [`tenant-api-integration.md`](tenant-api-integration.md) —— 给租户看，不是给你看的。
> 本节只讲运维关心的部分。

### 5.1 Tenant self-service API — **on the DATA plane** (2026-09-17)

Each tenant can be given an **Access Token** (admin UI Tenants form →
`Access token` field + Generate button; stored as a SHA-256 hash, never echoed,
rotate by setting a new value). The tenant then serves itself, on the
**data-plane listener** (`HYDRA_LISTEN`, i.e. the port its clients already use):

| endpoint | what it does |
| --- | --- |
| `GET  /tenant/{tid}/api/v1/whoami` | the tenant's own non-secret config + snapshot version |
| `POST /tenant/{tid}/api/v1/auth/cache/invalidate` | clear the tenant's cache **across every data-plane node** |
| `GET  /tenant/{tid}/api/v1/usage?since=…&until=…&group_by=…` | token usage in a window (SQLite single node, ClickHouse in a cluster) |

```bash
# 欠费停机 / 付费恢复: force re-auth for one key, everywhere
curl -X POST http://<data-plane-addr>/tenant/t-acme/api/v1/auth/cache/invalidate \
  -H "Authorization: Bearer <tenant-access-token>" \
  -H "content-type: application/json" \
  -d '{"api_keys":["sk-aaa"]}'     # optional; empty body = clear ALL for the tenant
→ {"invalidated":1,"checked":1,"tenant_id":"t-acme","scope":"keys",
   "fleet":{"state":"applied","nodes_total":3,"nodes_applied":3,"lagging":[],"event_id":"1737-0","waited_ms":41}}
```

**Two optional query parameters control the wait** (design §4.2.2 / Q10):

| parameter | values | effect |
|---|---|---|
| `wait` | `converged` (default) \| `none` | `none` publishes and answers `202` immediately with the `event_id` — use it in bulk scripting where blocking per call is not worth it, then reconcile later |
| `timeout_ms` | `1`..`60000` | overrides `HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS` for this one request |

Both are **validated, not tolerated**: an unknown `wait` is `400 invalid_wait` and an
out-of-range `timeout_ms` is `400 invalid_timeout_ms`. A misspelt value that behaved
like the default would leave you believing you had asked for (or skipped) a wait you
never got.

> `wait=none` returning `202` with `"lagging"` listing the nodes is **not** a claim
> that those nodes are behind — nobody looked. Treat it as "in flight" and reconcile
> with `event_id`. If you need to know, use the default.

**The old management-plane route is deleted.** It was
`POST /api/v1/tenants/{id}/auth/cache/invalidate`, and it ran *before* the admin
gate — so exposing it meant exposing every operator endpoint on the same port,
which is why the management port can stay bound to loopback.

Identity comes from the **token only**: the URL's `{tenant_id}` is a
cross-check (mismatch → 403 `tenant_id_mismatch`), and `Host` plays no part.
An invalid/missing/unconfigured token is 401 (fail-closed), worded identically
for "no token" and "wrong token". Lost token ⇒ the operator rotates it (the API
never returns it).

The failure budget is a **lockout**, not just throttling: when a dimension (per
**source IP**, per **token digest**, or per **tenant**) exceeds its budget it is
locked for `HYDRA_TENANT_API_LOCKOUT_SECS` (default 900), and once that failure
window rolls the attacker can re-trip it (~11 requests / 15 min) — so the
interruption is effectively renewable. Under B1 the lockout is consulted **only on
the authentication-failure path**: a request carrying a **valid token is never
refused** by it. The old whole-API blackout for co-located tenants therefore no
longer occurs — what remains is that **failed** attempts from clients sharing an
egress IP are counted together. The budget keys on the **socket peer IP**, never
`X-Forwarded-For` (a caller-controlled header must not choose its own bucket);
behind a load balancer every request shares the balancer's address, so the per-IP
budget is effectively fleet-wide there. **Do not** "fix" legitimate-client
throttling by raising `HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN` — raising it
proportionally raises the token-guessing budget (the dimension's purpose), so it is
not a free mitigation. Instead configure `HYDRA_TRUSTED_PROXIES` when behind a
trusted LB (so the per-IP dimension keys on the real client), and/or rate-limit per
real client IP at the LB/WAF. All windows are per-process, so an N-node fleet
allows N times the configured rate; the per-node bound is what the fan-out actually
depends on.

> **`HYDRA_TRUSTED_PROXIES` deployment checklist (read before setting it):**
> - **Confirm the LB both appends and strips.** It must **append** the real client IP to `X-Forwarded-For` **and** **strip/override any inbound `X-Forwarded-For`** from the client. Trusting a proxy that forwards a client-supplied XFF unmodified lets clients forge the header and rotate limiter buckets — the per-IP budget is **silently defeated**.
> - **Multiple header lines are handled.** The node reads **all** `X-Forwarded-For` header lines (in order) and walks right-to-left skipping trusted proxies, so both the merged-single-header form and the separate-line append form (common in HAProxy) resolve correctly.
> - **Catch-all disables the per-IP dimension.** A `0.0.0.0/0` or `::/0` entry trusts XFF from **any** peer and therefore defeats the per-IP dimension; the node **warns loudly at startup** but does not refuse to start.
> - **Observability.** Watch `hydra_tenant_api_auth_failures_total{reason}`: many **distinct IPs accelerating in unison** is the signature of a misconfigured (non-stripping) trusted proxy, not a single-source guesser.

`503 not_ready` means this node has no configuration snapshot yet — retry.
`429 rate_limited` carries `Retry-After`; invalidations are capped per tenant per
minute (default 10) because each one fans out to every node and re-hits the
tenant's `auth_url` from all of them.

> **Known limitation (no fix promised):** when a tenant-API request body exceeds the 1 MiB cap, the node replies `413` and closes the connection **without draining the rest of the body** — a client still uploading a large body may observe a connection reset before it reads the `413` body.

### 5.2 How long can a revoked key keep working? (`HYDRA_AUTH_ALLOW_TTL_MAX_SECS`)

The honest answer, in order:

1. **Normal case**: as long as the invalidation takes to converge — the response's
   `fleet.state: applied` means every live node has applied it. If you get a
   `202`, the named `lagging` nodes are still serving the old verdict.
2. **A node that is alive and serving traffic but whose consumer is NOT
   advancing** (consumer task died, network partition to Redis, node not in the
   registry): nothing clears its L1, so the fallback is the entry's own TTL — and
   that TTL used to be whatever the TENANT asked for via `expires_in`.
   `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` (default **300**) now bounds it. Lower it to
   make revocation take effect faster on such a node; raise it to reduce
   `auth_url` traffic. It never caps a DENY (those are already short).
   *Lowering it does not retroactively shorten entries already written to Redis —
   use an invalidation for that.*
3. **Watch `hydra_invalidation_consumer_stalled_seconds`**: it measures how long
   a node's applied watermark has not advanced. **Alert above 60 s** — that is the
   signal that case 2 is happening, and before this metric existed the condition
   was completely invisible.

### 5.3 Usage accounting: two rates that legitimately disagree

- `GET /api/v1/stats/usage` (management) reads **in-process prometheus counters**:
  process health, **resets on restart**, no time dimension.
- `GET /tenant/{tid}/api/v1/usage` (tenant) reads the **metering store**
  (persistent rows, time window, `source: sqlite|clickhouse`): the
  reconciliation view.

They cannot match, and neither is "the bug":

| what is lost | which rate loses it |
| --- | --- |
| everything before a restart | the counters |
| requests that failed before provider selection, and the four drop paths | the rows |
| nothing (deliberate) | — |

**In a ClickHouse cluster `requests` is an APPROXIMATION**: an INSERT retried
after a read timeout re-sends the whole batch, and the CH table has no `trace_id`
column to de-duplicate on, so `COUNT(*)` can overstate. Tokens and `errors` are
unaffected in the same way (they are summed from the same duplicated rows). Use
it for trends and reconciliation, not for invoicing.

E3 **never** writes a usage row: querying usage does not bill as usage (a
tenant's own control-plane calls leave `ctx.selected` empty).

### 5.4 E3 troubleshooting

| symptom | cause | action |
| --- | --- | --- |
| `503 usage_store_unavailable` | the store is unreachable, OR it answered something undecodable, OR the result exceeded the response cap | check `hydra_tenant_api_usage_query_total{result}`: `store_unavailable` / `decode_error` / `result_too_large`. The tenant sees the same code all three ways, on purpose. For `result_too_large` the fix is to narrow `since`/`until` or reduce `group_by` cardinality (see the cap note below) — **not** a retry |
| `400 window_too_large` | window wider than `HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS` (default 31) | narrow the window; in ClickHouse the table's key leads with `created_at`, so a wide window scans other tenants' rows |
| `400 invalid_since` / `invalid_until` | the bound is not RFC3339 / epoch / space-separated, or `since > until` | the response echoes the NORMALISED window — compare against what you sent |
| latency climbs (`hydra_tenant_api_usage_query_seconds`) | wide windows | lower the window cap, and consider changing the CH table key to `ORDER BY (tenant_id, created_at)` (needs a rebuild + backfill; not done here) |

> **ClickHouse usage reads are response-size-capped (~64 KiB — `MAX_CLICKHOUSE_RESPONSE` in `crates/hydra-server/src/clickhouse.rs`).** A result that exceeds the cap (a very wide `group_by` over a long window) fails the read and surfaces as `503 usage_store_unavailable` with a "narrow the query" message — **not** a retryable error. Narrow `since`/`until` or reduce `group_by` cardinality (e.g. `day` instead of `model`).

### 5.5 Sub-tenant configuration (admin API)

Sub-tenants and their routes are operator-managed through the admin API; the
tenant-facing **read-only** mirrors are the two `GET /tenant/{tid}/api/v1/sub-tenants` /
`sub-tenant-routes` endpoints (documented in `tenant-api-integration.md` §5.4–§5.5).
The v1 operator write path adds **no new cluster mechanism**: it is the existing
admin-CRUD pattern, which inherits the automatic leader forwarding of
`maybe_forward_mutation`.

> **v2 (2026-09-18) adds a tenant self-service WRITE path** — the four data-plane
> write endpoints plus the four internal `/api/v1/internal/tenant-config/...` routes
> (cluster-token gated, leader-only). See **§5.5a** below and the tenant contract in
> `tenant-api-integration.md` §5.6. The v1 operator admin-CRUD above is unchanged.

**Resources** (flat, under `/api/v1/`, admin-token gated, design §13.2):

| resource | paths |
|---|---|
| `sub-tenants` | `GET/POST /api/v1/sub-tenants`, `GET/PUT/DELETE /api/v1/sub-tenants/{id}` |
| `sub-tenant-routes` | `GET/POST /api/v1/sub-tenant-routes`, `GET/PUT/DELETE /api/v1/sub-tenant-routes/{id}` |

- List supports `?tenant_id=` (sub-tenants) / `?sub_tenant_id=` (routes) filters.
- Create a sub-tenant **without** a `key_prefix` → the leader auto-generates one
  (8 chars `[A-Z0-9]` + a `_` separator, e.g. `QQCX_`); the generate→validate→insert
  loop retries up to 5 times on **both** a DB `UNIQUE` race and a validator overlap
  rejection (a generated 9-char prefix can be a superstring of an existing shorter
  one). An explicit empty prefix is `400`.
- On a successful mutation the snapshot is reloaded (`reload_best_effort`).

**Write validation is error-level, fail-closed** (pure `validate_sub_tenant_write`,
`crates/hydra-core/src/sub_tenant.rs`): a rejected write is never persisted. Duplicates
are `409`; every other rule violation is `400`:

| condition | code | HTTP |
|---|---|---|
| `name` already used by another sub-tenant in the tenant | `name_duplicate` | 409 |
| `key_prefix` already used by another sub-tenant in the tenant | `prefix_duplicate` | 409 |
| route's `provider_id` is not a known provider | `provider_not_found` | 400 |
| `provider_id` known but not in the tenant's `tenant_providers` | `provider_not_in_tenant` | 400 |
| `model_key` outside the tenant's `tenant_models` (only when the tenant has a mapping) | `model_not_in_tenant` | 400 |
| `provider_id` does not serve `model_key` | `model_not_served_by_provider` | 400 |
| `key_prefix` empty | `empty_key_prefix` | 400 |
| `key_prefix` non-ASCII | `invalid_key_prefix` | 400 |
| `key_prefix` has no separator (`_`/`-`) | `invalid_key_prefix` | 400 |
| `key_prefix` overlaps a same-tenant prefix or an enabled operator binding | `key_prefix_overlap` | 400 |
| tenant already at the sub-tenant quota | `quota_exceeded` | 400 |
| sub-tenant already at the route quota | `quota_exceeded` | 400 |

**Per-tenant quotas** (config-DoS guard, `crates/hydra-core/src/sub_tenant.rs`):
`MAX_SUB_TENANTS_PER_TENANT = 64`, `MAX_ROUTES_PER_SUB_TENANT = 32`.

**Snapshot-side backstop** (`config::validate`, Warn-only, *not* the sole line of
defence): warns on a route referencing an unknown sub-tenant/provider, a prefix
without a separator, and same-tenant prefix overlap.

**Default-route semantics (Q11, strict (a′))**: a **default route** (`model_key = NULL`)
pins the sub-tenant's traffic to **one** provider; any model that provider does not
serve becomes **unroutable for that prefix** (fail-closed `503 NoAvailableProvider`).
This mirrors the fail-closed operator binding and is intentional — do not expect a
model to "fall back" to normal routing when the default route's provider lacks it.

**Operator-binding overlap is one-sided** (noted in the post-implementation
review): a sub-tenant write rejects a prefix that overlaps an enabled operator
binding, but an operator binding write does **not** check sub-tenant prefixes.
An operator can therefore later add a binding that supersedes a sub-tenant
prefix; by (a′) the operator wins and the sub-tenant route silently never fires
for those keys (deterministic, and the catalog stays honest). Add such a binding
with intent.

**Observability**: v1 adds **no new Prometheus metrics** for sub-tenant routing (per
plan; catalog consistency is pinned by core tests, not a metric).

**Availability**: admin CRUD is only available on nodes that serve the admin API
(leader / standby). **Edge nodes 404 every admin path pre-auth** (they serve only
`/metrics` `/healthz` `/readyz`), so sub-tenant CRUD from an edge is unavailable —
the same boundary as every existing admin resource.

### 5.5a Tenant self-service write path (v2, A′)

In v2 a tenant can CRUD its **own** sub-tenants and routes on the **data plane**,
without the operator. The two faces and the single write point:

**Four data-plane endpoints** (tenant-token gated, on the data-plane port; the tenant
sees a normal response):

| endpoint | what it does |
| --- | --- |
| `PUT    /tenant/{tid}/api/v1/sub-tenants/{name}` | upsert by `(tenant_id, name)`; body `{key_prefix?, enabled}` |
| `DELETE /tenant/{tid}/api/v1/sub-tenants/{id}` | delete by immutable id (idempotent) |
| `PUT    /tenant/{tid}/api/v1/sub-tenant-routes` | upsert by `(sub_tenant_id, model_key)`; body `{sub_tenant_id, model_key?, provider_id, enabled}` |
| `DELETE /tenant/{tid}/api/v1/sub-tenant-routes/{id}` | delete by immutable id (idempotent) |

**Four internal routes** (the leader's write point — **NOT exposed on the data-plane
port**; cluster-token gated, **leader-only**):

| route | gate |
| --- | --- |
| `PUT /api/v1/internal/tenant-config/sub-tenants` | `HYDRA_CLUSTER_TOKEN` + receiver-side lease assertion |
| `DELETE /api/v1/internal/tenant-config/sub-tenants/{id}` | same |
| `PUT /api/v1/internal/tenant-config/sub-tenant-routes` | same |
| `DELETE /api/v1/internal/tenant-config/sub-tenant-routes/{id}` | same |

The data-plane edge authenticates with the **tenant token** (same gate as the read
endpoints) and then either **forwards to the lease-holding leader** (cluster) or
**executes locally** (single-node). The leader:

1. **lease assertion** — non-candidate → `404`, candidate without the lease → `503
   not_leader` (a standby **never** writes locally);
2. **re-auth** — the tenant Bearer travels in the dedicated `x-hydra-tenant-token`
   header (**never** `Authorization`, **never** the body) and is re-authenticated
   against the same `ConfigStore`;
3. **authorization binding** — the write target must be the authenticated tenant's own
   resources (`body.tenant_id != T` → `403` by pure string comparison before any
   lookup; a route whose `sub_tenant_id` is missing / foreign → `404`, indistinguishable);
4. **write** — the shared transactional write core (`admin/sub_tenant_write.rs`);
5. **audit** — one structured record per write (below).

**Single-node executes locally**: with no forwarder (a single-node / `all` build), the
data-plane node **is** the writer and applies the write locally through the same
write core, then `reload_all` so `config_version` advances. The two faces share one
write point (`apply_config_write`), so the admin / internal / local paths cannot diverge
on validation, quota, or natural-key upsert semantics.

**Cluster topology constraint (known limitation)**: a cluster node with a forwarder
**always forwards**; a cluster **leader's own data plane** therefore has no forward
target (the self-forward guard resolves `None`) and returns `503 no_leader` for tenant
writes. The leader still owns the authoritative DB — it simply does not serve tenant
writes on its data plane. The sample topology (LB → edge:8080) does not route tenant
traffic to the leader, so this is latent today; a deployment that exposes the leader
data plane would need the lease holder to take the local path (the single writer is the
lease holder, so this preserves the invariant). Tracked as a refinement, not a
correctness bug: it fails closed.

**Per-tenant write rate limit (D6)**: the leader applies a fixed-window,
**process-local** `Throttle` keyed on the authenticated tenant id
(`AdminState.config_write_throttle` / `config_write_per_min`, `admin/mod.rs`). Budget is
`HYDRA_TENANT_CONFIG_WRITE_PER_MIN` (**default 60**/min); beyond it the write is `429
too_many_requests` (the retry seconds are in the message body; this endpoint sets **no**
`Retry-After` header, unlike the data-plane invalidate 429). Because all writes land on the leader, one
in-process window covers the cluster. **Anti-DoS only — the window is in-process and
therefore RESETS when leadership moves**: a leader failover allows a bounded burst of
config writes. This is the same accepted class as the E2 allow-TTL bound
(`HYDRA_AUTH_ALLOW_TTL_MAX_SECS`) — it is not a durability guarantee.

**Audit log (D7)**: each successful config write emits one `tracing` record
(`target = "hydra::tenant_config_write"`) carrying `tenant_id`, `trace_id`, `action`
(`upsert`/`delete`), `resource` (`sub_tenant`/`sub_tenant_route`), `resource_id`, and
`config_version`. The tenant **bearer** and the **request body are NEVER logged**.

**D5 tightening (quota)**: the write core counts the per-tenant quota against **ALL DB
rows, including disabled ones** (not just the enabled-only snapshot). This fixes v1
review finding 1 — "disable → re-create" can no longer bypass
`MAX_SUB_TENANTS_PER_TENANT` (64) / `MAX_ROUTES_PER_SUB_TENANT` (32). It also re-validates
prefix overlap inside the write transaction (finding 2, closing the
validate-then-insert TOCTOU).

**Disable/delete ≠ revoke**: deleting a sub-tenant only stops steering; its keys keep
flowing through normal routing and are revoked only by the tenant's `auth_url` (see
`tenant-api-integration.md` §7.6).

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
> 不再有 `retry_after_connect` 配置、不再有 `upstream_bytes_seen` / `body_too_large` 守卫、不再有 Pingora 的 `set_retry` / `fail_to_connect` / `error_while_proxy` 钩子。详见 `dev-docs/design-change-terminate-mode.md` §4.3。

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

> ~~H2 paths are truly zero-copy on the forward leg; H1 paths incur one kernel copy per chunk (Pingora core limitation, design §8.5).~~ **（已废弃）** Terminate-mode 放弃 kernel-level 零拷贝（body 经 userspace buffer 传给 reqwest），但保留"零 JSON 往返"（body 字节未被 serde 处理）。详见 `dev-docs/design-change-terminate-mode.md` §5。

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
- **Admin UI**: `http://<admin_addr>/admin/` — same-origin (the token is kept
  in `sessionStorage` for the current tab, so a reload stays signed in while
  closing the tab signs out). Useful for incident inspection (breaker dead-set,
  health, manual reload, key reveal with audit log).

### 9.1 Alerting: which metric means what

**This repository ships the METRICS, not the alert rules.** The alert-rule files
(Prometheus rules / Alertmanager routes) belong to the OPERATIONS repository —
adding them here would create a second owner for the same policy. The table below
is the contract those rules are written against: every expression uses a metric
name and label that really exists in this codebase (`/metrics`).

| Alert | Expression | Meaning |
|---|---|---|
| Certs configured, no TLS listener bound | `hydra_listener_tenant_certs > 0 and hydra_listener_bound{protocol="tls"} == 0` | Tenant SNI is silently not served (the 2026-09-16 outage shape) |
| TLS listener configured, no certs | `hydra_listener_bound{protocol="tls"} == 1 and hydra_listener_tenant_certs == 0` | Handshakes will fail until a certificate is written |
| Invalid listener configuration | `increase(hydra_listener_misconfig_total[10m]) > 0` | Startup-time configuration problem (certs without a port / port without certs) |
| Registry rows piling up | `hydra_registry_nodes{state="dead"} > 5` | Reaping is failing, or node identities drift |
| Registry reaping churn | `increase(hydra_registry_reaped_total[1h]) > 20` | Nodes keep being recreated (unstable identity — see §13.6) |
| Config snapshot stale | `hydra_config_snapshot_stale == 1` | A post-write reload failed; the in-memory snapshot is behind the DB |
| Upstream first-byte timeouts | `increase(hydra_upstream_first_byte_timeout_total[10m]) > 0` | The upstream accepted the connection and then sent no response headers; each attempt fails within `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS` (default 30s) instead of burning the 300s exchange timeout. **Do not use `hydra_retries_total{stage="connect"}`** — nothing emits that label value (retries are recorded with `stage="terminate_loop"`), so such a rule could never fire |
| Replication stalled (upgrade window) | `changes(hydra_control_snapshot_version[10m]) == 0 and hydra_control_poll_total{result="ok"} > 0` | Fail-closed mixed-version signal. **Only polling nodes publish these** — scope the rule by role |

The label is `protocol`, never `transport` (`hydra_listener_bound` is registered
with `&["protocol"]`). Note the metric-name family: listener signals live under
`hydra_listener_*`; there is deliberately **no** `hydra_proxy_listener_*` alias —
two names for one signal would mean two owners.

> **Known blind spot in the breaker PROBE (audit §7-1, deliberately NOT fixed).**
> A revived-by-probe decision treats any response status `< 500` as "healthy", and
> the probe hits a path that a real inference request never uses — so an upstream
> that accepts connections and returns e.g. 404 to the probe (or that is merely
> slow to answer) can be revived while genuine traffic still fails. Whether 4xx
> should count as healthy, and whether the probe should exercise the real
> inference path with a real credential, is a PRODUCT decision that needs real
> credentials to validate; it is recorded here rather than silently "fixed" by
> adding switches whose defaults preserve the blind spot.
>
> What IS fixed: the per-attempt **first-byte bound** (`HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS`,
> default 30s). Without it a "connected but silent" upstream consumed the full
> 300s exchange timeout on every attempt and every failover hop.

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

### 10.7 Data-plane requests return 401 `missing_api_key`

The gateway accepts a client api-key from any of these transports (first
match in this order wins):

| # | Transport | Typical client |
|---|-----------|----------------|
| 1 | `Authorization: Bearer <k>` | OpenAI SDK, Anthropic SDK (OAuth) |
| 2 | `Authorization: <k>` (bare, no scheme) | self-rolled clients |
| 3 | `x-api-key` | Anthropic SDK |
| 4 | `api-key` | Azure OpenAI |
| 5 | `x-goog-api-key` | Gemini CLI / google-genai |
| 6 | query `?key=` / `?api_key=` / `?apikey=` / `?access_token=` | browser WebSocket |

Check, in order:

1. **Something is stripping the header.** Reverse proxies, CDNs and API
   gateways routinely drop unknown `x-goog-api-key`/`api-key` headers, or
   strip the query string entirely — verify with the gateway's own access log
   or a direct call to the Hydra port.
2. **A non-Bearer `Authorization` scheme is not a key.** `Basic …`/`Digest …`
   are rejected on purpose; such a request falls through to the other
   transports and, if none carries a key, ends in 401.
3. **The query form depends on nothing in front of Hydra logging URLs.**
   Hydra itself never logs request URIs and never forwards the query string
   upstream, but a fronting proxy might log it — keep the credential out of
   the query whenever a header is possible.
4. **Multiple transports with different values** resolve to the first match
   above and emit one `warn` (`source` + `conflicting` labels, never values)
   — grep for *"differing api-key values"* when a call authenticates as an
   unexpected key.

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

## 13. Cluster mode operations (design §20 / dev-docs/cluster.md)

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
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"         # required, >= 16 chars
export HYDRA_CLUSTER_TOKEN="$(openssl rand -hex 32)"       # required (control channel)
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # SAME on every node
docker compose -f docker-compose.cluster.yml up -d --scale hydra-edge=2
curl -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://localhost:8081/api/v1/tenants
```

k3s / k8s manifests and bare-metal systemd live in `dev-docs/cluster.md` §4.

### 13.3 Cluster environment variables (quick map)

| Variable | Notes |
|---|---|
| `HYDRA_ROLE` | `leader` / `edge`; unset = single-node (unchanged behavior) |
| `HYDRA_REDIS_URL` / `HYDRA_REDIS_MODE` | backbone; `single` wired, sentinel/cluster fail-fast |
| `HYDRA_CLUSTER_TOKEN` | shared control-channel token (all nodes) |
| `HYDRA_CONTROL_URL` / `HYDRA_PUBLIC_URL` | active control endpoint (snapshot polling) / this node's registered URL. `HYDRA_CONTROL_URL` is **not** the admin-mutation forward target — a standby forwards writes to the ACTUAL lease holder, resolved live from the registry (self-forward/mutual-forward loop guards; see `dev-docs/cluster.md` §5.2) |
| `HYDRA_ADMIN_TOKEN` | required on leaders, shared cluster-wide |
| `HYDRA_ENCRYPTION_KEY` | master key, identical fleet-wide |
| `HYDRA_USAGE_SINK=clickhouse` | mandatory in cluster mode (+ `HYDRA_CLICKHOUSE_URL`) |
| `HYDRA_LEADER_LEASE_MS` / `HYDRA_CONTROL_POLL_MS` | 15000 / 1000 defaults |
| `HYDRA_NODE_ID` | this node's registry + lease identity; defaults to `HOSTNAME`, then random (see §13.6) |
| `HYDRA_FORWARD_TIMEOUT_SECS` | standby→leader admin-forward timeout (default 5). It bounds the CONNECT phase; the total deadline is that value + 2s so a connect-phase failure is reported as the definite failure it is (see `forward.rs`) |
| `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS` | upstream time-to-first-byte bound per attempt (default 30); `0` is rejected. See the alert row in §9.1 |
| `HYDRA_SHUTDOWN_DRAIN_SECS` | seconds Pingora drains in-flight requests after SIGTERM (default 20); size `terminationGracePeriodSeconds` from it (see §13.5b) |
| `HYDRA_REGISTRY_STALE_GRACE_SECS` | TTL of the registry "last seen" witness (default 120). Only `> 0` values are accepted; a small value narrows the grace window in which a merely-silent node is protected from reaping |

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
`HYDRA_CONTROL_URL` points at itself. Full checklist: `dev-docs/cluster.md` §5.

### 13.5 Redis outage behavior

Data plane keeps serving (last-known-good snapshot + local caches). Election is
**fail-closed**: a leader that cannot renew demotes immediately (writes stop)
until Redis recovers. See `dev-docs/cluster.md` §3 for the full matrix.

### 13.5b Shutdown drain vs `terminationGracePeriodSeconds`

`HYDRA_SHUTDOWN_DRAIN_SECS` (default **20**) is how long Pingora may spend
draining in-flight requests after `SIGTERM`. It maps to Pingora's
`grace_period_seconds`. Set your pod's grace period from it:

```
terminationGracePeriodSeconds  >=  HYDRA_SHUTDOWN_DRAIN_SECS (20)
                                 +  graceful_shutdown_timeout_seconds (5, the
                                    final runtime-shutdown step — not the
                                    in-flight window)
                                 +  slack (10)
                                 =  35  (default)
```

**Why this must be set explicitly:** Pingora's DEFAULT `grace_period_seconds` is
`None` ⇒ 300s, far beyond a typical 30s Kubernetes grace period. The pod would be
`SIGKILL`ed while still draining, losing both the usage-sink flush and the
registry de-registration. The deployment manifests are owned by the operations
repository; this repository only reads the environment variable.

### 13.6 Registry identity: `HYDRA_NODE_ID`, `HOSTNAME`, and why they matter

Node identity is resolved as **`HYDRA_NODE_ID` → `HOSTNAME` → random**, and it is
used for TWO things: the registry row (`hydra:{nodes}`) **and the leader lease**
(the lease value is the bare node id). Three consequences, all operator-visible:

1. **Pods need STABLE names.** The `HOSTNAME` fallback only helps under a
   StatefulSet (or a Deployment with a pinned name). Under a plain Deployment
   `HOSTNAME` changes on every restart, so each restart registers a new row and
   the fallback buys nothing — set `HYDRA_NODE_ID` explicitly instead.
2. **Two nodes must never share one `HOSTNAME`.** They would share a registry
   row, and — worse — BOTH would satisfy the lease-renew check (`GET
   hydra:lease == <our node_id>`), i.e. **two nodes would each believe they hold
   the lease** (split brain). The shutdown `unregister()` of either process also
   deletes the shared row, including the peer's registration.
3. **Reaping is two-strike, deliberately.** `hydra_registry_reaped_total` /
   `hydra_registry_nodes{state}` show the reaper at work. A registry row is
   deleted only when its 30s heartbeat AND its grace witness
   (`hydra:{node:seen}:<id>`, TTL = `HYDRA_REGISTRY_STALE_GRACE_SECS`, default
   120) are both absent **on two consecutive sweeps** (the reaper ticks every
   60s). The first observation only records a strike. That extra tick exists
   because a node running a binary that predates the witness key writes its row
   exactly ONCE, at boot: deleting such a row while the process is alive would
   make it invisible to the fleet forever — and if it held the lease, every
   standby admin write would answer 503 permanently (forwarding is fail-closed
   with no static fallback). Any sign of life (a heartbeat, or a refreshed
   witness) clears the strike. The current lease holder is never reaped at all.

### 13.7 Known limitations (as of this revision)

- ~~Disabled `limit_role` / `provider_key_binding` rows are not carried in
  config snapshots — after a failover they are lost from replicas.~~ **FIXED**:
  the snapshot contract carries the full fidelity rows (including disabled ones,
  `provider_key` identity and tenant access-token hashes), so a promoted replica
  is byte-faithful. See `dev-docs/cluster.md` and the snapshot wire v2 notes.
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
