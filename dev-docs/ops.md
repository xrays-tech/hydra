# Hydra — Operations Runbook (v1)

> Audience: SRE / on-call for a single-instance `hydra` deployment.
> Reference: `dev-docs/design.md` (§6 lifecycle, §8 failover/breaker, §11 auth,
> §12 TLS, §13 admin, §15 deploy, §16 security, §17 metrics).

This runbook covers **deployment, configuration, graceful zero-downtime upgrade,
cert rotation, rate-limit tuning, breaker operations,
troubleshooting**, and the **load baseline** established in wave-6. It is the
single source of truth for ops; the design doc is the source of truth for
behaviour.

---

## 1. Deployment shape (design §15.3)

Hydra ships as a **single static binary** and talks to a **shared PostgreSQL or
MySQL database**. The backend is selected at compile time (`DB_BACKEND=postgres|mysql`
→ feature `db-postgres`/`db-mysql`); SQLite is deleted. No local config database,
raft data directory or per-node config volume exists. Redis is required only for
cluster mode (`HYDRA_REDIS_URL`); a single node needs the relational database, the
usage store, and nothing else.
**Configuration is environment-only**: there is no config-file loader — no `toml`
dependency in any `Cargo.toml`, and `grep -rn 'hydra\.toml' crates/` is empty
(re-verified 2026-09-29). The `hydra.toml` schema in design §15.1 is a **target**,
and anything you put in such a file today is ignored.

```
/opt/hydra/hydra          # the release binary (self-contained: UI embedded)
```

Build the release binary (exactly one backend feature):

```bash
# postgres (default image tag hydra:postgres / hydra:latest)
cargo build --release --features server,usage-clickhouse,db-postgres
# mysql (hydra:mysql)
cargo build --release --features server,usage-clickhouse,db-mysql

# or let environment/build.sh do the cross-compile + image:
./environment/build.sh                    # DB_BACKEND=postgres
DB_BACKEND=mysql ./environment/build.sh   # mysql variant
```
（`usage-clickhouse` 是 compose 默认 sink 需要的；`server` 本身不隐含它。）

The binary embeds the admin UI at compile time (`include_dir!`), so the
`admin-ui/{index.html,app.js,api-docs.js,style.css}` files are **not** needed on
disk at runtime. The release binary is the only artefact you ship.

### 1.1 Environment variables (single source of truth for runtime knobs)

