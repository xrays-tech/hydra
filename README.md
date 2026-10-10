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
./environment/build.sh

# 2. run the full stack (hydra + mock-tenant + clickhouse)
cd environment && docker compose up -d

# 3. register your providers (reads secure/config.json)
python3 ../environment/init.py
```

### From source

```bash
cargo build --release --features server
# HYDRA_ENCRYPTION_KEY and HYDRA_USAGE_SINK are required (no defaults)
HYDRA_ADMIN_TOKEN=<token> \
HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)" \
HYDRA_USAGE_SINK=none \
./target/release/hydra
```

## Configure

Hydra boots from **environment variables** (runtime) and stores all routing config in **SQLite** (managed via the admin API).

| Env var              | Default                          | Purpose                                              |
| -------------------- | -------------------------------- | ---------------------------------------------------- |
| `HYDRA_DB_URL`       | `sqlite:hydra.db?mode=rwc`       | SQLite database location                             |
| `HYDRA_LISTEN`       | `0.0.0.0:8080`                   | Proxy **plaintext** listen address (always bound)    |
| `HYDRA_TLS_LISTEN`   | *(unset)*                        | Optional proxy **TLS** listen address, e.g. `0.0.0.0:8443`. Setting it (not the tenant certs) is what creates the HTTPS listener; per-tenant certs are selected by SNI. |
| `HYDRA_ADMIN_ADDR`   | `127.0.0.1:8081`                 | Admin REST + UI + `/metrics` listen address          |
| `HYDRA_ADMIN_TOKEN`  | —                                | Bearer token gating `/api/v1/*` (**required for admin**) |
| `HYDRA_ENCRYPTION_KEY` | —                              | Base64 of 32 bytes; encrypts provider api-keys at rest (**required**, fail-closed). Generate: `openssl rand 32 \| base64`. |
| `HYDRA_USAGE_SINK`   | — (**required**)                 | `clickhouse` (shared store) or `none` (no metering)   |
| `HYDRA_CLICKHOUSE_URL` | —                              | ClickHouse HTTP endpoint (when sink=clickhouse)      |
| `RUST_LOG` | *unset* | Log level; in-process default when unset is tracing's `error` — `info` is the recommended/deployed value |

> The only switch between single-node and cluster is **`HYDRA_CLUSTER_PEERS`**: unset = single node (the table above is all you need); set (≥3 entries, byte-identical on all three nodes) = enter a raft cluster — you then also need:

| Env var | Default | Purpose |
|---|---|---|
| `HYDRA_CLUSTER_PEERS` | — (**required for cluster**) | Static member list, `name=host:port` comma-separated; **order is the raft id**; ≥3 entries; identical on every node |
| `HYDRA_NODE_ID` | — (**required**) | This node's name from the member list; unique per node |
| `HYDRA_ARACHNE_LISTEN` | — (**required**) | This node's raft transport address; must equal its own entry in the member list |
| `HYDRA_CLUSTER_ID` | hash of the member list (**set explicitly**) | Cluster identity; setting it explicitly lets you change the member list later |
| `HYDRA_REDIS_URL` | — (**required**) | Shared data-plane state backbone (Redis is one cluster dependency, alongside the usage sink) |
| `HYDRA_REDIS_MODE` | `single` | Read by the cluster only; only `single` is accepted |

**Ports**: `8080` proxy (plaintext, always bound) · `8443` proxy (TLS, only if `HYDRA_TLS_LISTEN` is set) · `8081` admin (REST + UI + metrics).

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

Hydra resolves tenant `acme` by domain → calls `auth_url` to authorize the key → routes `gpt-4o` to an allowed provider → swaps in a provider key → streams the response back → records usage (tokens + cached + TTFT).

**Model catalog**: `GET /v1/models` is publicly readable (with or without an api-key — the presented key does not trigger auth, it only narrows the catalog by key-prefix binding) and returns the catalog of models this tenant can currently call (Hydra aggregates locally: union across allowed providers, filtered by the tenant model whitelist, excluding broken/disabled/keyless providers); chat calls still require an api-key. Admin has a read-only aggregate at `GET /api/v1/tenants/{tenant_id}/models`. See `dev-docs/design-tenant-model-catalog.md` and `dev-docs/aegis/plans/2026-09-08-public-models-catalog.md`.

## Project layout

```
crates/hydra-core/    pure domain logic (router, SWRR, breaker, SSE scan, limits) — zero I/O deps
crates/hydra-server/  Pingora proxy shell (terminate-mode), DB, auth, usage sink, TLS, admin
environment/          Dockerfile + docker-compose + mock-tenant + init script
integration/          Python CRUD test suite + e2e proxy test + mock LLM/auth
dev-docs/                 design.md, ops.md, dev-plan.md, architecture analysis
```

## Cluster Mode

Single node needs no external dependency; **the only cluster switch is `HYDRA_CLUSTER_PEERS`**
(a static member list, `name=host:port` comma-separated). Set it (≥3 entries) and this process is a
raft cluster member; leave it unset and you stay single-node with unchanged behavior. The member
list **order is the raft id** and must be byte-identical on every node. **Minimum 3 members**
(`MINIMUM_MEMBERS = 3`); fewer is rejected while parsing the member list.

The cluster is three **identical** raft members (each runs data plane + admin API + local SQLite +
raft); there is **no edge role and no `--scale`**. Exactly **one writer** exists at any time, decided
by a raft write probe. The control plane is Arachne (raft-linearizable KV); Redis only carries
approximate data-plane state.

Cluster build / run:

```bash
# requires the arachne feature, or a node with HYDRA_CLUSTER_PEERS set refuses to boot
cargo build --release --features server,cluster-redis,arachne,usage-clickhouse
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"          # every node uses it
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # identical across the cluster
cd environment && docker compose -f docker-compose.cluster.yml up -d   # 3 identical members, do NOT --scale
```

Per node: `HYDRA_CLUSTER_PEERS` (identical on all), a distinct `HYDRA_NODE_ID` and
`HYDRA_ARACHNE_LISTEN` (the latter must equal this node's own entry in the list), `HYDRA_REDIS_URL`,
`HYDRA_ADMIN_TOKEN`, `HYDRA_ENCRYPTION_KEY` (identical), `HYDRA_USAGE_SINK`; **strongly set
`HYDRA_CLUSTER_ID` explicitly** (default = hash of the member list, so changing the list changes
cluster identity and makes every node refuse to start).

Measured (3 real processes, docker Redis): election/failover **~1.1–1.6 s**, no dual writer ever
observed, and on quorum loss admin writes return 503 immediately while the data plane keeps serving.
See **[`dev-docs/cluster.md`](dev-docs/cluster.md)** (member list, failure matrix, membership-change SOP)
and [`environment/docker-compose.cluster.yml`](environment/docker-compose.cluster.yml).

## More

- Design & architecture: [`dev-docs/design.md`](dev-docs/design.md)
- Architecture change (terminate-mode): [`dev-docs/design-change-terminate-mode.md`](dev-docs/design-change-terminate-mode.md)
- Operations runbook: [`dev-docs/ops.md`](dev-docs/ops.md)
- Deployment guide (single node / compose / K3s / K8s): [`dev-docs/deployment.md`](dev-docs/deployment.md)
- Interactive workflow diagram: [`dev-docs/workflow.html`](dev-docs/workflow.html)

Rust 1.83+ · Pingora 0.8.x
