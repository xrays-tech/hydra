# Hydra

> **Tired of Python LLM gateways that leak gigabytes of RAM at idle and silently mangle your tool calls through lossy OpenAI↔Anthropic translation?**
>
> **Hydra is a Rust + Pingora LLM gateway that speaks both OpenAI and Anthropic *natively* — zero protocol conversion, per-tenant TLS, and billing-grade usage metering (cached tokens + TTFT) — running at ~65 MiB RSS with zero `unsafe` and zero production `unwrap`/`panic!`; the only panic sources are 2 `expect()` calls asserting unreachable invariants (both registered, decision D-13).**

**A high-performance LLM routing gateway.** Route **OpenAI (`/v1/chat/completions`) and Anthropic (`/v1/messages`)** client traffic to upstream model providers — format-homogeneous pass-through (the client's path is preserved end-to-end, including usage parsing), with per-tenant auth, weighted load balancing, failover, circuit breaking, rate limiting, granular usage metering (input/cached/output tokens + TTFT), and per-tenant TLS. Built in Rust on [Pingora](https://github.com/cloudflare/pingora).

[中文文档](README.zh-CN.md) · [🌐 项目主页](https://xrays-tech.github.io/hydra/)

## Highlights

> Measured on a 10-core machine against a threaded mock upstream (no real paid provider hit). Full methodology + 8C16G VPS capacity extrapolation in the [evaluation report](dev-docs/evaluation-report.html).

| | metric | note |
|---|---|---|
| ⚡ | **11,056 RPS** peak throughput | c=25, p99 = 4.39 ms |
| 🪶 | **65 MiB** RSS under full load | 18.6 → 65.4 MiB; < 0.4% of a 16 GB box |
| ⏱️ | **~0.3 ms** per-request gateway overhead | negligible vs. LLM latency; the model-extraction pass is O(body) (~0.09-1.3 ms/MB), so multi-MB bodies add milliseconds |
| 🛡️ | **0** production `unwrap`/`panic!`, **2** `expect()` invariant assertions | both crates `#![forbid(unsafe_code)]`; the 2 panic sources are unreachable-invariant assertions — `main.rs` (cert store), `proxy/provider_client.rs`. **Count = what `check_source_purity.cjs` measures; the number is a testable claim, not prose.** |
| 🔐 | **AES-256-GCM** provider keys at rest | fail-closed boot; admin API never returns plaintext |
| 🧪 | **core + server** tests (see [`docs/index.html`](docs/index.html) for the exact measured count), `clippy -D warnings` clean | CI hard gate |

**Production-readiness: 9.2 / 10** — see the [full report](dev-docs/evaluation-report.html).

---

## What it is

Hydra sits between your agents/clients and your LLM providers. A client request is resolved to a tenant by domain, authenticated **internally** against the tenant's own stored per-user API keys (no external auth call), the **full request body is read** so `model` can be extracted from any position/schema, then Hydra routes (model × tenant-allowed providers ∩ any matching Global Role, weighted round-robin), swaps the client key for a provider key, calls the real provider via its own HTTP client (reqwest), streams the response back, parses usage tokens (including cached tokens), and records it all.

```
Agent ──► Pingora ──► [tenant resolve → internal API-key auth → read full body → extract model
                        → route → swap key → reqwest call to provider → stream SSE back
                        → parse usage (input/cached/output + TTFT) → record]
```

If a provider fails, Hydra **failovers** to the next candidate automatically (trivial — the full body is already buffered, replay is O(1) refcount).

## Features

- **Terminate-mode proxy**: reads the full request body in `request_filter` (model extraction works for ANY position/schema — no first-chunk peeking); calls the provider via a dedicated reqwest client; streams the SSE response back through Pingora's session writer. Returns `Ok(true)` so Pingora never dials upstream.
- **Two client protocols, format-homogeneous**: accept OpenAI (`POST /v1/chat/completions`) **and** Anthropic (`POST /v1/messages`). The path you call selects the format end-to-end — the upstream URL, request body, and usage parser all match (no OpenAI↔Anthropic conversion). `UsageScanner` picks `ProviderKind::Anthropic` for `/v1/messages` (parses `input_tokens`/`output_tokens`/`cache_read_input_tokens`), `Generic` otherwise.
- **Routing**: model name → providers ∩ tenant-allowed providers; smooth weighted round-robin (Nginx SWRR).
- **Internal API-key auth + four-level user hierarchy**: a client presents its own api-key (`hk_` + CSPRNG, stored only as SHA-256; plaintext shown once at creation), resolved to a user under `tenant → user_group → user → api_key`. Unknown/disabled/expired key → **401**, disabled user/group → **403**, a user flagged delinquent via the tenant API → **402 `user_delinquent`**. No external `auth_url`, no auth cache. Tenant access tokens (`ht_`, multi-token per tenant) gate the tenant self-service API only. The old external-auth path (`auth_url` / `HttpAuthChecker` / `AuthCache`) and the sub-tenant / key-prefix routing layer were REMOVED by the T10 cleanup (the greenfield schema has no such tables); calls to those old paths are plain 404s.
- **Global Role routing**: a `global_role` table keyed on `(tenant, user_group, user, api_key, model)` — each dimension `*` = any — maps to a provider set. Matched rules' providers are **unioned** (dedup, `provider_id` ascending) and always intersected with providers actually serving that model; wildcard-tenant roles still intersect `tenant_providers` (fail-closed). Weighted SWRR by default, or session-sticky weighted rendezvous (`HYDRA_SESSION_AFFINITY`, default on). No match → today's `tenant_providers ∩ models_by_key` behavior.
- **Failover + circuit breaker**: the failover loop tries each candidate provider in sequence; consecutive failures trip a dead-set with background probing. Full body replay is O(1) `Bytes::clone()`.
- **Rate limiting**: in-memory sliding window (request count + token), per role, m/h/d windows.
- **Usage recording**: pluggable sink (**ClickHouse** — the shared store — or `none` to switch metering off; the retired per-node SQLite sink was removed 2026-10-07); **granular token breakdown**: `prompt_tokens` / `completion_tokens` / `total_tokens` / `cached_tokens` (OpenAI `prompt_tokens_details` + Anthropic `cache_read_input_tokens`); **latency metrics**: `forward_latency_ms` (Hydra overhead before provider call) + `ttft_ms` (time to first token). All numeric fields default to 0 (no NULLs).
- **Per-tenant TLS**: SNI-based certificate selection with hot-reload (BoringSSL/OpenSSL).
- **Admin REST + UI**: full CRUD for all config entities, Prometheus `/metrics`, embedded dashboard.

## Deploy

### Docker (recommended)

```bash
# both keys are REQUIRED — compose fails fast if not exported
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"

# 1. cross-compile the linux/amd64 binary + build the image
#    DB_BACKEND=postgres (default) or DB_BACKEND=mysql selects the compiled database backend
./environment/build.sh

# 2. run the full stack (hydra + postgres + mock-tenant + clickhouse)
cd environment && docker compose up -d

# 3. register your providers (reads secure/config.json)
python3 ../environment/init.py
```

### From source

```bash
# exactly one database backend feature is required: db-postgres | db-mysql
cargo build --release --features server,db-postgres
# HYDRA_DB_URL must match the compiled backend; HYDRA_ENCRYPTION_KEY and HYDRA_USAGE_SINK are required
HYDRA_DB_URL=postgres://hydra:hydra@127.0.0.1:5432/hydra \
HYDRA_ADMIN_TOKEN=<token> \
HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)" \
HYDRA_USAGE_SINK=none \
./target/release/hydra
```

## Configure

Hydra boots from **environment variables** (runtime) and stores all routing config in a **shared PostgreSQL or MySQL database** (managed via the admin API). The backend is selected at compile time: `DB_BACKEND=postgres|mysql` maps to the `db-postgres` / `db-mysql` Cargo feature. SQLite is gone. Migrations are greenfield baselines under `migrations/postgres/` and `migrations/mysql/`, applied at boot.

| Env var              | Default                          | Purpose                                              |
| -------------------- | -------------------------------- | ---------------------------------------------------- |
| `HYDRA_DB_URL`       | *(compiled-backend dev fallback)* | **Required in practice.** PostgreSQL/MySQL URL; its scheme must match the compiled backend (`db-postgres` ⇒ `postgres://…`, `db-mysql` ⇒ `mysql://…`). |
| `HYDRA_LISTEN`       | `0.0.0.0:8080`                   | Proxy **plaintext** listen address (always bound)    |
| `HYDRA_TLS_LISTEN`   | *(unset)*                        | Optional proxy **TLS** listen address, e.g. `0.0.0.0:8443`. Setting it (not the tenant certs) is what creates the HTTPS listener; per-tenant certs are selected by SNI. |
| `HYDRA_ADMIN_ADDR`   | `127.0.0.1:8081`                 | Admin REST + UI + `/metrics` listen address          |
| `HYDRA_ADMIN_TOKEN`  | —                                | Bearer token gating `/api/v1/*` (**required for admin**) |
| `HYDRA_ENCRYPTION_KEY` | —                              | Base64 of 32 bytes; encrypts provider api-keys at rest (**required**, fail-closed). Generate: `openssl rand 32 \| base64`. |
| `HYDRA_USAGE_SINK`   | — (**required**)                 | `clickhouse` (shared store) or `none` (no metering)   |
| `HYDRA_CLICKHOUSE_URL` | —                              | ClickHouse HTTP endpoint (when sink=clickhouse)      |
| `HYDRA_CONFIG_POLL_MS` | `5000`                         | Cluster-only config revision-probe interval; unset/`0`/garbage ⇒ 5000 |
| `RUST_LOG` | *unset* | Log level; in-process default when unset is tracing's `error` — `info` is the recommended/deployed value |

> The only switch between single-node and cluster is **`HYDRA_REDIS_URL`**: unset = single node with no Redis; set = cluster data-plane state and config fanout. There is no raft, no leader, and no member list.

| Env var | Default | Purpose |
|---|---|---|
| `HYDRA_REDIS_URL` | — | Set ⇒ cluster mode. Shared rate limits / breaker votes / invalidation, plus the `hydra:config:changed` fanout channel. |
| `HYDRA_NODE_ID` | hostname, else random | Stable node identity for logs, fanout origin and breaker votes |
| `HYDRA_CONFIG_POLL_MS` | `5000` | Revision-probe fallback for a missed pub/sub message or Redis outage |
| `HYDRA_REDIS_MODE` | `single` | Read when set; only `single` is accepted |

Three retired raft-era variables (`HYDRA_CLUSTER_PEERS`, `HYDRA_CLUSTER_ID`, `HYDRA_ARACHNE_LISTEN`) and the other Arachne names are **ignored with a startup ERROR** if still set; remove them from manifests.

**Ports**: `8080` proxy (plaintext, always bound) · `8443` proxy (TLS, only if `HYDRA_TLS_LISTEN` is set) · `8081` admin (REST + UI + metrics).

## Use

### Admin UI

Open `http://<host>:8081/admin/` and enter the admin token. Manage providers, models, keys, tenants, access, rate-limit roles, Global Roles, and view circuit-breaker status.

### Admin REST

```bash
TOKEN=<your-admin-token>

# create a provider
curl -X POST http://localhost:8081/api/v1/providers \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"openai","key":"openai","name":"OpenAI","endpoint":"https://api.openai.com","weight":1}'

# create a tenant + grant provider + model access
curl -X POST http://localhost:8081/api/v1/tenants \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"acme","name":"ACME","domain":"acme.example.com","enabled":true}'

# list / reload / metrics
curl -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/providers
curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/reload
curl -H "Authorization: Bearer $TOKEN" http://localhost:8081/metrics
```

### Point a client at Hydra

Any OpenAI-compatible client: set the base URL to the proxy and send the tenant's client api-key.

```bash
curl https://acme.example.com/v1/chat/completions \
  -H "Authorization: Bearer <client-api-key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

Hydra resolves tenant `acme` by domain → authorizes the key against the tenant's stored per-user API keys (internal: 401 on unknown/disabled key, 403 on disabled user/group, 402 on a delinquent user) → routes `gpt-4o` to an allowed provider (optionally narrowed by a matching Global Role) → swaps in a provider key → streams the response back → records usage (tokens + cached + TTFT).

**Model catalog**: `GET /v1/models` is publicly readable (with or without an api-key — the presented key does not trigger auth; an api-key that resolves to a principal narrows the catalog through that principal's Global Role rules, a subset of the anonymous union) and returns the catalog of models this tenant can currently call (Hydra aggregates locally: union across allowed providers, filtered by the tenant model whitelist, excluding broken/disabled/keyless providers); chat calls still require an api-key. Admin has a read-only aggregate at `GET /api/v1/tenants/{tenant_id}/models`. See `dev-docs/design-tenant-model-catalog.md` and `dev-docs/aegis/plans/2026-09-08-public-models-catalog.md`.

## Project layout

```
crates/hydra-core/    pure domain logic (router, SWRR, breaker, SSE scan, limits) — zero I/O deps
crates/hydra-server/  Pingora proxy shell (terminate-mode), DB, auth, usage sink, TLS, admin
environment/          Dockerfile + docker-compose + mock-tenant + init script
integration/          Python CRUD test suite + e2e proxy test + mock LLM/auth
dev-docs/                 design.md, ops.md, dev-plan.md, architecture analysis
```

## Cluster Mode

Single node needs no Redis. **The only cluster switch is `HYDRA_REDIS_URL`**: set it and this
process joins the shared data-plane state and the config-change fanout; leave it unset and it is
a single-node deployment.

The cluster is **three identical nodes sharing one relational database**. There is no raft, no
leader and no static member list. Every node proxies, serves the admin API and writes the shared
database directly. Write serialization is a `hydra_lock` row taken with `SELECT … FOR UPDATE`;
every write bumps `config_revision.version` in the same transaction. Each process serves an
in-process `ArcSwap<ConfigData>` cache. After a committed local write and reload, the node
publishes `{version, origin}` on Redis `hydra:config:changed`; peers reload quietly. A
`HYDRA_CONFIG_POLL_MS` (default 5000) revision probe is the missed-message/outage backstop.
External reloads never republish. `/healthz/leader` is gone (404); cluster status is local-node
only (`node_id`, `role`, `config_version`, `db_reachable`, `redis_configured`,
`redis_reachable`).

Cluster build / run:

```bash
# one database backend feature is required; the image also bundles Redis + ClickHouse
cargo build --release --features server,cluster-redis,usage-clickhouse,db-postgres
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"          # every node uses it
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # identical across the cluster
cd environment && docker compose -f docker-compose.cluster.yml up -d   # 3 identical nodes, one shared Postgres
```

Per node: the same `HYDRA_DB_URL`, `HYDRA_REDIS_URL`, `HYDRA_ADMIN_TOKEN`, `HYDRA_ENCRYPTION_KEY`
and `HYDRA_USAGE_SINK`; a distinct `HYDRA_NODE_ID` is recommended but defaults to the hostname.
There are no per-node config volumes and no raft ports.

A killed node only removes capacity, not write availability: any survivor keeps writing the shared
database. Its cache converges from the Redis notification, or from the poll fallback if the
notification was missed. The availability boundary is the database (and Redis for shared
data-plane state), not a quorum.

See **[`dev-docs/cluster.md`](dev-docs/cluster.md)** (write/cache protocol, failure modes, node-kill
drill) and [`environment/docker-compose.cluster.yml`](environment/docker-compose.cluster.yml).

## More

- Design & architecture: [`dev-docs/design.md`](dev-docs/design.md)
- Architecture change (terminate-mode): [`dev-docs/design-change-terminate-mode.md`](dev-docs/design-change-terminate-mode.md)
- Operations runbook: [`dev-docs/ops.md`](dev-docs/ops.md)
- Deployment guide (single node / compose / K3s / K8s): [`dev-docs/deployment.md`](dev-docs/deployment.md)
- Interactive workflow diagram: [`dev-docs/workflow.html`](dev-docs/workflow.html)

Rust 1.83+ · Pingora 0.8.x