| Var | Default | Purpose |
|-----|---------|---------|
| `HYDRA_ADMIN_TOKEN` | *(unset)* | **Required.** Admin bearer token (design §13.3). Unset ⇒ admin API denies everything (fail-closed). **Never put this in `hydra.toml`.** |
| `HYDRA_ENCRYPTION_KEY` | *(unset)* | **Required.** Base64 of 32 bytes; AES-256-GCM master key encrypting provider api-keys at rest. Unset ⇒ the binary refuses to start (fail-closed). Generate with `openssl rand 32 \| base64`. Load from an `EnvironmentFile=` (see §1.2); never inline. A matching `HYDRA_ENCRYPTION_KEY_FILE` (**raw 32-byte file**) is also accepted, and **the file wins when both are set**. Measured 2026-09-30 (`integration/test_master_key_sources.py`): the two forms are the **same keystream** — rows sealed through one open through the other, in both directions; a trailing `\n` or `\r\n` in the file is trimmed (so `openssl rand 32 > key` and a Kubernetes secret volume both work), while a trailing **space** is not (only line endings are trimmed, and the error counts the bytes); a file holding **base64** text fails with `master key must be 32 bytes, got 44: HYDRA_ENCRYPTION_KEY holds the BASE64 … while HYDRA_ENCRYPTION_KEY_FILE holds the RAW bytes …`; and with neither variable set the binary refuses to start naming both. |
| `HYDRA_ENCRYPTION_KEY_VERSION` | master-key version tag written to new ciphertext (default 1). A rotation sets the NEW version here (positive integer; `0`/garbage is refused at startup rather than silently defaulted). |
| `HYDRA_ENCRYPTION_KEY_PREVIOUS` (+ `_PREVIOUS_VERSION`) | the OLD master key during a rotation window: with it set, the provider keeps a key ring so existing ciphertext still opens while `HYDRA_RESEAL_SECRETS=1` rewrites every row. Drop both after a clean re-seal report. |
| `HYDRA_RESEAL_SECRETS` | *(unset)* | **One-shot maintenance switch**: set to `1`, `true`, `yes` or `on` (case-insensitive) to re-seal every stored secret under `HYDRA_ENCRYPTION_KEY`/`_VERSION` and EXIT — the process does not serve traffic in this mode. Run it once during a rotation, with the old key still present in `HYDRA_ENCRYPTION_KEY_PREVIOUS`; the report (`provider_keys=… tenant_certs=… limit_keys=… already_current=… failed=…`) decides the exit code, and `failed` non-empty means **do not delete the previous key**. A row whose version already equals the current one is re-verified by actually opening it, so "already current" means "opens under the current key" — measured live 2026-09-30 with a row *labelled* current but sealed with other material: it is reported as `reseal FAILED: … labelled key_version 2 (the CURRENT version) but it does NOT open under the current key …`, exit 1. `0`/`false`/`no`/`off` (and unset) mean "serve normally" as you would expect. **Any other value is REFUSED at startup** (`exit 1`, with the offending value echoed): before 2026-09-30 every unrecognised value — `YES`, `on`, `TRUE`, `reseal`, `2` — silently fell through to "serve traffic normally", so a typo in this one-shot command left the operator with a healthy-looking node and **no indication that the rotation never ran** (measured in `integration/test_key_rotation_live.py`). See §3. |
| `HYDRA_DB_URL` | *(compiled-backend fallback)* | **Required in practice.** Shared PostgreSQL/MySQL URL. The scheme must match the compiled backend (`db-postgres` ⇒ `postgres://…`, `db-mysql` ⇒ `mysql://…`); there is no runtime driver switch. |
| `HYDRA_LISTEN` | `0.0.0.0:8080` | Proxy **plaintext** listener. Always bound — the listener topology is derived from configuration only, never from whether tenants have certs (see `dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`). |
| `HYDRA_TLS_LISTEN` | *(unset)* | Optional proxy TLS listener, e.g. `0.0.0.0:8443`. **Setting this is what enables HTTPS** — per-tenant certificates are then selected by SNI. Unset with tenant certs present ⇒ the certs are NOT served (logged as an error + `hydra_listener_misconfig_total`); set but the address cannot be bound ⇒ plaintext keeps serving and the failure is logged + counted. Must differ from `HYDRA_LISTEN`. |
| `HYDRA_ADMIN_ADDR` | `127.0.0.1:8081` | Admin REST + UI + `/metrics` listener. **Bind loopback only** (design §13.3). |
| `HYDRA_USAGE_SINK` | ***(required — no default)*** | Where usage goes. Accepted values are the registered backends (`dev-docs/usage-backends.md` is the list's owner): `clickhouse` — the shared store every node writes to (needs `HYDRA_CLICKHOUSE_URL`, and a build with `--features usage-clickhouse`; the release scripts build it) — or `none`, which records **nothing** and says so at startup, counts every discarded record on `hydra_usage_records_dropped_total{reason="sink_disabled"}` and answers `503 usage_store_unavailable` on `GET /usage`. Unset ⇒ the process **refuses to start** (ADR-0002 D-1: "where does the billing data go" is a decision, not a guess). `sqlite` was retired on 2026-10-07 and is refused **by name**; it wrote usage into the node's own database, so a cluster's usage was scattered across nodes and the node answering `GET /usage` was usually not the one that recorded the request. |
| `HYDRA_CLICKHOUSE_URL` | *(unset)* | ClickHouse HTTP endpoint, e.g. `http://hydra-clickhouse:8123` (required when `HYDRA_USAGE_SINK=clickhouse`). **Credentials ARE supported**: use `http://user:pass@host:8123` (sent as HTTP Basic auth) or query params (`?user=&password=`); other query params like `?database=dogress` are passed through verbatim. **Do not put a path in this URL** — the sink always POSTs to `/`; a path is *trimmed* rather than used (measured 2026-09-30: before that fix `http://host:8123/clickhouse` failed at connect with `invalid port value`, because the path was still glued to the port when it was parsed). **What the sink actually puts on the wire** (measured 2026-09-30 against a mock ClickHouse, `integration/test_clickhouse_sink_wire.py`): `POST /?<your params>&query=INSERT%20INTO%20usage_record%20(…)%20SETTINGS%20…%20FORMAT%20JSONEachRow`, with the usage **row values in the request BODY** as one JSON object per line — **not** as `param_*` query parameters (that binding form belongs to the usage *reader*, `usage/backends/clickhouse/mod.rs` — moved there from `usage_query.rs` by ADR-0002 T1.3). Useful when diagnosing from a network capture: the SQL appears percent-encoded in the URL, the row values do not, and `client_api_key` in the body is **masked** (`sk*******-1`), never the tenant's real key. |
| `RUST_LOG` | `info` (set by the image + every compose file) | `tracing` env filter — `main.rs:172` uses `EnvFilter::from_default_env()`. **`HYDRA_LOG` is NOT read**: it was listed here as an alias until 2026-09-29 and has never existed in code (`grep -rn HYDRA_LOG crates/ scripts/ tools/ environment/` is empty). Measured 2026-09-29 on a live instance: started with **only** `HYDRA_LOG=debug` it logs **0 lines** (while `/health` and the API still answer 200), with `RUST_LOG=debug` it logs ~130 lines — so an operator reaching for the documented alias during an incident gets no extra logging. Also note `info` is a **deployment** default, not a binary default: with neither variable set the same instance logs **0 lines** (`from_default_env()` defaults to ERROR-level), and the shipped `Dockerfile`/compose files are what set `RUST_LOG=info`. |
| `HYDRA_TENANT_API` | `on` | Master switch for the tenant API on the data plane (`/tenant/…`). `off`/`0`/`false` ⇒ the prefix is not intercepted at all and the process behaves exactly as before the API existed. |
| `HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS` | `2000` | How long the **fleet-converge barrier** waits for every live node to confirm an invalidation-stream event before reporting `pending` (`202` with `lagging`). The auth cache it originally served is gone (T10); the value is still read (`crates/hydra-server/src/tenant_api/mod.rs`) and wired into the admin fleet reporting for config-publish convergence. |
| `HYDRA_TENANT_API_RATE_LIMIT_PER_MIN` | `60` | Per-tenant cap on AUTHORISED requests (429 beyond it) — counts every authenticated request the node accepts for the tenant, including ones it then rejects with 4xx/5xx (only the 403 URL-tenant mismatch is exempt). The amplification budget: one tenant's credential must not be able to spend other tenants' availability. |
| `HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN` | `10` | Cap on FAILED authentications, per **source IP** and per **token digest** independently. |
| `HYDRA_TENANT_API_LOCKOUT_SECS` | `900` | Once a dimension exceeds its failure budget it is locked for this long. **The lockout is consulted only on the authentication-failure path**: a request with a valid token is never refused by it, so a locked-out guesser **can** distinguish `429` (wrong token) from `200` (right token) — the no-validity-oracle property is deliberately given up (marginal guessing value ≈ 0 at ≥16-char tokens, and the `403` URL-tenant-mismatch channel already exposes validity). |
| `HYDRA_TRUSTED_PROXIES` | *(unset)* | Comma-separated **IP or CIDR** allowlist of reverse proxies whose `X-Forwarded-For` is trusted (IPv4/IPv6; a bare IP is treated as /32 or /128). Unset/empty = trust nobody (use the socket peer IP — the conservative default). When the peer is trusted, the per-IP limiter reads **all** `X-Forwarded-For` header lines (in order) and keys on the **rightmost** address that is not itself a trusted proxy, falling back to the peer when there is no `X-Forwarded-For` / all entries are trusted proxies / any entry is invalid. **A malformed entry fails startup.** Misconfiguration risk: trusting a proxy that does not strip inbound `X-Forwarded-For` lets a client forge it and rotate its per-IP bucket, effectively disabling the per-IP dimension. |
| `HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS` | `31` | E3 window ceiling. Not cosmetic: the ClickHouse table's key leads with `created_at`, so a wide window scans every tenant's rows in it. |
| `HYDRA_SESSION_AFFINITY` | `on` | Session-sticky routing switch. With a session header (priority `X-Opencode-Session` → `Session-Id` → `x-conversation-id`; first non-empty after trim, ≤256 B wins), the same `(tenant_id, model_key, api_key_id, session)` resolves to a STABLE provider via weighted rendezvous: `score = -ln(u)/weight`, **minimum** wins (ties by `provider_id` ascending), so `P(i) ∝ weight` — NOT argmax-of-hash. `off`/`false`/`0` ⇒ the no-session-header path (legacy SWRR). Parse is **TOTAL**: unset or an unrecognised value falls back to the default `on` and does NOT refuse startup (a routing hint, not a safety control). Only ever picks *within* the matched `global_role` union; breaker / admission / rate limits still apply. Read at `main.rs:667` (`std::env::var("HYDRA_SESSION_AFFINITY")`). |
| `HYDRA_MAX_USERS_PER_TENANT` | `1000` | Ceiling on ONE tenant's `user` rows (plan §3.5). Enforced on the CREATE path of `PUT /tenant/{tid}/api/v1/users` ONLY — updating an existing row adds no row, so a tenant at its ceiling can still edit what it has. Over it ⇒ **400 `hierarchy_limit_exceeded`** naming this env. Unset / `0` / unparseable falls back to `1000`. Read via `env_positive_u32("HYDRA_MAX_USERS_PER_TENANT", DEFAULT_MAX_USERS_PER_TENANT)` in `crates/hydra-server/src/tenant_api/mod.rs`. |
| `HYDRA_MAX_HIERARCHY_ROWS` | `3000` | Ceiling on the TOTAL rows of the FIVE identity tables (`user_group` + `user` + `user_api_key` + `tenant_access_token` + `global_role`) — plan §3.5, sized for headroom under the `multi_put` 4096-entry publish budget. Enforced on EVERY hierarchy CREATE: `PUT .../user-groups`, `PUT .../users`, `POST .../api-keys` (updates are not checked, they add no row). Over it ⇒ **400 `hierarchy_limit_exceeded`** naming this env. The count is taken from the PINNED config snapshot (no extra DB read on a write path); `user_group`/`user` rows and EVERY api-key row are all counted, `tenant_access_token`/`global_role` count only ENABLED rows, so disabled rows in those two tables make the ceiling slightly LOOSE — the direction that admits a create rather than refusing one. Unset / `0` / unparseable falls back to `3000`. Read via `env_positive_u32("HYDRA_MAX_HIERARCHY_ROWS", DEFAULT_MAX_HIERARCHY_ROWS)` in `crates/hydra-server/src/tenant_api/mod.rs`. |
| `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` | `5000` | Deadline for an E3 read, independent of the writer's `HYDRA_CLICKHOUSE_IO_TIMEOUT_MS`. |
| `HYDRA_CLICKHOUSE_IO_TIMEOUT_MS` | `15000` | Deadline for ONE write attempt (the response to an `INSERT`). A flush that trips it is **retried** with the same `insert_deduplication_token`, so a slow ClickHouse costs latency inside the flush budget rather than a lost batch; the buffer keeps up to `MAX_RETAINED` (10 000) rows in the meantime. |
| `HYDRA_CLICKHOUSE_CONNECT_TIMEOUT_MS` | `3000` | Deadline for the TCP connect to ClickHouse. Distinct from the write deadline on purpose: a black-holed host must fail the batch quickly so the retry/backoff loop can report it (`hydra_usage_records_dropped_total`) instead of holding the flush for the full write budget. |

> **Known listener limitation (recorded here, not hidden in a test):** the startup
> planner compares listener addresses as **strings**, so the *same port on different
> bind addresses* (`HYDRA_LISTEN=0.0.0.0:8080` together with
> `HYDRA_TLS_LISTEN=127.0.0.1:8080`) is **NOT** rejected statically — only *identical*
> addresses are caught (the fatal `HYDRA_LISTEN and HYDRA_TLS_LISTEN both point at …`
> check). The case is left to the runtime: `probe_bind` plus Pingora's all-or-nothing
> service build decide at startup. Do **not** deepen the static check casually: "same
> port on different interfaces" is a legitimate configuration on some hosts, so a
> stricter check would reject valid setups. The limitation is pinned by an explicit
> `#[ignore]`d test —
> `crates/hydra-server/tests/boot_listeners.rs::same_port_on_different_addresses_is_not_detected_statically`
> — and CI **does** now run it: the `optional-features` job runs
> `cargo test -p hydra-server --test boot_listeners -- --ignored` (`ci.yml`), and the
> `live-deps` job runs the ClickHouse ones. (This note used to say CI "never" ran
> `--ignored`, with a `grep -c` of 0 as evidence; that was true before the fix that
> added those two steps.) `scripts/check_ci_wiring.cjs` asserts this stays true: every
> file with a real `#[ignore]` must be run by a step passing `--ignored`.

> The current binary reads the proxy/admin addresses and DB URL from env (the
> `hydra.toml` shape in design §15.1 is the *target* schema; env vars are the
> *current* mechanism). Prefer env for secrets; `hydra.toml` may carry
> non-secret defaults.

> **Auth is now INTERNAL (no `auth_url` is ever consulted).** A data-plane request is authenticated by its client api-key matched against the tenant's stored `user_api_key` rows (`ConfigData.users_by_api_key_hash`): unknown / disabled / expired key → **401 `invalid_api_key`**; user / group / tenant disabled → **403**; a user flagged `billing_status = delinquent` (set via the tenant API `PUT /users/{id}/billing`) → **402 `user_delinquent`** (`{"error":{"message":"user_delinquent","type":"insufficient_quota","code":"user_delinquent"}}`). No external auth hop, no auth cache, no `auth/cache/invalidate` round-trip — a revoke or a delinquency flips on the next request (SLO ≤5 s across nodes via config publish + 1 s materialise). Tenant access tokens (`tenant_access_token`, now **multi-token** per tenant, `ht_` prefix, plaintext shown exactly once at creation) gate ONLY the tenant self-service API. The `auth_url` and `access_token_hash` columns are DROPPED (migration `0015` / T10 complete), and so is the old `HttpAuthChecker` / `AuthCache` / `redis/auth_cache.rs` surface — nothing sends a request to an external auth service any more. `DELETE /api/v1/auth/cache` and the tenant-plane `POST /tenant/{tid}/api/v1/auth/cache/invalidate` are REMOVED; calls to those paths are ordinary 404s.

> **Hierarchy ceilings (capacity, plan §3.5) — ALL THREE WIRED.** The tenant self-service API refuses a mint past its ceiling with **400 `hierarchy_limit_exceeded`** (never a silent shard), and it never refuses an update of an EXISTING row: these ceilings count ROWS, and an update adds none. Read in `TenantApiConfig::from_env` (`crates/hydra-server/src/tenant_api/mod.rs`, the three multi-line `env_positive_u32` calls at `:278-293`) — `HYDRA_MAX_USER_API_KEYS_PER_TENANT` (default `1000`, per tenant, enforced in `handlers::user_api_key_create`), `HYDRA_MAX_USERS_PER_TENANT` (default `1000`, per tenant, `handlers::user_upsert`), `HYDRA_MAX_HIERARCHY_ROWS` (default `3000`, GLOBAL over the five identity tables, enforced on all three hierarchy creates; the count comes from the PINNED snapshot — the table row above says exactly which rows it includes, and in which direction it can be loose). Before this wiring only the api-keys ceiling was live, and the `multi_put` 4096-entry publish budget was the only bound on the rest of the identity surface.

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
Environment=HYDRA_DB_URL=postgres://hydra:<pass>@127.0.0.1:5432/hydra
Environment=HYDRA_LISTEN=0.0.0.0:8080
Environment=HYDRA_ADMIN_ADDR=127.0.0.1:8081
Environment=HYDRA_USAGE_SINK=clickhouse
Environment=HYDRA_CLICKHOUSE_URL=http://127.0.0.1:8123
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

[Install]
WantedBy=multi-user.target
```

Load the secret from an `EnvironmentFile=` owned by root (`chmod 600`); never
inline it.

### 1.3 Database at-rest access (§16.2)

Provider api-keys and certificate private keys are stored **AES-256-GCM encrypted at
rest** in the shared database (`api_key_ciphertext` / `api_key_nonce` / `key_version`
columns; see `hydra-server::crypto`). They are decrypted to plaintext only in memory,
at the DB boundary. The master key (`HYDRA_ENCRYPTION_KEY`, §1.2) is the primary
protection — a stolen database dump alone is useless without it. Harden the database
itself: a dedicated least-privilege account, network ACLs / TLS, encrypted storage and
encrypted backups. The old per-file `chmod 0600`/`data/` instructions no longer apply.

#### Rotating the master key (`HYDRA_ENCRYPTION_KEY`) — supported procedure

**Do not simply replace the key.** Everything at rest is sealed under it
(provider api-keys and tenant certificate private keys), so with only a new key
configured the boot-time decrypt fails and the process refuses to start — and the
admin API that could re-enter those keys runs in that same process, so there is
no way back in. The supported path is a two-key window plus a one-shot re-seal:

```bash
# 1. Generate the new key and pick its version (a positive integer greater than
#    every version currently in use; never 0). List what is in use with the DB client:
#      psql "$HYDRA_DB_URL" -c \
#        'SELECT key_version, COUNT(*) FROM provider_key GROUP BY key_version;'
#      psql "$HYDRA_DB_URL" -c \
#        'SELECT cert_key_version, COUNT(*) FROM tenant WHERE cert_key_version IS NOT NULL GROUP BY 1;'
#    (MySQL: mysql --execute='SELECT key_version, COUNT(*) FROM provider_key GROUP BY key_version;' "$DB")
NEW_KEY="$(openssl rand 32 | base64)"
NEW_VERSION=2        # e.g. current is 1 (the default)
OLD_VERSION=1

# 2. Run ONCE with the new key as current and the old key as previous.
#    The process re-seals every row, prints a report and EXITS (it does not serve).
docker run --rm \
  -e HYDRA_DB_URL="$HYDRA_DB_URL" \
  -e HYDRA_ENCRYPTION_KEY="$NEW_KEY" -e HYDRA_ENCRYPTION_KEY_VERSION="$NEW_VERSION" \
  -e HYDRA_ENCRYPTION_KEY_PREVIOUS="$OLD_KEY" -e HYDRA_ENCRYPTION_KEY_PREVIOUS_VERSION="$OLD_VERSION" \
  -e HYDRA_RESEAL_SECRETS=1 \
  hydra:latest
# → `reseal: provider_keys=N tenant_certs=M limit_keys=L already_current=K failed=0`
#   exit code 0 = every secret now opens under the new version.
#   Any `reseal FAILED: ...` line + exit code 1 = that row was left UNTOUCHED;
#   do not drop the old key until the report is clean.

# 3. Restart the fleet with the NEW key only (drop both PREVIOUS variables).
```

Notes, deliberately explicit:

- **The window is a window.** While `HYDRA_ENCRYPTION_KEY_PREVIOUS` is set, the
  ring holds both keys: new writes use the new version, old ciphertext still
  opens. Leaving the previous key in place keeps the old key's blast radius
  alive, so drop it as soon as the report is clean.
- **A row nobody can open is reported, never rewritten.** The tool needs the key
  that sealed it; if you have lost that key, that row is unrecoverable and the
  report says so by name instead of silently counting it as rotated.
- **Run it against a stopped fleet** (or accept that a live process keeps its
  in-memory plaintext until its next restart): writes are one transaction per
  table, but a live node's config snapshot is not re-read by the tool.
- The re-seal reads and writes inside ONE transaction, so a crash halfway leaves
  the DB entirely before or entirely after — never a mix.

For higher assurance use SQLCipher or full-disk encryption (design §16.2). The
admin API always returns masked provider keys; `?reveal=1` is accepted as a
no-op for backward-compat but never reveals plaintext (§16.2).

---

### 1.4 Backup & restore

The shared relational database **is** the configuration authority: providers,
sealed provider keys, tenants, user groups/users/api-keys, global/limit roles and
certificate content all live there. Redis is not a source of truth, so it does not
need a backup.

```bash
# PostgreSQL — consistent snapshot of a live database
pg_dump --format=custom --file="/backup/hydra-$(date +%F).dump" "$HYDRA_DB_URL"

# MySQL
mysqldump --single-transaction --routines --triggers \
  --result-file="/backup/hydra-$(date +%F).sql" hydra
```

**Restore** (stop every Hydra node first, or accept that they serve a stale cache):

```bash
# PostgreSQL
pg_restore --clean --if-exists --dbname "$HYDRA_DB_URL" /backup/hydra-2026-11-10.dump

# MySQL
mysql hydra < /backup/hydra-2026-11-10.sql
```

Then start the nodes with **the same `HYDRA_ENCRYPTION_KEY` / `_VERSION`** that was
current when the snapshot was taken. Migrations run at boot.

> ⚠️ **The backup is useless without that key.** A node pointed at a restored
> database under a different master key refuses to serve (the sealed rows cannot be
> opened). Keep the key separate from the database and from the host.
>
> ⚠️ **Migrations are forward-only at boot.** A backup taken under an older binary
> may not match the schema the current binary expects. Keep the matching binary with
> the backup, or validate the restore on a copy before switching production.
>
> ⚠️ **A revision rewind needs one forced reload.** If the restored
> `config_revision.version` is lower than a node's in-memory version, that node sees
> "no advance" and skips the normal reload. Run `POST /api/v1/reload?force=1` on it
> (admin token) to re-read the restored database and converge.

ClickHouse usage data is backed up with ClickHouse's own tooling; it is not part of
the configuration database.

### Historical: destructive upgrade that dropped the local usage table (SQLite-era, migration 0013)

Usage used to have a second home in the node's own SQLite database. ADR-0002 (user ruling D-3)
retired that store **and dropped its table**, so from this release on:

1. **Before upgrading**, if the rows matter: back the database up
   (`sqlite3 hydra.db "VACUUM INTO 'usage-backup.db'"`, §"Backup") **and** export them
   (`sqlite3 -header -csv hydra.db "SELECT * FROM usage_record" > usage.csv`).
2. **After the first start** of the new binary the table — and every row in it — is GONE, and
   `GET /usage` answers only what ClickHouse holds.
3. **Rollback is one-way.** `sqlx::migrate!` records 0013 as applied, so reverting the *code* leaves
   a migration that exists in the database and not in the binary (`VersionMissing`); the two ways
   back are the file from step 1, or deleting that row from `_sqlx_migrations` by hand.
4. A deployment that has not set `HYDRA_USAGE_SINK` will not start at all (there is no default) —
   set `clickhouse` before upgrading, or `none` to run with metering off.

**Deliberately not covered here**: usage records live in ClickHouse
(`HYDRA_USAGE_SINK=clickhouse`; back it up with ClickHouse's own tooling). They used to have a
second home in the node's own SQLite database, and migration `0013` **drops that table** — see
§"Destructive upgrade" below for what to do before upgrading. A cluster node
loads the same shared database; there is no replica rebuild from a leader and no
`db::restore_config` any more.

---

## 2. Graceful zero-downtime upgrade (design §15.3)

Pingora has built-in socket handover via `SIGQUIT` (graceful shutdown of the old
process) + `hydra -u` (the new process inherits the listening socket from the
old). In-flight requests on the old process finish; new connections go to the
new process.

**Measured 2026-09-29 (round 68): this now works, and until then it could never
work.** Two independent causes, both fixed:

1. `-u` never reached Pingora — the binary passed `Opt::default()` and never read
   argv, so the new process never asked for the old one's sockets and died with
   `cannot bind … Address already in use … refusing to start` (the old process
   logged `Trying to send socks` into a socket nobody was listening on);
2. even with the flag, hydra's three pre-flight bind probes (plaintext, TLS, admin)
   aborted the handover, because during an upgrade those addresses are *legitimately*
   held by the predecessor and Pingora inherits them (`listen(fds)` looks the address
   up in the transferred FD table) — so the probes now stand down in upgrade mode.

End-to-end result with both fixes: a client hammering the data port every 50 ms saw
**0 refused connections and 0 non-listener answers across 624 attempts** spanning the
`SIGQUIT`, the handover and the old process's full drain+exit, and the port was still
served afterwards. Without `-u` the new process still refuses loudly — that refusal is
the guard against a silent split-brain bind, not a bug. The whole procedure is
asserted by `scripts/handover.test.sh` (CI `integration` job + local gate).

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

- **Upgrade socket path**: Pingora's upgrade socket (`upgrade_sock`, default
  `/tmp/pingora_upgrade.sock`) must be on a path shared by both old and new
  processes and writable by both. In containers with a read-only rootfs, mount a
  small tmpfs at that path (design wave-6 §6 risk note). If the path is not shared,
  the handover cannot happen and the new process fails to take the port over with
  `address already in use` — the same message the round-68 bug produced, so read the
  old process's log: `listener sockets sent` means the transfer happened, its absence
  means it did not.
- **Start the new process promptly.** The old process sends its sockets when it
  receives `SIGQUIT` and waits ~1 s for a receiver (measured: with the new process
  started 0.3 s later, the send completed 1.0 s after the signal). If nothing ever
  listens on the upgrade socket, the send fails and the old process simply continues
  draining — and its data listener is closed when the graceful shutdown starts
  (~5 s in), so the port goes dead for the remainder of the drain.
- **`systemctl restart` is NOT a zero-downtime upgrade** (measured 2026-09-29): a
  restart stops first and starts afterwards, and the old process closes its data
  listener the moment graceful shutdown begins. So the port REFUSES connections for
  roughly `HYDRA_SHUTDOWN_DRAIN_SECS` + 5 s — measured 125 refusals out of 313 probes
  with the drain set to 2 s (a 6.25 s window; the 20 s default therefore costs ≈25 s),
  against 0 refusals in 624 probes for the §2.1 handover. `KillSignal=SIGQUIT` (§1.2)
  only makes the drain graceful; it does not create a handover. To get one under
  systemd, have a wrapper run the §2.1 sequence (start `hydra -u` while the old pid is
  still draining), or accept the gap and upgrade at low traffic.
- **Config drift across upgrade**: the new process re-reads **env** on boot
  (`hydra.toml` is not read — see §1.1). If you changed env vars, set them before
  step 2.
- **DB schema**: backend-specific migrations (`migrations/postgres|mysql`) run on
  boot. A forward-only migration is safe during a rolling upgrade (new processes
  run it; old ones keep their connection). A backward-incompatible migration
  blocks rollback — keep the previous binary until you're confident, and take a
  database backup first (§1.4).

### 2.3 Verifying an upgrade (smoke)

```bash
# Before: continuous low-RPS probe through the proxy.
hey -z 60s -c 4 https://acme.example.com/v1/chat/completions ...

# During: run the upgrade. The probe must show zero non-2xx from connection
# resets — and with the handover there is NO rebind at all (measured: 0 refusals in
# 624 attempts; the new process inherits the listening socket, it does not bind it).
# `scripts/handover.test.sh` is exactly this probe, run in CI.

# After: GET /api/v1/health → 200, and /metrics still answers with the hydra_*
# families. NOTE that the counters RESTART AT ZERO: a handover ends in a NEW process
# with a fresh Prometheus registry (measured 2026-09-29: hydra_requests_total read 31
# before the switch and 0 after it). There is no counter "continuity" to verify, and
# `rate()`/`increase()` handle the reset — reading the reset as lost traffic is a
# false alarm. What IS worth asserting after an upgrade: /health 200, the metrics
# endpoint reachable, and the config snapshot unchanged — measured 2026-09-29 with a
# provider in the database: POST /api/v1/reload reported `version 1 / providers 1`
# both before and after the handover (both processes read the same database; only the
# PROCESS state, like the counters, starts fresh).
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
  -d '{"id":"t1","name":"Acme","domain":"acme.com","cert_file":"data/certs/acme.crt","cert_key":"data/certs/acme.key","enabled":true,"created_at":"x","updated_at":""}'
```

The PUT triggers `ConfigStore::reload_all()` and the W4b cert-reload contract
re-resolves every cert path from the fresh snapshot. **New** TLS handshakes use
the new cert; **existing** connections are unaffected (they keep the cert they
negotiated).

**Measured 2026-09-29** (`integration/test_cert_reload.py`, run in CI): with three
self-signed certs told apart by subject and validity, `PUT` alone (no `/reload`, no
restart, same pid) switched a new handshake from `O=OLD-CERT` to `O=NEW-CERT` while a
session opened *before* the rotation still reported `OLD-CERT` — claims 2 and 3 hold
exactly as written. The mechanism is the one named above: disabling the post-write
`reload_all()` (falsification probe) made every handshake fail with no certificate at
all. Two operational details worth having in advance:
* an SNI that matches **no** tenant is **refused at the handshake** — there is no
  default certificate (`hydra::tls` logs `no cert matched SNI and no default
  configured`). Load-balancer health checks that speak TLS to the data port must send
  a real tenant `server_name`, or they will mark the node down while it is healthy;
  probing `/healthz` (token-free) instead avoids the issue entirely.
* starting with `HYDRA_TLS_LISTEN` set and **no** tenant cert yet is fine: the node
  logs `HYDRA_TLS_LISTEN is set but no tenant cert is loaded yet; TLS handshakes will
  fail until a certificate is written. No restart is needed once one is written.`

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


> **Per-provider admission limits are RESIZED on hot-reload (decision item D-14,
> implemented 2026-10-09).** `max_concurrency`, `max_queue_depth` and
> `queue_wait_timeout_ms` are read when a provider's gate is created and again on
> every subsequent `acquire`: a configuration change swaps in a NEW gate
> generation (a fresh semaphore) the next time that provider admits a request,
> while requests already in flight keep the OLD generation until they finish
> (in-flight permits hold an `Arc` to the generation they were issued from, so
> the old cap drains naturally). Concretely: `PUT /api/v1/providers/{id}` with
> the new limits returns 200 and the row changes (`GET …/providers/{id}` shows
> the new value), and the next request to that provider is admitted under the
> NEW limits — no restart needed.
>
> **The window before the resize is visible, not silent.** `GET
> /api/v1/concurrency` reports both sides: `max_concurrency` / `max_queue_depth` /
> `queue_wait_timeout_ms` are what the RUNTIME gate is enforcing, while
> `configured_max_concurrency` / `configured_max_queue_depth` /
> `configured_queue_wait_timeout_ms` are what the LIVE configuration snapshot asks for
> right now (resolved through `hydra_core::config::resolve_policy`, i.e. provider row →
> process default — not a value some earlier request happened to observe). Between a
> config write and the first request that applies it, the two disagree —
> `limits_stale: true` — and each applied resize increments
> `hydra_admission_resizes_total{provider}` and logs one `INFO` naming both sides.
> The pre-D-14 behaviour (gates never resized, a per-request *stale-limits* counter
> counting requests admitted under stale limits, restart required) is gone — when the
> resize landed the counter was renamed to `hydra_admission_resizes_total`.
Limits are configured as **roles** in the `limit_roles` table. Each role carries
`matching_*` dimensions (any `NULL` = match-all on that dimension), a
`limit_count` and/or `limit_token` ceiling, and a `window` (`m` / `h` / `d`).
The matcher selects roles for a request; the most restrictive surviving role
applies. **Note (decision D-11, 2026-10-09): `matching_provider` can never
match** — the limit pre-gate runs before routing, so no provider is known when
roles are matched — and the admin write boundary now **refuses** a non-NULL
value with `400 matching_provider_cannot_match` (POST and PUT). Legacy rows that
already carry it are still loaded and warned by name at config load; the only
writable value is `null` (match-all).

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

> **`matching_key` accepts the RAW client key, its mask, OR its `sha256:` digest — and since
> 2026-10-08 the COLUMN IS SEALED AT REST (decision D-16). USE THE DIGEST.**
> (`integration/test_limit_roles_enforcement.py` is the live drill for every form below; the
> `test_replica_fidelity.py` this note used to cite was retired with the edge role.) This section used
> to say nothing about the form
> while `design.md` said "NULL **or equal to** the client api-key" (the raw key), and the code
> silently compared only the mask. Now:
> * **the column is SEALED** (D-16①, 2026-10-08 — the ruling was "seal everything, re-seal legacy
>   rows without a manual step"): `limit_role.matching_key` goes in through `kp.seal`
>   (`db.rs::seal_limit_key`, exactly like a provider api-key), and a row written BEFORE that
>   decision is re-sealed the first time a loader reads it (`db::seal_legacy_limit_keys` — no
>   operator step; the node logs `sealed N legacy plaintext 'limit_role.matching_key' value(s)`
>   once). Master-key rotation covers the column too: `hydra --reseal` reports `limit_keys=N`.
> * **what sealing does NOT cover** — the admin plane: `GET /api/v1/limit-roles` returns `matching_key` MASKED
>   (digest form as-is, everything else via `mask_key` — P3-4, 2026-10-09), so the node never echoes a
>   recoverable client key; only something holding the master key can open the sealed envelope. Sealing
>   protects the shared database and its backups from a database-level reader; it does not make the
>   credential unrecoverable to the master-key holder.
> * **UPGRADE NOTE (this change bumps the config-tree format to `TOC_FORMAT = 4`)**: the entity's
>   bytes keep their shape but change MEANING, so a mixed-version cluster refuses each other's trees
>   **by name** (`the table of contents declares format 3; this build speaks 4`) instead of decoding
>   them. That is deliberate and it is the safe direction: a build speaking 3 would otherwise take the
>   envelope text as the value to match and a key-scoped role would silently **stop being enforced**.
>   The refusing node keeps serving its last-known-good config, so **upgrade every node before
>   publishing a config change** — an old node will not pick up new config until it is upgraded.
> * **Write the DIGEST form** — `sha256:<64 lowercase hex>` (decision D-16③) — when that matters: it
>   matches the same key and stores **nothing recoverable at all**, not even under the master key
>   (`printf %s "$KEY" | sha256sum`, then prefix the hex with `sha256:`).
> * the **raw** form also fires (measured on the leader: `[200, 200, 429, 429]` for
>   `sk-raw-only-probe-77`; before the fix the same leg measured `[200, 200, 200, 200]`, i.e. a
>   quota that looked configured and enforced nothing) — use it only when you accept the exposure
>   above, e.g. a throwaway key;
> * the **mask** form (`mask_key`: for a key of length ≥ 20 the first 10 and last 4 characters with
>   the middle replaced by `*`; for length 6–19 the first 2 and last 2) is what you can read out of
>   the `client_api_key` column of the usage rows — that is how to obtain it;
> * the **counting bucket is keyed by a DIGEST of the presented key** (D-15②, 2026-10-08), so neither
>   the raw key nor its mask ever becomes part of a Redis key name or a metric label — and **two
>   different client keys whose masks coincide have two separate windows**. (Before D-15② the bucket
>   used the mask and those two keys shared ONE quota: measured with `sk-fidelity-limited` and
>   `skzzzzzzzzzzzzzzzed`, which mask identically — once the first key's window was spent, the second
>   key's very first request was refused `429`. The window identity is now
>   `(role_id, digest(presented key))`; the one-off cost of the change is that existing windows
>   restarted once.)
> * **the node warns about what the VALUE costs** (added 2026-09-30; **reworded 2026-10-08 by D-16**,
>   because it used to say the column was plaintext and that stopped being true). A `matching_key`
>   that is **not** a mask and not a digest is a raw client key: it is now sealed at rest and in the
>   config tree, and still **recoverable by anything holding the master key** — but the admin API
>   never echoes it (P3-4, 2026-10-09: `GET /api/v1/limit-roles` returns `matching_key` MASKED), so
>   no admin response carries a live credential; the node says so by name at config load. **The
>   digest form (`sha256:<64 hex>`) is the one to use (decision D-16③)**: it matches the same key
>   and stores nothing recoverable at all — `printf %s "$KEY" | sha256sum` and prefix the hex with
>   `sha256:`. The **mask** form also works and no longer carries a shared-window cost (D-15②, see
>   the bullet above).
> * **where those config warnings appear** (measured 2026-09-30, `integration/test_limit_roles_enforcement.py` L9):
>   at **startup**, on **every config load**, and — because the admin write path reloads — **immediately
>   when you write the role** (`POST`/`PUT /api/v1/limit-roles` answers `201`/`200` and the warning is in
>   the node log right after it; no explicit `POST /reload` needed). **Since 2026-10-08 (decision D-17)
>   the write response ALSO carries them**: `POST`/`PUT /api/v1/limit-roles` answers with a `warnings`
>   array — **always present, possibly empty** — holding exactly the warnings whose subject is the role
>   you just wrote, so a script no longer has to read the leader's log to learn that the role it created
>   is unsound (e.g. a key-scoped role without a tenant scope is a shared cross-tenant budget;
>   `crates/hydra-server/tests/admin_api.rs::limit_role_write_returns_the_warnings_for_that_role`
>   pins both the non-empty and the empty case). The former "can never match" example — a role with a
>   non-NULL `matching_provider` — is no longer writable at all: the write boundary refuses it with
>   `400 matching_provider_cannot_match` (decision D-11, 2026-10-09). Warnings about the configuration as a whole (no subject)
>   are still log-only, and a **materializing node does not re-validate** the tree it receives (`apply_snapshot` loads
>   without validation, by design: the WRITER validated it before publishing) — so scripts that create roles
>   should check the log of the node that received the write, not assume a clean `201` means a sound role.
> * **always set `matching_tenant` on a key-scoped role.** The window belongs to
>   `(role_id, digest(presented key))` and nothing else, so a role with `matching_key` set and
>   `matching_tenant` NULL is a **cross-tenant** budget: two tenants whose auth backends both accept
>   the same key string share it, and one tenant's traffic can refuse the other's very first request.
>   Startup validation now warns by name (`limit_role 'r-x' scopes on matching_key but has
>   matching_tenant NULL: its window is shared by EVERY tenant that accepts that key`) — the same
>   treatment the inert `matching_provider` dimension already had (D-11), a warning rather than an
>   error because a deliberately shared budget is legitimate, it just must not be accidental.
>
> `matching_model` and `matching_tenant` are **exact** matches (not prefixes).
> operator key-prefix mechanisms). `matching_provider` is inert — the pre-routing gate runs before a
> provider is chosen — and **decision D-11 (2026-10-09) made the admin write boundary refuse any
> non-NULL value** with `400 matching_provider_cannot_match`; the config-load Warn remains as the
> backstop for legacy/file-loaded/restored rows. The two
> items this note used to leave open are both CLOSED and implemented (2026-10-08): the counting
> bucket keys on a digest of the raw key (**D-15②**) and the `matching_key` column is sealed at rest
> with legacy rows re-sealed by the loader (**D-16①**); the digest FORM (**D-16③**) is the one to
> prefer. This note describes what the code does today.
- **Multi-instance limitation (v1)**: counters are per-process. Multi-instance
  deployments need Redis-backed counters (v2 candidate, §16.6).

---

## 5. Auth is internal — no auth cache, no `auth_url` (T10 complete)

There is **no auth cache on the data plane**: a client api-key (`hk_…`) is matched
against the tenant's stored `user_api_key` rows. Unknown / revoked / expired / disabled key ⇒
**401 `invalid_api_key`**; user / group / tenant `enabled:false` ⇒ **403**; a `delinquent` user ⇒
**402 `user_delinquent`**. A revoke or a billing flip therefore needs **no cache-busting call**:
it takes effect on the **next** request on the node that received the write, and on the other nodes
within **≤5 s** (config publish + ~1 s materialise).

- The `auth_url` / `access_token_hash` columns and the legacy tables were DROPPED by migration `0015`;
  `DELETE /api/v1/auth/cache` and `POST /tenant/{tid}/api/v1/auth/cache/invalidate` are gone — both
  answer `404 not_found` now.
- The **invalidation bus** itself still exists for cross-node cache/config invalidation (cluster P4).
  Its health is watched on `hydra_invalidation_consumer_applied_id` /
  `hydra_invalidation_consumer_lag_events` / `hydra_invalidation_consumer_stalled_seconds` (§9.1 / §13.x)
  — a stalled consumer is the "one node is behind" alert, not a `202` from a retired endpoint.

### 5.1 Tenant self-service API — **on the DATA plane**

Each tenant can hold one or more **Access Tokens** (`tenant_access_token` rows, `ht_` prefix,
created via the admin Tenants form / API, stored as SHA-256 hashes, plaintext shown exactly once;
rotate by creating a new one and deleting the old). The tenant then serves itself, on the
**data-plane listener** (`HYDRA_LISTEN`, i.e. the port its clients already use):

| endpoint | what it does |
| --- | --- |
| `GET  /tenant/{tid}/api/v1/whoami` | the tenant's own non-secret config + snapshot version |
| `GET  /tenant/{tid}/api/v1/usage?since=…&until=…&group_by=…` | token usage in a window (`source: clickhouse`, or `503 usage_store_unavailable` when `HYDRA_USAGE_SINK=none`) |
| `GET /tenant/{tid}/api/v1/user-groups` · `PUT …/user-groups` · `DELETE …/user-groups/{id}` | list / upsert / delete your groups (non-empty delete ⇒ `409 group_not_empty`) |
| `GET /tenant/{tid}/api/v1/users[?group_id=]` · `PUT …/users` · `DELETE …/users/{id}` | list / upsert / delete your users (`user_group_id` required; delete cascades keys) |
| `GET …/users/{uid}/api-keys` · `POST …` · `DELETE …/{key_id}` | list / **mint (plaintext `hk_` shown ONCE)** / revoke a key |
| `PUT  …/users/{uid}/billing` | `active` / `delinquent` (delinquent ⇒ **402** on `/v1/*`) |

```bash
# who you are / what your config says
curl -s http://<data-plane-addr>/tenant/t-acme/api/v1/whoami   -H "Authorization: Bearer <tenant-access-token>"

# revoke one client key — effective on the NEXT request (no cache, no TTL)
curl -s -X DELETE http://<data-plane-addr>/tenant/t-acme/api/v1/users/$UID/api-keys/$KID   -H "Authorization: Bearer <tenant-access-token>"
```

> There are **no `wait=` / `timeout_ms` parameters any more** (they belonged to the retired
> invalidate endpoint). There is also no `503 config_not_published`: a tenant write commits to the
> shared database (one transaction, the shared lock row held), and that commit IS the success. Peers
> converge via the Redis config notification or the 5 s poll fallback. If a node's post-write
> reload fails, the write still succeeded and the node marks `snapshot_stale`; recover with
> `POST /api/v1/reload?force=1` (§5.5).

> 租户侧的完整对接说明（字段表、错误码、边界、排障）在
> [`tenant-api-integration.md`](tenant-api-integration.md) —— 给租户看，不是给你看的.
> 本节只讲运维关心的部分.


### 5.3 Usage accounting: two rates that legitimately disagree

- `GET /api/v1/stats/usage` (management) reads **in-process prometheus counters**:
  process health, **resets on restart**, no time dimension.
- `GET /tenant/{tid}/api/v1/usage` (tenant) reads the **metering store**
  (persistent rows, time window, `source: clickhouse`; `none` answers
  `503 usage_store_unavailable`): the reconciliation view.

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

> **Retry-idempotency migration — `non_replicated_deduplication_window` (2026-09-29).**
> The sink now sends a **stable `insert_deduplication_token` per flush**, so a batch
> whose response was lost *after* the insert committed is re-sent as a no-op instead
> of double-counting usage/quota/billing. That token is **only effective if the table
> carries the dedup window**: a plain `MergeTree` accepts the token and **silently
> ignores it** (verified on 24.3 — no error, just no deduplication).
> **Fresh** instances get it from `environment/clickhouse/init.sql` (and from the
> inline DDL in `environment/docker-compose.local.yml`); an **already-initialised**
> instance needs a one-off
> `ALTER TABLE usage_record MODIFY SETTING non_replicated_deduplication_window = 1000`
> (verified on the bundled instance: without it a re-sent batch produced a second row,
> with it the second insert is a no-op).
> **Residual, stated plainly (updated 2026-10-09):** batch-level dedup protects only a
> re-sent *batch* under the same token. A batch that outlives the retry window is
> re-flushed later under a **new** token, and a batch whose composition changed between
> attempts also gets a new token — so a lost ack spanning either boundary can still
> duplicate rows on a table that only has batch-level protection. **Since 2026-10-09 the
> schema carries row-level idempotency too** (decision P2-1): the table is
> `ReplacingMergeTree() ORDER BY (dedup_key, tenant_id, provider_id)` with a
> stable per-row `dedup_key` (the per-request trace id), so a duplicated row written by
> any retry collapses to one. **The collapse happens on background merge — Hydra's
> own `/usage` reads use `FROM usage_record FINAL` (review N2, 2026-10-09) so
> "counted once" is immediate for the gateway; an operator querying the table
> directly must add `FINAL` too.** **No version column is used** — every copy of one
> `dedup_key` is byte-identical (a retry re-sends the very same in-memory
> `UsageRecord`), and `created_at` cannot serve as a version anyway: it is a
> `String`, and ClickHouse rejects a String version column (verified live on 24.3,
> `Code: 169 BAD_TYPE_OF_FIELD` — if a future change ever lets the same key carry
> different content, add an Int*/DateTime version column and switch to
> `ReplacingMergeTree(version)`). **Fresh** instances get it from `environment/clickhouse/init.sql`
> and the inline DDL in `environment/docker-compose.local.yml`. An **already-initialised**
> `MergeTree` instance is NOT migrated automatically — a new binary writes a `dedup_key`
> column the old table does not have, so it must be rebuilt first (new table + copy +
> rename), see the migration steps below.

