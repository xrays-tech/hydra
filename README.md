# Hydra

> **Tired of Python LLM gateways that leak gigabytes of RAM at idle and silently mangle your tool calls through lossy OpenAI↔Anthropic translation?**
>
> **Hydra is a Rust + Pingora LLM gateway that speaks both OpenAI and Anthropic *natively* — zero protocol conversion, per-tenant TLS, and billing-grade usage metering (cached tokens + TTFT) — in a 65 MiB binary with zero `unsafe`, `unwrap`, or `panic`.**

**A high-performance LLM routing gateway.** Route **OpenAI (`/v1/chat/completions`) and Anthropic (`/v1/messages`)** client traffic to upstream model providers — format-homogeneous pass-through (the client's path is preserved end-to-end, including usage parsing), with per-tenant auth, weighted load balancing, failover, circuit breaking, rate limiting, granular usage metering (input/cached/output tokens + TTFT), and per-tenant TLS. Built in Rust on [Pingora](https://github.com/cloudflare/pingora).

[中文文档](README.zh-CN.md)

## Highlights

> Measured on a 10-core machine against a threaded mock upstream (no real paid provider hit). Full methodology + 8C16G VPS capacity extrapolation in the [evaluation report](docs/evaluation-report.html).

| | metric | note |
|---|---|---|
| ⚡ | **11,056 RPS** peak throughput | c=25, p99 = 4.39 ms |
| 🪶 | **65 MiB** RSS under full load | 18.6 → 65.4 MiB; < 0.4% of a 16 GB box |
| ⏱️ | **~0.3 ms** per-request gateway overhead | negligible vs. LLM latency |
| 🛡️ | **0** production `unwrap`/`panic`/`unsafe` | both crates `#![forbid(unsafe_code)]` |
| 🔐 | **AES-256-GCM** provider keys at rest | fail-closed boot; admin API never returns plaintext |
| 🧪 | **core 114 + server 173** tests, `clippy -D warnings` clean | CI hard gate |

**Production-readiness: 9.2 / 10** — see the [full report](docs/evaluation-report.html).

---

## What it is

Hydra sits between your agents/clients and your LLM providers. A client request is resolved to a tenant by domain, authenticated against the tenant's own auth endpoint, the **full request body is read** so `model` can be extracted from any position/schema, then Hydra routes (model × tenant-allowed providers, weighted round-robin), swaps the client key for a provider key, calls the real provider via its own HTTP client (reqwest), streams the response back, parses usage tokens (including cached tokens), and records it all.

```
Agent ──► Pingora ──► [tenant resolve → external auth → read full body → extract model
                        → route → swap key → reqwest call to provider → stream SSE back
                        → parse usage (input/cached/output + TTFT) → record]
```

If a provider fails, Hydra **failovers** to the next candidate automatically (trivial — the full body is already buffered, replay is O(1) refcount).

## Features

- **Terminate-mode proxy**: reads the full request body in `request_filter` (model extraction works for ANY position/schema — no first-chunk peeking); calls the provider via a dedicated reqwest client; streams the SSE response back through Pingora's session writer. Returns `Ok(true)` so Pingora never dials upstream.
- **Two client protocols, format-homogeneous**: accept OpenAI (`POST /v1/chat/completions`) **and** Anthropic (`POST /v1/messages`). The path you call selects the format end-to-end — the upstream URL, request body, and usage parser all match (no OpenAI↔Anthropic conversion). `UsageScanner` picks `ProviderKind::Anthropic` for `/v1/messages` (parses `input_tokens`/`output_tokens`/`cache_read_input_tokens`), `Generic` otherwise.
- **Routing**: model name → providers ∩ tenant-allowed providers; smooth weighted round-robin (Nginx SWRR).
- **Key-prefix binding gate**: pin client api-keys by raw prefix to one provider (`sk_aaa_*` → Provider A); longest prefix wins, fail-closed (bound provider unavailable ⇒ 503, never falls back).
- **External auth**: each tenant points to its own `auth_url`; Hydra caches verdicts 5 min and exposes an invalidation endpoint (the tenant decides欠费/封禁).
- **Failover + circuit breaker**: the failover loop tries each candidate provider in sequence; consecutive failures trip a dead-set with background probing. Full body replay is O(1) `Bytes::clone()`.
- **Rate limiting**: in-memory sliding window (request count + token), per role, m/h/d windows.
- **Usage recording**: pluggable sink (SQLite default, ClickHouse optional); **granular token breakdown**: `prompt_tokens` / `completion_tokens` / `total_tokens` / `cached_tokens` (OpenAI `prompt_tokens_details` + Anthropic `cache_read_input_tokens`); **latency metrics**: `forward_latency_ms` (Hydra overhead before provider call) + `ttft_ms` (time to first token). All numeric fields default to 0 (no NULLs).
- **Per-tenant TLS**: SNI-based certificate selection with hot-reload (BoringSSL/OpenSSL).
- **Admin REST + UI**: full CRUD for all config entities, Prometheus `/metrics`, embedded dashboard.

## Deploy

### Docker (recommended)

```bash
# 1. cross-compile the linux/amd64 binary + build the image
./environment/build.sh

# 2. run the full stack (hydra + mock-tenant + clickhouse)
cd environment && docker compose up -d

# 3. register your providers (reads secure/config.json)
python3 ../environment/init.py
```

### From source

```bash
cargo build --release --features server
HYDRA_ADMIN_TOKEN=<token> ./target/release/hydra
```

## Configure

Hydra boots from **environment variables** (runtime) and stores all routing config in **SQLite** (managed via the admin API).

| Env var              | Default                          | Purpose                                              |
| -------------------- | -------------------------------- | ---------------------------------------------------- |
| `HYDRA_DB_URL`       | `sqlite:hydra.db?mode=rwc`       | SQLite database location                             |
| `HYDRA_LISTEN`       | `0.0.0.0:8080`                   | Proxy listen address (use `:443` + certs for TLS)    |
| `HYDRA_ADMIN_ADDR`   | `127.0.0.1:8081`                 | Admin REST + UI + `/metrics` listen address          |
| `HYDRA_ADMIN_TOKEN`  | —                                | Bearer token gating `/api/v1/*` (**required for admin**) |
| `HYDRA_ENCRYPTION_KEY` | —                              | Base64 of 32 bytes; encrypts provider api-keys at rest (**required**, fail-closed). Generate: `openssl rand 32 \| base64`. |
| `HYDRA_USAGE_SINK`   | `sqlite`                         | `sqlite` or `clickhouse`                             |
| `HYDRA_CLICKHOUSE_URL` | —                              | ClickHouse HTTP endpoint (when sink=clickhouse)      |
| `RUST_LOG`           | `info`                           | Log level                                            |

**Ports**: `8080`/`443` proxy · `8081` admin (REST + UI + metrics).

## Use

### Admin UI

Open `http://<host>:8081/admin/` and enter the admin token. Manage providers, models, keys, tenants, access, rate-limit roles, and view/invalidate the auth cache and circuit breaker.

### Admin REST

```bash
TOKEN=<your-admin-token>

# create a provider
curl -X POST http://localhost:8081/api/v1/providers \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"openai","key":"openai","name":"OpenAI","endpoint":"https://api.openai.com","weight":1}'

# create a tenant (auth_url mandatory) + grant provider + model access
curl -X POST http://localhost:8081/api/v1/tenants \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d '{"id":"acme","name":"ACME","domain":"acme.example.com","auth_url":"https://auth.acme.example.com/v","enabled":true}'

# list / reload / metrics
curl -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/providers
curl -X POST -H "Authorization: Bearer $TOKEN" http://localhost:8081/api/v1/reload
curl http://localhost:8081/metrics
```

### Point a client at Hydra

Any OpenAI-compatible client: set the base URL to the proxy and send the tenant's client api-key.

```bash
curl https://acme.example.com/v1/chat/completions \
  -H "Authorization: Bearer <client-api-key>" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-4o","messages":[{"role":"user","content":"hi"}],"stream":true}'
```

Hydra resolves tenant `acme` by domain → calls `auth_url` to authorize the key → routes `gpt-4o` to an allowed provider → swaps in a provider key → streams the response back → records usage (tokens + cached + TTFT).

## Project layout

```
crates/hydra-core/    pure domain logic (router, SWRR, breaker, SSE scan, limits) — zero I/O deps
crates/hydra-server/  Pingora proxy shell (terminate-mode), DB, auth, usage sink, TLS, admin
environment/          Dockerfile + docker-compose + mock-tenant + init script
integration/          Python CRUD test suite + e2e proxy test + mock LLM/auth
docs/                 design.md, ops.md, dev-plan.md, architecture analysis
```

## Cluster Mode

Single node stays zero-dependency. A cluster is opt-in: set `HYDRA_ROLE=leader|edge`
with a Redis (the one required external dependency) — self-sustaining (automatic
election, failover, join/leave, self-healing) and orchestration-agnostic
(compose / k3s / k8s / bare metal).

```bash
cargo build --release --features server,cluster-redis,usage-clickhouse
cd environment && docker compose -f docker-compose.cluster.yml up -d --scale hydra-edge=2
```

Live-acceptance verified (dual leader + stateless edges, docker Redis):
failover ~11–18 s, cross-node rate limiting, shared circuit breaker,
auth-cache invalidation bus, cert rotation without shared volumes.
See **[`docs/cluster.md`](docs/cluster.md)** (env table, failure matrix,
failover drill + acceptance record).

## More

- Design & architecture: [`docs/design.md`](docs/design.md)
- Architecture change (terminate-mode): [`docs/design-change-terminate-mode.md`](docs/design-change-terminate-mode.md)
- Operations runbook: [`docs/ops.md`](docs/ops.md)
- Interactive workflow diagram: [`docs/workflow.html`](docs/workflow.html)

Rust 1.83+ · Pingora 0.8.x