### 5.5 The user hierarchy: admin READS, tenant WRITES (plan §3.5)

The hierarchy (`user_group` -> `user` -> `user_api_key`) is written **only** through the
tenant self-service API (§5.1). Admin is the **read** half:

- `GET /api/v1/user-groups`, `GET /api/v1/users`, `GET /api/v1/user-api-keys` — operator
  inventory, filtered by `?tenant_id=`. Key material is **always masked** in admin responses
  (no plaintext, no `key_hash`).
- `POST`/`PUT`/`DELETE` on those admin paths are plain **404s** — there is no operator write
  path for the hierarchy. If you need to change a user/group/key, call it as the tenant
  on `/tenant/{tid}/api/v1/...` with that tenant's access token.

**Write semantics** (shared-database architecture — the write executes on the RECEIVING node):
one database transaction -> commit -> local reload -> peers reload via Redis notification or the
5 s poll fallback (**≤5 s**). There is no leader, no forwarding and no publish-to-head step.

- **A committed transaction is success.** There is no `503 config_not_published`; every node can
  accept the write.
- The `config_version` in a 200 body is the reconciliation baseline. A node whose snapshot has not
  reloaded yet keeps answering from its previous version until it converges.
- If the post-write reload fails, the write is still committed and the response is still 2xx; the
  node logs at `error` with `target: "hydra::tenant_api"` (`tenant` / `trace_id` / `error`) and sets
  `hydra_config_snapshot_stale=1`. Fix the offending row, then `POST /api/v1/reload?force=1`.

**What was removed here (T10 Stage C, migration `0015`):** the whole sub-tenant / key-prefix
plane — `sub_tenant`, `sub_tenant_route`, `provider_key_binding` tables, the `sub-tenants` /
`sub-tenant-routes` admin + tenant API paths, auto-generated key prefixes, the
`x-hydra-tenant-token` header, and `validate_sub_tenant_write`. Requests to those paths are
ordinary 404s; per-(tenant, user, model, key, session) pinning is **Global Role** scheduling
(`GET /api/v1/global-roles`), not prefixes.


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

Measured end-to-end 2026-09-30 (`integration/test_breaker_probe_revival.py`) — the revival
promise in the paragraph above is real, and so are its two sharp edges:

| Situation | Measured |
|---|---|
| A provider that **genuinely recovers** (its probe path answers again) | leaves the dead-set **on its own in 9.5 s** (probe interval 10 s) — **no traffic and no manual reset needed**, and it serves immediately after |
| A provider whose **chat** path is 100 % broken but whose `GET /v1/models` answers **404** | **revived anyway** after 10.0 s, so the next requests fail again: trip → revive → trip, i.e. **flapping**. This is the blind spot §9.1 records: the probe path is not a path a real inference request uses, and any status **`< 500` counts as healthy** |
| A provider answering **5xx** on the probe path | **stays dead** (deliberately: reviving on any response made a 500-ing provider flap). Note this is what the code does — the earlier version of this section said "any HTTP response (even a 401/429) is alive", which was wrong above 4xx |

- The HTTP probe treats **any status `< 500`** as "host alive" — a 401/429/404 means the host
  answered (unauthenticated, throttled, or without that route), while a 5xx means it is still
  failing. A bare TCP connect is the fallback when the HTTP probe itself errors (DNS, TLS,
  timeout).
- **There are no breaker knobs to tune.** `threshold` (**5**) and `probe_interval` (**10 s**)
  are compiled-in defaults (`hydra-server/src/proxy/config.rs::BreakerPolicy::default()`): no
  environment variable reads them and this project ships **no config file at all** (there is no
  loader), even though `design.md`'s `[breaker]` sketch and earlier revisions of this section
  read as if there were. An operator who wants a provider back sooner than the probe does has
  exactly one lever: `DELETE /api/v1/breaker/{id}` (§6.1). In cluster mode `HYDRA_BREAKER_QUORUM`
  is the one breaker setting that IS read (see §13).

---

## 7. ~~⚠️ `retry_after_connect`~~ — duplicate-billing risk (**已删除，见 terminate-mode**)

> **此配置项已在 terminate-mode 重写中删除，字段本身于 2026-10-09（P3-1）从代码彻底移除。**
> 以下内容保留作为历史参考。
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
| `HYDRA_MAX_REQUEST_BODY_HARD` | 32 MiB (= 33554432 **bytes**) | **413 Payload Too Large**（体为 `{"error":{"message":"request_body_too_large","type":"proxy_error"}}`）立即返回并关闭连接（`set_keepalive(None)`，§6.7）。在 `request_filter` 的全 body 读取循环中检测。**值按字节给**（例：`4194304` = 4 MiB）；`0`/非法值被拒绝并回落默认。2026-09-29 起可配置 —— 此前该值只来自 `ProxyConfig::default()`，本表下面那句"调低它降内存"的建议**根本无法执行**（实测：`HYDRA_MAX_REQUEST_BODY_HARD=1048576` 时 2 MiB 的体被 413，未设置时同一请求正常通过）。 |
| `HYDRA_REQUEST_BODY_TIMEOUT_SECS` | 60 s | **408 Request Timeout** + 关闭连接（不排空——这个客户端本来就不在发送）。这是**整个 body** 的总期限，不是空闲期限：pingora 对 HTTP/1 的 body 读取只有 per-read 期限（每个字节都会重置它），HTTP/2 则完全没有读超时，所以只有总期限挡得住"发一半就不发了"的客户端。**可达性（2026-09-29 实测，T10 更新）**：body 读取发生在**租户解析与认证之后**（`proxy.rs`：`resolve_tenant` → 认证 → 读全 body），so this hole is NOT triggerable by arbitrary anonymous clients — an authentication-rejected request gets answered before large-body upload (measured then: auth upstream unreachable ⇒ 33 MiB client got `503 auth_upstream_unavailable` with `size_upload=0`; since T10 the same shape is `401 invalid_api_key`). But ANY caller holding a valid api-key for the tenant reaches this step (there is no external auth service left whose laxity or absence gates it), so it remains a **cross-tenant availability** risk; the premise is simply "this request authenticates"。默认值按 `max_request_body_hard`（32 MiB）标定：60 s ≈ 0.5 MB/s 传满上限，慢链路请显式调大。`0` 被拒绝（那会 408 掉所有带 body 的请求）。 |
| body 读取出错 | — | **400 `request_body_read_error`** + 关闭。读取错误曾被当作"body 正常结束"，把**截断的** body 当完整请求转发给上游（静默损坏路径）；现在 fail-closed。 |

**Trade-off**（terminate-mode）：~~更大的软上限意味着更多请求可以安全故障转移（有利于可用性）~~ **不再适用**——全 body 已缓存，所有候选都能 O(1) 重放。内存占用 = 并发请求数 × 平均 body 大小（500 并发 × 2MB avg ≈ 1GB）。如需降低内存峰值，调低 **`HYDRA_MAX_REQUEST_BODY_HARD`**（字节；超过即 413——设置它需要重启进程）。

> ~~H2 paths are truly zero-copy on the forward leg; H1 paths incur one kernel copy per chunk (Pingora core limitation, design §8.5).~~ **（已废弃）** Terminate-mode 放弃 kernel-level 零拷贝（body 经 userspace buffer 传给 reqwest），但保留"零 JSON 往返"（body 字节未被 serde 处理）。详见 `dev-docs/design-change-terminate-mode.md` §5。

---

## 9. Observability (design §17, implemented W5)

- **`/metrics`** (self-hosted, no sidecar): Prometheus exposition on the admin port, **gated by the
  admin bearer token — one rule, every node**. It used to depend on the node's ROLE (an `edge` served
  it token-free), so the exposure depended on how a node was configured rather than on one rule; the
  `edge` role and its role-dependent probes are retired. The series carry `tenant`/`provider`/`model` labels, i.e. customer
  identifiers, so this is not public data.
  **Scrape configuration:** send `Authorization: Bearer <admin token>`
  (Prometheus: `authorization: {type: Bearer, credentials_file: ...}` with the
  file readable only by the scraper). The trade-off is explicit: the scraper
  holds the inventory-wide credential. If that is unacceptable, scrape the admin
  port over loopback (the default bind) or through a sidecar proxy that injects
  the header, rather than opening the port. `/healthz` and `/readyz` stay
  token-free on purpose — a load balancer must be able to probe without a secret.
  Key series: `hydra_requests_total`, `hydra_request_duration_seconds`,
  `hydra_upstream_duration_seconds`, `hydra_retries_total`,
  `hydra_tokens_total`, `hydra_auth_decisions_total`,
  `hydra_breaker_dead`, `hydra_breaker_state_transitions_total`,
  `hydra_limit_rejected_total`, `hydra_sni_host_mismatch_total`,
  `hydra_route_errors_total`, `hydra_mid_stream_errors_total`,
  **`hydra_usage_records_dropped_total{reason}`** — the one series that says you are
  **under-billing**: usage rows the sink could not deliver. Its reasons are `channel_full`
  (the bounded channel is full because the flush loop is retrying a backend that is down),
  `channel_closed`, `retention_cap` (the buffer hit `MAX_RETAINED` = 10 000),
  `shutdown_unflushed` (the final drain at shutdown failed) and **`sink_disabled`** — which is
  not a fault at all: on a node started with `HYDRA_USAGE_SINK=none` every record is counted
  here on purpose, so "this node meters nothing" is as visible as a real drop. Measured 2026-09-30
  (`integration/test_usage_drop_accounting.py`): with a dead ClickHouse and the shipped
  defaults, the FIRST drop appears after ~600 requests — the sink buffers a full 256-record
  batch and starts its backoff before the channel can overflow, so `channel_full` is the
  reason an operator will actually see first, and the 10 000-record `retention_cap` is much
  harder to reach. Every drop is also logged with `dropped_trace_id` (per-row diagnosis),
  and the counter STOPS growing as soon as the backend answers again.
- **Tracing**: structured logs via `tracing` (`RUST_LOG`). Every request carries
  an `X-Hydra-Trace-Id` echoed to the client and logged end-to-end.
- **Admin UI**: `http://<admin_addr>/admin/` — same-origin (the token is kept
  in `sessionStorage` for the current tab, so a reload stays signed in while
  closing the tab signs out). Useful for incident inspection (breaker dead-set,
  health, manual reload, key reveal with audit log).

### 9.0b Config watcher liveness

`hydra_config_watch_last_ok_unixtime` (unix seconds of the last successful subscriber
message or poll probe) is the watcher-liveness gauge. Alert when
`time() - hydra_config_watch_last_ok_unixtime > 3 * (HYDRA_CONFIG_POLL_MS/1000)` on a
Redis-configured node; use `hydra_config_version` advancing with writes plus
`hydra_config_snapshot_stale` as the convergence signal.

### 9.1 Alerting: which metric means what

**This repository ships the METRICS, not the alert rules.** The alert-rule files
(Prometheus rules / Alertmanager routes) belong to the OPERATIONS repository —
adding them here would create a second owner for the same policy. The table below
is the contract those rules are written against: every expression uses a metric
name and label that really exists in this codebase (`/metrics`).

| Alert | Expression | Meaning |
|---|---|---|
| Certs configured, no TLS listener bound | `hydra_listener_tenant_certs > 0 and hydra_listener_bound{protocol="tls"} == 0` | Tenant SNI is silently not served (the 2026-09-16 outage shape). **Read the two protocols differently**: `protocol="plain"` is LIVENESS (a startup self-check dials the port and rewrites it to 0 if the data plane never came up), while `protocol="tls"` is CONFIGURATION (it is published from the config decision and never revised). Both listeners live in ONE Pingora service whose bind is all-or-nothing, so a bind failure of either address shows up as `plain == 0` — a `tls == 1` therefore means "TLS was configured", not "TLS is accepting" |
| TLS listener configured, no certs | `hydra_listener_bound{protocol="tls"} == 1 and hydra_listener_tenant_certs == 0` | Handshakes will fail until a certificate is written |
| Invalid listener configuration | `increase(hydra_listener_misconfig_total[10m]) > 0` | Startup-time configuration problem (certs without a port / port without certs) |
| ~~Registry rows piling up~~ / ~~Registry reaping churn~~ | **RETIRED — do not write these rules** | They were `hydra_registry_nodes{state="dead"} > 5` and `increase(hydra_registry_reaped_total[1h]) > 20`. The node registry and its reaper are deleted (they belonged to the Redis-lease/raft era; cluster membership is now just `HYDRA_REDIS_URL`, so there is no heartbeat row to pile up and no reaping to churn), so **neither series is registered any more** and both rules were silently dead. What replaces them is the pair below: "is anyone the writer" and "is publishing getting through" — a registry-shaped question about dead rows has no meaning when there are no rows |
| **A node stopped reloading** | `hydra_config_snapshot_stale == 1` | The last reload (post-write or explicit `POST /api/v1/reload`) failed. The write may still have committed; this node serves the previous snapshot until a reload succeeds. **Recovery:** fix the row that fails validation, then `POST /api/v1/reload?force=1` (the gauge clears and the pending state takes effect) |
| Config fanout is failing | `increase(hydra_config_notify_total{result="error"}[10m]) > 0` | A local write could not publish `{version,origin}` on `hydra:config:changed`. The write and reload succeeded; peers converge through the `HYDRA_CONFIG_POLL_MS` fallback. Sustained means Redis/publish connectivity, not data loss |
| Config subscriber is lagging | `increase(hydra_config_bus_lagged_total[10m]) > 0` | The Redis pub/sub broadcast ring dropped messages before this node read them. The watcher immediately probes `config_revision.version` and reloads quietly (source `pubsub`), so a burst is repaired without waiting for the poll interval. Sustained growth means the subscriber cannot keep up with config-write volume; investigate Redis/client saturation, not data loss |
| A local write never advanced the shared revision | `increase(hydra_config_reload_total{source="local", changed="false"}[10m]) > 0` | A write path reloaded but the revision did not advance. Writes bump `config_revision` in the same transaction, so this should not happen on a healthy path; investigate the write statement (a missed bump). One occurrence during a no-op admin reload is normal |
| Cluster node is not polling | `increase(hydra_config_reload_total{source="poll"}[10m]) == 0 and hydra_config_version > 0` | A Redis-configured node should poll every `HYDRA_CONFIG_POLL_MS`; a flat counter means the watcher task died or the interval is enormous. This is the backstop, so a dead poller is a correctness risk after a missed notification |
| Admission gate resized on hot-reload | `increase(hydra_admission_resizes_total[10m]) > 0` | A provider's admission limits were changed and a new gate generation was applied (decision D-14, 2026-10-09): the `PUT /api/v1/providers/{id}` row change took effect on the next request. In-flight requests finished under the old cap, so a brief spike around a resize is expected. `GET /api/v1/concurrency` shows both sides for the affected provider (`max_concurrency` vs `configured_max_concurrency`) plus `limits_stale: true` only in the window between a config write and the first request that applies it |
| Upstream first-byte timeouts | `increase(hydra_upstream_first_byte_timeout_total[10m]) > 0` | The upstream **accepted the connection** and then sent no response headers; each attempt fails within `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS` (default 30s). Because the request WAS written, this is a post-send failure: the client gets `502 upstream_transport_error` **without failover** (replaying it could double-bill). A route that never completes the connect is a *different* case since 2026-09-30 — `HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS` (default 10s) fails it as a connect error, which **does** fail over and does **not** land in this counter. Before that bound existed it was misreported here *and* did not fail over: measured with a black-holed route, `codes=[502,200,502,200,502,200]` over six requests with `retries=0`. **Do not use `hydra_retries_total{stage="connect"}`** — nothing emits that label value (retries are recorded with `stage="terminate_loop"`), so such a rule could never fire |
| Failed credential attempts | `increase(hydra_admin_auth_failures_total{result=~".+_throttled"}[10m]) > 0` | A peer exceeded `HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN` failed attempts on the admin port. `result` is `<gate>_denied` (401) or `<gate>_throttled` (429). `gate` is **`admin`**: the internal
`cluster` gate and its token were deleted on 2026-10-05 (they guarded the `/api/v1/internal/*`
family, which no longer has routes), so `admin_*` is the only pair the counter emits now. The filter
`result=~".+_throttled"` is written against the SHAPE for exactly that reason. The gate used to be
unmetered: no limit, no counter and only a `debug!` line (filtered out at the shipped
`RUST_LOG=info`), so a brute-force run was free and invisible. The budget is per-process, so N reachable admin ports mean N times the budget; and IPv6 peers are bucketed by /64 (an unbounded address space inside one /64 cannot be used to rotate buckets) |
| Upstream streams wedged mid-answer | `increase(hydra_upstream_stream_idle_timeout_total[10m]) > 0` | The upstream sent response headers (and maybe some body) and then no byte for `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` (default 120s). The client has already received a `200` plus whatever arrived, so this is a **truncated** answer, not a retryable failure — and this path also feeds the circuit breaker |
| Provider quietly left the rotation | `increase(hydra_candidate_skipped_total{reason=~"no_key|breaker_dead"}[10m]) > 0` | A provider that WOULD have served the request was dropped while the candidate set was built: `no_key` (its api-keys are gone), `breaker_dead` (open breaker), `breaker_dead` (open breaker). **`invalid_weight` is deliberately NOT in the expression**: `weight < 0` cannot reach a running process — `migrations/0001_init.sql` declares `weight INTEGER NOT NULL DEFAULT 1 CHECK (weight >= 0)`, and every in-memory provider comes from those rows (`ConfigStore::load`), so the reason exists only for hand-built `ConfigData` (it is kept and unit-tested as defensive vocabulary, and this is the same honesty rule as the `stage="connect"` note below: do not write a rule for a series that cannot occur). The failover loop's own reasons (`missing_config`, `bad_endpoint`, `no_usable_key`) are unreachable with a valid config for the same reason and are likewise not in the expression. This is the ONLY per-provider signal for "it stopped being chosen": the request still SUCCEEDS through another provider, so nothing fails and `hydra_route_errors_total` (failures only, tenant-labelled) stays flat. **Deliberately absent: `soft_disabled`** (`weight == 0`) — that is an intentional act (§10.4 step 7), so it is not counted at all; a rule on it could never fire. The series exists only after the first drop (`IntCounterVec`), so an `== 0` rule is meaningless |
| ~~Replication stalled (upgrade window)~~ | **RETIRED — do not write this rule** | It was `changes(hydra_control_snapshot_version[10m]) == 0 and hydra_control_poll_total{result="ok"} > 0`, and BOTH terms are dead: `hydra_control_snapshot_version` lost its recorder with the polling client (removed with the old control plane; the gauge was still registered and exported as 0, so the rule looked fine) and `result="ok"` is a label value nothing has ever recorded — the only values emitted are `rate_limit_error` and `invalidation_trim_error`. A rule on it could never fire. The facts it was trying to express are covered by the two rows above |
| **Billing data is being LOST** | `increase(hydra_usage_records_dropped_total[10m]) > 0` | Usage rows the sink could not deliver — the gateway is **under-counting usage, quota and billing** for every drop. `reason="channel_full"` means the sink's backend is down and the bounded channel (batch size, 256) filled while the flush loop was backing off; `retention_cap` means 10 000 rows are already buffered; `channel_closed` means the sink was shut down; `shutdown_unflushed` means the final drain at shutdown failed; **`sink_disabled`** means this node was started with `HYDRA_USAGE_SINK=none`, so every record is discarded by choice (set a real backend to stop it). Every drop also logs `dropped_trace_id`, so a lost row can be traced to its request. Requests are still served normally (losing telemetry never breaks the proxy path — measured), which is exactly why this needs an alert rather than a symptom |
| Shared limits are OFF (Redis unreachable) | `increase(hydra_control_poll_total{result="rate_limit_error"}[10m]) > 0` | A cluster rate-limit check could not reach Redis, so that role was **failed open** — the request was admitted without its window being consulted (hard-coded direction, `HYDRA_RATE_LIMIT_FAIL_MODE` does not exist). One increment per matched role per affected request, so this is also the "how much traffic is unprotected" signal. Before 2026-09-30 this counter could grow **forever**: the pool was built without a reconnect policy, so a severed connection was never re-dialled and the limits stayed off until a restart (`ops.md` §13.5; measured 90 s with zero reconnect attempts). A flat line at a fixed total is stale, not healthy — check that it stops growing within seconds of Redis returning |

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
> default 30s), which bounds response HEADERS. Without it a "connected but silent"
> upstream burned the whole exchange on every attempt and every failover hop.
>
> The **body** has its own bound since 2026-09-29:
> `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` (default **120s**) is the maximum gap
> between two body chunks of one streamed response. It replaced the upstream
> client's former 300s *total* deadline — `ClientBuilder::timeout` runs until the
> response body has FINISHED, so it truncated every generation longer than 300s
> (the client got HTTP 200 plus half an SSE body and was billed in full). An idle
> window cannot do that: a stream that keeps producing tokens resets it, while a
> wedged upstream is cut after the window and counted in
> `hydra_upstream_stream_idle_timeout_total{provider}`. There is deliberately **no
> total-exchange cap** — a legitimately long generation must not be truncated.

> **Mid-stream failures are not retried.** Streaming responses that fail AFTER
> the `200` + first byte are sent cannot be retried (sent bytes cannot be
> unsent); the connection is closed and the failure is counted in
> `hydra_mid_stream_errors_total{provider}`.
>
> On the breaker: the **idle-bound** case (upstream went silent mid-answer) also
> feeds the circuit breaker — it is unambiguous provider-side evidence. The other
> mid-stream causes (a failing downstream write, i.e. usually the CLIENT going
> away) deliberately do **not**, because counting those would let a client's own
> disconnects mark a healthy provider dead. Distinguishing every remaining cause
> (and the half-open probe semantics) is still the recorded product decision
> §7-1.

> **Measured 2026-09-30** (`integration/test_client_disconnect.py`, a client that reads the first
> chunk and then resets the connection with `SO_LINGER 0`): the gateway **stops pulling** from the
> upstream — the mock delivered **3 of its 8 chunks** and no more — and the request is **still
> metered**: a usage row exists carrying the tokens the upstream had already reported
> (`tokens_in=5 tokens_out=8`). Two consequences worth knowing before you reconcile:
> the row is **indistinguishable from a complete answer** (`status_code=200`, `error` NULL — the
> store records the usage, not the truncation), and this event **does** land in
> `hydra_mid_stream_errors_total{provider}` while the provider stays **out of the dead-set** — i.e.
> the metric and the breaker deliberately disagree about the same event (the series covers every
> mid-stream cause; only the upstream-silent one is provider evidence). If you need to know how
> many answers your clients abandoned, that is still the open decision §7-1.

---

### 9.2 Historical: retired control-plane series (do not build dashboards on these)

The shared-database rewrite deleted the raft/Arachne control plane. These series are
**no longer registered**; the rows are kept so an operator who copied an old dashboard
or alert can see why it stopped resolving. Do not write rules against them.

| Retired series | Replaced by |
|---|---|
| `hydra_arachne_this_node_leader`, `hydra_arachne_leader_flips_total` | no leader; `hydra_config_version` + `GET /api/v1/cluster/status` |
| `hydra_arachne_publish_total`, `hydra_arachne_publish_readback_total`, `hydra_arachne_quorum_unavailable_total` | commit is the database transaction; `hydra_config_notify_total` only reports the fanout attempt |
| `hydra_arachne_config_bytes` | database size / provider/tenant row counts |
| `hydra_arachne_watch_total`, `hydra_arachne_watch_cache_total` | no materializer/watch cache; `hydra_config_reload_total{source="poll"}` + `hydra_config_snapshot_stale` |
| `hydra_replica_materialize_retries_total` | no replica materializer; `hydra_config_snapshot_stale` |
| `hydra_registry_nodes`, `hydra_registry_reaped_total` | no registry; cluster status is local-node only |
| `hydra_control_snapshot_version` | `hydra_config_version` |

**Cache panels that are still meaningful:**

```promql
# reloads by source; "poll changed=true" means pub/sub was missed or Redis was down
sum by (source, changed) (rate(hydra_config_reload_total[5m]))

# failed local fanout (poll fallback will repair)
increase(hydra_config_notify_total{result="error"}[10m])

# is this node serving the latest revision?
hydra_config_version
hydra_config_snapshot_stale
```

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
you which gate is firing in aggregate. A **successful** request can still have
dropped providers on the way (§9.1 `hydra_candidate_skipped_total`) — a provider
with no usable key stops being chosen silently, because the tenant's other
providers keep serving.

### 10.5 Database locked / busy / connection errors

The shared database serializes writers with a the shared lock row row (`SELECT … FOR UPDATE`)
and a bounded busy timeout. Common causes:

- **Too many concurrent writers / pool exhaustion.** Check pool metrics and the DB's
  `max_connections`; size the pool per node so `nodes × pool ≤ DB limit`.
- **A long-running external transaction** holding the shared lock row (a manual `psql` session,
  a migration, a restore) blocks every write until it ends.
- **MySQL deadlock/lock-wait timeout** (`1213` / `1205`) or PostgreSQL serialization
  failure (`40001` / `40P01`) is mapped to a retryable **503 `storage_busy`**. Retry the
  request; if it persists, find the competing transaction.
- **`database disk image is malformed` is gone** — that was the SQLite-era restore
  failure. A restore now goes through `pg_restore`/`mysqldump`; see §1.4.

### 10.5b MySQL 1615 on tenant delete

`DELETE FROM tenant` can hit MySQL error **1615 `Prepared statement needs to be
re-prepared`**. Hydra handles it with a scoped text-protocol delete
(`.persistent(false)`) inside the write transaction; operators need no action.
If you see 1615 from your own tooling, use a text-protocol client/statement rather
than a prepared statement that outlives a schema change.

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
| Auth hop (local api-key match) | *measure* | *measure* | Hash lookup against the tenant's `user_api_key` rows — no network hop (T10: there is no auth service). |

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

- **Single instance is the default, not the ceiling.** A single node is one process plus a
  shared PostgreSQL/MySQL database. Cluster mode is the same binary with `HYDRA_REDIS_URL`
  set: any node accepts writes, shared rate-limit/breaker state lives in Redis, and config
  changes fan out through Redis + a poll fallback.
- **No raft / no leader.** There is no consensus layer, no member list, no lease, no
  quorum and no `/healthz/leader`. The availability boundary is the database.
- **Single static admin token.** No RBAC, no token rotation (v2, §16.6 / §13.3).
- **No web UI auth** beyond the in-memory token prompt. The UI is a power-user tool; for
  fleet management use the REST API.
- **PostgreSQL and MySQL are the only config stores.** ClickHouse is the shared usage store
  (`clickhouse`, mandatory in practice) or metering is explicitly off (`none`). SQLite is
  deleted, including tests/dev.

For the remaining v2 backlog see design §16.6.

---

## 13. Cluster mode operations (design §20 / dev-docs/cluster.md)

Cluster mode is opt-in (**set `HYDRA_REDIS_URL`**) and needs no control-plane process. The
authoritative reference is **[`cluster.md`](cluster.md)** (node model, write/cache protocol,
failure modes, deployment, node-kill drill). This section is the runbook-level index.

### 13.1 Build

```bash
# exactly one DB backend feature; the image also bundles Redis + ClickHouse
cargo build --release --features server,cluster-redis,usage-clickhouse,db-postgres
# mysql variant: ... db-mysql
# single-node builds keep the same DB backend but may omit cluster-redis:
cargo build --release --features server,db-postgres
```

`environment/build.sh` wraps this (`DB_BACKEND=postgres|mysql`). There is no `arachne`
feature; a cluster build is just the Redis data-plane feature plus the chosen DB backend.

### 13.2 Minimal cluster (compose)

```bash
cd environment
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"         # required, >= 16 chars — on EVERY node
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # SAME on every node
docker compose -f docker-compose.cluster.yml up -d
curl -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://localhost:8081/api/v1/tenants
```

Three nodes (`hydra-a`/`b`/`c`) share **one Postgres**; Redis and ClickHouse are shared too.
There are no per-node config volumes, no raft ports, and no membership step. Scroll
`replicas`/the compose scale independently of the database. k3s/k8s manifests live in
`dev-docs/deployment.md` §4.

### 13.3 Cluster environment variables (quick map)

| Variable | Notes |
|---|---|
| `HYDRA_DB_URL` | **Required.** PostgreSQL/MySQL URL shared by every node. The scheme must match the compiled backend. |
| `HYDRA_REDIS_URL` / `HYDRA_REDIS_MODE` / `HYDRA_CONFIG_POLL_MS` | `HYDRA_REDIS_URL` is the cluster switch and also carries shared limits/breaker/invalidation + the config notification. `HYDRA_REDIS_MODE` accepts only `single` (validated whenever set). `HYDRA_CONFIG_POLL_MS` defaults to **5000** and is the revision-probe fallback: unset/garbage/`0` ⇒ 5000; tiny values hammer the DB, very large values effectively disable the backstop. |
| `HYDRA_NODE_ID` | Stable node identity for logs, fanout origin and breaker votes. Defaults to hostname, then random. |
| `HYDRA_ADMIN_TOKEN` | Required; every node serves its own admin API. |
| `HYDRA_ENCRYPTION_KEY` | Master key, identical fleet-wide. |
| `HYDRA_USAGE_SINK` | `clickhouse` (+ `HYDRA_CLICKHOUSE_URL`) or `none`. Required, no default. |

Other runtime knobs (unchanged by the cluster rewrite):

| Variable | Notes |
|---|---|
| `HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS` | TCP/TLS connect bound (default **10**); must be strictly below the first-byte bound. `0`/garbage falls back. |
| `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS` | Response-header bound (default 30); `0` falls back. |
| `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` | Max gap between streamed body chunks (default 120); `0` rejected. |
| `HYDRA_MAX_REQUEST_BODY_HARD` | Request-body hard cap in bytes (default 33554432 = 32 MiB); exceed ⇒ 413. |
| `HYDRA_REQUEST_BODY_TIMEOUT_SECS` | Total request-body read deadline (default 60); exceed ⇒ 408. |
| `HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN` | Per-peer failed admin credential budget (default 10); past it ⇒ 429 + `Retry-After`. Valid tokens are never locked out. |
| `HYDRA_SHUTDOWN_DRAIN_SECS` | In-flight drain after SIGTERM (default 20); size `terminationGracePeriodSeconds` from it (§13.5b). |

### 13.3b Retired variables

**Shared-DB rewrite retired these** (they are in `RETIRED_CLUSTER_ENV`; setting one logs a
startup ERROR naming it):

| Retired name | Why |
|---|---|
| `HYDRA_CLUSTER_PEERS` | No static member list; cluster = `HYDRA_REDIS_URL` |
| `HYDRA_CLUSTER_ID` | No raft cluster identity / data directory |
| `HYDRA_ARACHNE_LISTEN` | No raft transport |
| `HYDRA_ARACHNE_DATA_DIR` | No raft WAL/materialized state; the DB is authority |
| `HYDRA_ARACHNE_SNAPSHOT_THRESHOLD_MB` | No Arachne snapshots |

**Older retirements** (also read by nothing): `HYDRA_ROLE`, `HYDRA_EDGE`,
`HYDRA_CLUSTER_TOKEN`, `HYDRA_CONTROL_URL`, `HYDRA_PUBLIC_URL`, `HYDRA_CONTROL_POLL_MS`,
`HYDRA_LEADER_LEASE_MS`, `HYDRA_REGISTRY_STALE_GRACE_SECS`, `HYDRA_FAILOVER_GRACE_MS`,
`HYDRA_FORWARD_TIMEOUT_SECS`. Remove all of them from manifests; the startup diagnostic
names each one that is still present.

### 13.4 Node-kill drill

```bash
# every node serves the admin API; any survivor accepts writes (there is no writer to find)
for p in 8081 8082 8083; do echo -n "$p: "; \
  curl -s -o /dev/null -w '%{http_code}\n' \
    -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://localhost:$p/api/v1/health; done

docker compose -f docker-compose.cluster.yml kill hydra-a
# write through a survivor; its own config_version advances
curl -s -X POST -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{"id":"drill-provider","key":"drill","name":"Drill","endpoint":"https://example.invalid","weight":1}' \
  http://localhost:8082/api/v1/providers >/dev/null || true
curl -s -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" localhost:8082/api/v1/cluster/status

docker compose -f docker-compose.cluster.yml start hydra-a
# it reloads from the shared DB; no "rejoin"/"catch-up" step
```

**What to expect:** a killed node loses capacity, not write availability. Survivors keep
writing the shared DB; their caches converge through `hydra:config:changed` or the poll
fallback. Restarting the node reloads from the database. There is no leader election, no
quorum loss and no `503 config_not_published`.

### 13.5 Redis outage behavior

The data plane keeps serving from each node's in-process snapshot. A Redis outage costs the
shared data-plane state: rate limits fail open, breaker votes stop syncing, invalidation
propagation pauses (entries expire by TTL). **Config correctness is unaffected**: the
notification is best-effort and the poll fallback converges; writes still commit to the
shared database. See `dev-docs/cluster.md` §5 for the full matrix.

The connection pool uses `redis::reconnect_policy()` (retry forever, 1 s, jittered). Watch
`hydra_control_poll_total{result="rate_limit_error"}`: it counts fail-open decisions and
stops growing once Redis is back.

### 13.5b Shutdown drain vs `terminationGracePeriodSeconds`

`HYDRA_SHUTDOWN_DRAIN_SECS` (default **20**) is how long Pingora may spend draining
in-flight requests after `SIGTERM`; add Pingora's final ~5 s shutdown step and slack:

```
terminationGracePeriodSeconds >= HYDRA_SHUTDOWN_DRAIN_SECS (20) + 5 + slack (10) = 35
```

New connections are refused during the drain (data and admin), so gate readiness before
`SIGTERM` and expect a short metrics-scrape gap. `SIGTERM`, `SIGINT` and `SIGQUIT` flush the
usage sink; keep `terminationGracePeriodSeconds` above the drain or the buffered batch is
lost. There is no SQLite WAL to replay after shutdown any more; the shared DB is durable on
commit.

### 13.6 Node identity, database availability, MySQL notes

- `HYDRA_NODE_ID` is informational (logs / fanout origin / breaker votes). A plain
  Deployment's changing hostname is harmless now; set it explicitly only if you want stable
  log labels.
- `GET /api/v1/cluster/status` is local-node only: `node_id`, `role`, `config_version`,
  `db_reachable`, `redis_configured`, `redis_reachable` (currently `null`). There is no
  peer list or lease holder to inspect.
- **Database availability is the cluster availability boundary.** If the DB is down, all
  nodes can still serve their last snapshot but cannot commit writes. Restore DB service
  first; there is no "majority" to assemble.
- **MySQL specifics**: reserved-word columns were renamed (`provider_code`, `model_key`,
  `limit_window`); `delete_tenant` uses a scoped text-protocol delete to avoid MySQL 1615.
  Use `mysqldump --single-transaction` for backups. Greenfield migrations live in
  `migrations/mysql/`; PostgreSQL equivalents in `migrations/postgres/`.
- **Revision rewind** after a restore/manual SQL: a node whose local version is ahead of
  the restored `config_revision` skips the normal reload. Run
  `POST /api/v1/reload?force=1` on it.

### 13.6b Recovery from a full stop

| Situation | What to do |
|---|---|
| **Database and Redis up** | Start the nodes in any order; each loads the shared DB. No majority/claim window |
| **Redis down, database up** | Start anyway: data plane serves, shared limits degrade, config fanout degrades to polling. Restore Redis; no restart required |
| **Database down** | Nodes fail to start (migrations) or serve stale snapshots if already up. Restore the database, then restart/serve. This is the availability boundary |
| **Restored an older backup** | Use the matching binary, then `POST /api/v1/reload?force=1` on any node whose version is ahead; verify `config_version` converges |

### 13.7 Known limitations (as of this revision)

- **Database HA is the cluster HA.** There is no raft majority that keeps writes available
  when the DB is unreachable.
- **Every write does a full reload.** Acceptable at current scale; `changed=false` no-ops are
  cheap. The reload lock serializes the snapshot swap.
- **`HYDRA_CONFIG_POLL_MS` bounds** are operational: tiny intervals hammer the DB; huge ones
  effectively disable the backstop (§13.3).
- **No peer liveness view**: `/api/v1/cluster/status` reports this node only. A Redis notify
  failure is not a node-down signal.
- **MySQL portability subset**: booleans are `SMALLINT` 0/1 and `expires_at` stays RFC3339
  text (lexicographic compare); see `migrations/mysql/0001_init.sql`.
- Breaker `threshold` (5) and `probe_interval` (10s) remain compiled-in; the lever is
  `DELETE /api/v1/breaker/{id}`.
- `HYDRA_FAILOVER_GRACE_MS` was never wired and is retired (§13.3b). `HYDRA_BREAKER_QUORUM`
  uses an in-code default (`1`) **and is read**. `HYDRA_RATE_LIMIT_FAIL_MODE` **does not
  exist at all**: the Redis rate-limit path is hard-coded fail-open.
- Redis sentinel/cluster modes fail fast (single mode wired), and any other value of
  `HYDRA_REDIS_MODE` fails when set.
- The invalidations stream is trimmed by `MAXLEN` (10 000) every 30 s and bumps the
  generation only when a dropped entry had not been applied by every live node. Watch
  `hydra_invalidation_generation_bumps_total` / `hydra_invalidation_trimmed_total`.

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
