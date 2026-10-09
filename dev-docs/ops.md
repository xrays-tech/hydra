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
WAL). No external database, queue, or cache is required for v1 (single instance).
**Configuration is environment-only**: there is no config-file loader — no `toml`
dependency in any `Cargo.toml`, and `grep -rn 'hydra\.toml' crates/` is empty
(re-verified 2026-09-29). The `hydra.toml` schema in design §15.1 is a **target**,
and anything you put in such a file today is ignored.

```
/opt/hydra/
├── hydra                  # the release binary (self-contained: UI embedded)
└── data/
    ├── hydra.db           # SQLite (chmod 0600, §16.2)
    ├── hydra.db-wal
    └── hydra.db-shm
```

Build the release binary:

```bash
# `usage-clickhouse` is REQUIRED here: the compose file this section points at
# (`environment/docker-compose.yml`) sets `HYDRA_USAGE_SINK: clickhouse`, and the
# binary answers `BuildSinkError::ClickHouseFeatureDisabled` and exits 1 when the
# feature is absent. `server` alone does not include it (they are independent
# features), so `--features server` looked right and could never start.
cargo build --release --features server,usage-clickhouse
# → target/release/hydra
```
（`dev-docs/deployment.md` 的构建命令一直是含 `usage-clickhouse` 的；此处与它对齐。）

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
| `HYDRA_DB_URL` | `sqlite:hydra.db?mode=rwc` | SQLite path. Use `sqlite://./data/hydra.db?mode=rwc` in production. |
| `HYDRA_LISTEN` | `0.0.0.0:8080` | Proxy **plaintext** listener. Always bound — the listener topology is derived from configuration only, never from whether tenants have certs (see `dev-docs/bug-2026-09-16-tenant-cert-flips-listener-to-tls.md`). |
| `HYDRA_TLS_LISTEN` | *(unset)* | Optional proxy TLS listener, e.g. `0.0.0.0:8443`. **Setting this is what enables HTTPS** — per-tenant certificates are then selected by SNI. Unset with tenant certs present ⇒ the certs are NOT served (logged as an error + `hydra_listener_misconfig_total`); set but the address cannot be bound ⇒ plaintext keeps serving and the failure is logged + counted. Must differ from `HYDRA_LISTEN`. |
| `HYDRA_ADMIN_ADDR` | `127.0.0.1:8081` | Admin REST + UI + `/metrics` listener. **Bind loopback only** (design §13.3). |
| `HYDRA_USAGE_SINK` | ***(required — no default)*** | Where usage goes. Accepted values are the registered backends (`dev-docs/usage-backends.md` is the list's owner): `clickhouse` — the shared store every node writes to (needs `HYDRA_CLICKHOUSE_URL`, and a build with `--features usage-clickhouse`; the release scripts build it) — or `none`, which records **nothing** and says so at startup, counts every discarded record on `hydra_usage_records_dropped_total{reason="sink_disabled"}` and answers `503 usage_store_unavailable` on `GET /usage`. Unset ⇒ the process **refuses to start** (ADR-0002 D-1: "where does the billing data go" is a decision, not a guess). `sqlite` was retired on 2026-10-07 and is refused **by name**; it wrote usage into the node's own database, so a cluster's usage was scattered across nodes and the node answering `GET /usage` was usually not the one that recorded the request. |
| `HYDRA_CLICKHOUSE_URL` | *(unset)* | ClickHouse HTTP endpoint, e.g. `http://hydra-clickhouse:8123` (required when `HYDRA_USAGE_SINK=clickhouse`). **Credentials ARE supported**: use `http://user:pass@host:8123` (sent as HTTP Basic auth) or query params (`?user=&password=`); other query params like `?database=dogress` are passed through verbatim. **Do not put a path in this URL** — the sink always POSTs to `/`; a path is *trimmed* rather than used (measured 2026-09-30: before that fix `http://host:8123/clickhouse` failed at connect with `invalid port value`, because the path was still glued to the port when it was parsed). **What the sink actually puts on the wire** (measured 2026-09-30 against a mock ClickHouse, `integration/test_clickhouse_sink_wire.py`): `POST /?<your params>&query=INSERT%20INTO%20usage_record%20(…)%20SETTINGS%20…%20FORMAT%20JSONEachRow`, with the usage **row values in the request BODY** as one JSON object per line — **not** as `param_*` query parameters (that binding form belongs to the usage *reader*, `usage/backends/clickhouse/mod.rs` — moved there from `usage_query.rs` by ADR-0002 T1.3). Useful when diagnosing from a network capture: the SQL appears percent-encoded in the URL, the row values do not, and `client_api_key` in the body is **masked** (`sk*******-1`), never the tenant's real key. |
| `RUST_LOG` | `info` (set by the image + every compose file) | `tracing` env filter — `main.rs:172` uses `EnvFilter::from_default_env()`. **`HYDRA_LOG` is NOT read**: it was listed here as an alias until 2026-09-29 and has never existed in code (`grep -rn HYDRA_LOG crates/ scripts/ tools/ environment/` is empty). Measured 2026-09-29 on a live instance: started with **only** `HYDRA_LOG=debug` it logs **0 lines** (while `/health` and the API still answer 200), with `RUST_LOG=debug` it logs ~130 lines — so an operator reaching for the documented alias during an incident gets no extra logging. Also note `info` is a **deployment** default, not a binary default: with neither variable set the same instance logs **0 lines** (`from_default_env()` defaults to ERROR-level), and the shipped `Dockerfile`/compose files are what set `RUST_LOG=info`. |
| `HYDRA_TENANT_API` | `on` | Master switch for the tenant API on the data plane (`/tenant/…`). `off`/`0`/`false` ⇒ the prefix is not intercepted at all and the process behaves exactly as before the API existed. |
| `HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS` | `2000` | How long `auth/cache/invalidate` waits for the fleet to confirm before answering `202` with `lagging`. |
| `HYDRA_TENANT_API_RATE_LIMIT_PER_MIN` | `60` | Per-tenant cap on AUTHORISED requests (429 beyond it) — counts every authenticated request the node accepts for the tenant, including ones it then rejects with 4xx/5xx (only the 403 URL-tenant mismatch is exempt). The amplification budget: one tenant's credential must not be able to spend other tenants' availability. |
| `HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN` | `10` | Cap on FAILED authentications, per **source IP** and per **token digest** independently. |
| `HYDRA_TENANT_API_LOCKOUT_SECS` | `900` | Once a dimension exceeds its failure budget it is locked for this long. **The lockout is consulted only on the authentication-failure path**: a request with a valid token is never refused by it, so a locked-out guesser **can** distinguish `429` (wrong token) from `200` (right token) — the no-validity-oracle property is deliberately given up (marginal guessing value ≈ 0 at ≥16-char tokens, and the `403` URL-tenant-mismatch channel already exposes validity). |
| `HYDRA_TRUSTED_PROXIES` | *(unset)* | Comma-separated **IP or CIDR** allowlist of reverse proxies whose `X-Forwarded-For` is trusted (IPv4/IPv6; a bare IP is treated as /32 or /128). Unset/empty = trust nobody (use the socket peer IP — the conservative default). When the peer is trusted, the per-IP limiter reads **all** `X-Forwarded-For` header lines (in order) and keys on the **rightmost** address that is not itself a trusted proxy, falling back to the peer when there is no `X-Forwarded-For` / all entries are trusted proxies / any entry is invalid. **A malformed entry fails startup.** Misconfiguration risk: trusting a proxy that does not strip inbound `X-Forwarded-For` lets a client forge it and rotate its per-IP bucket, effectively disabling the per-IP dimension. |
| `HYDRA_TENANT_API_INVALIDATE_PER_MIN` | `10` | Per-tenant invalidation cap (429 beyond it). Each one fans out to every node and re-hits the tenant's `auth_url` from all of them. |
| `HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS` | `31` | E3 window ceiling. Not cosmetic: the ClickHouse table's key leads with `created_at`, so a wide window scans every tenant's rows in it. |
| `HYDRA_AUTH_ALLOW_TTL_MAX_SECS` | `300` | Ceiling on an **allow** entry's TTL, including one a tenant asked for via `expires_in`. Bounds how long a revoked key can keep working on a node that missed the invalidation. Fails startup on a non-positive value. |
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

#### Rotating the master key (`HYDRA_ENCRYPTION_KEY`) — supported procedure

**Do not simply replace the key.** Everything at rest is sealed under it
(provider api-keys and tenant certificate private keys), so with only a new key
configured the boot-time decrypt fails and the process refuses to start — and the
admin API that could re-enter those keys runs in that same process, so there is
no way back in. The supported path is a two-key window plus a one-shot re-seal:

```bash
# 1. Generate the new key and pick its version (a positive integer greater than
#    every version currently in use; never 0). List what is in use with:
#      # `HYDRA_DB_URL` is a URL, NOT a path: strip the scheme and the ?query first.
#      # (Verified 2026-09-29: handing the URL itself to a sqlite client fails with
#      # "unable to open database file" — this section used to do exactly that.)
#      DB_PATH="${HYDRA_DB_URL#sqlite://}"; DB_PATH="${DB_PATH%%\?*}"
#      sqlite3 "$DB_PATH" \
#        'SELECT key_version, COUNT(*) FROM provider_key GROUP BY key_version;'
#      sqlite3 "$DB_PATH" \
#        'SELECT cert_key_version, COUNT(*) FROM tenant WHERE cert_key_version IS NOT NULL GROUP BY 1;'
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

### 1.4 Backup & restore (added 2026-09-29 — this section did not exist)

Everything the node needs to come back — providers, **sealed** provider keys,
tenants, their certificates, limit roles, sub-tenants — lives in ONE SQLite file,
in **WAL mode** (`db.rs:55` `journal_mode(WAL)`). Two consequences for a backup and
one for a restore, all measured against a live node — and all three are now pinned by
`integration/test_backup_restore.py` (CI `integration` job + the local gate), so this
section is executed, not just documented:

```bash
# ❌ WRONG: copying the database file alone while the node runs.
cp /opt/hydra/data/hydra.db /backup/hydra-$(date +%F).db
#    Committed-but-not-yet-checkpointed transactions live in `hydra.db-wal`, so the
#    copy can be structurally EMPTY. Measured: the copy was 4096 bytes and every
#    table was unreadable, while the live DB held 1 provider + 1 provider key +
#    1 tenant + 2 associations (the 560 KB of recent writes sat in `-wal`).
#    RE-MEASURED 2026-09-30 (integration/test_backup_restore.py, 300-provider burst
#    with a 4.1 MB WAL): the copy was 221 184 bytes — a perfectly normal-LOOKING
#    file — and silently held 215 of 301 providers. Nothing about it looks wrong,
#    which makes this shape MORE dangerous than the empty-file one: a restore from
#    it boots fine and quietly loses the newest configuration.

# ✅ RIGHT: let SQLite produce a consistent snapshot of a LIVE database.
DB_PATH=/opt/hydra/data/hydra.db
sqlite3 "$DB_PATH" "VACUUM INTO '/backup/hydra-$(date +%F).db'"
#    Measured: while the node kept serving, the snapshot held every row
#    (provider=1 provider_key=1 tenant=1 tenant_provider=1 tenant_model=1), and a
#    SECOND node booted from that file served a proxied request with HTTP 200 —
#    i.e. both the configuration and the sealed provider key survived.
#    `VACUUM INTO` needs no downtime; stop the node first only if you prefer.
#    Re-measured 2026-09-30 with 301 providers: snapshot 237 568 bytes, row-for-row
#    equal to the live database, `/api/v1/health` 200 before and after, and the
#    restored node served 200.
```

**Restore**: stop the node, put the snapshot in place as `data/hydra.db` (delete any
stale `hydra.db-wal` / `hydra.db-shm` beside it), then start with **the same
`HYDRA_ENCRYPTION_KEY` / `HYDRA_ENCRYPTION_KEY_VERSION`** that was current when the
snapshot was taken.

> ⚠️ **Deleting the sibling `-wal` / `-shm` is not a tidiness step — without it the
> restore does not boot.** Measured 2026-09-30: put a `VACUUM INTO` snapshot in place
> while the previous database's `-wal`/`-shm` were still next to it, and
> `sqlite3` reports `database disk image is malformed` and the node exits with
> `ERROR hydra: fatal startup error error=error returned from database:
> (code: 11) database disk image is malformed` — the stale log describes a state
> that belongs to a DIFFERENT file. Delete both files (or restore into an empty
> directory) and the very same snapshot is readable and serves 200.
>
> ...and you WILL meet a leftover `-wal`: **every stop leaves one**, including a
> graceful `SIGTERM`. Measured: after `SIGTERM` + drain, `hydra.db-wal` and
> `hydra.db-shm` are still there, because the process ends in Pingora's
> `process::exit(0)` and the SQLite pool is never closed. That is harmless on its own
> — restarting on the SAME database replays the WAL (measured: a row written just
> before the stop was still readable afterwards) — it is only fatal when the
> database file underneath it is REPLACED, i.e. exactly during a restore.


> ⚠️ **The backup is useless without that key.** Provider keys and certificate
> private keys are sealed at rest, and a node pointed at a restored database under a
> different master key **refuses to serve** (measured: `fatal startup error … error
> occurred while decoding …`; pinned by case D of `integration/test_backup_restore.py`,
> which reads the exit code and the log line). Keep the key separate from the database
> and from the host: a backup strategy that stores both in the same bucket protects
> against neither theft nor loss.

### Destructive upgrade: the local usage table is DROPPED (migration 0013)

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
§"Destructive upgrade" below for what to do before upgrading. A cluster
replica rebuilds from the leader rather than from a backup (`db::restore_config`,
design §11); `data/` permissions are §1.3.

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
- **DB schema**: SQLite migrations run on boot. A forward-only migration is
  safe during upgrade (the old process keeps its connection; the new process
  opens a fresh pool and runs migrations). A backward-incompatible migration
  blocks rollback — keep the previous binary until you're confident.

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
  -d '{"id":"t1","name":"Acme","domain":"acme.com","auth_url":"https://auth.acme.com/v","cert_file":"data/certs/acme.crt","cert_key":"data/certs/acme.key","enabled":true,"created_at":"x","updated_at":""}'
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
>   (`db.rs::seal_limit_key`, exactly like a provider api-key), the Arachne config tree carries that
>   sealed value (`cluster/arachne_entities.rs::sealed_limit_role`), a materializing node seals it
>   again as it writes its own database (`db/restore.rs`), and a row written BEFORE that decision is
>   re-sealed the first time a loader reads it (`db::seal_legacy_limit_keys` — no operator step; the
>   node logs `sealed N legacy plaintext 'limit_role.matching_key' value(s)` once). Master-key
>   rotation covers the column too: `hydra --reseal` reports `limit_keys=N`.
> * **what sealing does NOT cover** — the admin plane: `GET /api/v1/limit-roles` returns `matching_key` MASKED
>   (digest form as-is, everything else via `mask_key` — P3-4, 2026-10-09), so the node never echoes a
>   recoverable client key; only something holding the master key can open the sealed envelope. Sealing
>   protects databases, backups and replicas from a filesystem-level reader; it does not make the
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
> `matching_model` and `matching_tenant` are **exact** matches (not prefixes like the sub-tenant and
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
(202 — published, not all confirmed; `lagging` **names the nodes that did not confirm**, or is **empty** when the node could not enumerate the live fleet — see the three-state note below), **`single_node`**
(200 — this node is the whole data plane) or **`unavailable`** (503 — the channel
exists but did not answer; **the fleet was NOT told**).

> **This admin endpoint's three states — `202` is NOT "failure".** `DELETE
> /api/v1/auth/cache` answers `200` / `202` / `503`, and only `200` means "done":
>
> | HTTP | `fleet.state` | when | what it means |
> |---|---|---|---|
> | `200` | `applied` | every live node confirmed | done |
> | `200` | `single_node` | not a cluster node (its own local clear IS the whole answer) | done |
> | `202` | `pending` | published, not confirmed everywhere — **including the deterministic empty-fleet case below** | **in flight, not a failure**; retry (idempotent) or check `lagging` |
> | `503` | `unavailable` | no invalidation stream **or** publish failed, or the watermark read failed | the fleet was **not** told — retry / escalate |
>
> **The deterministic `202`: an empty live-fleet view.** The handler always *waits* on
> the convergence barrier — the admin endpoint has **no** `wait=` parameter (that
> switch exists only on the tenant plane), so the handler passes
> `Some(converge_timeout)` unconditionally (the `let report = { … }` block in the
> `DELETE /api/v1/auth/cache` handler in `crates/hydra-server/src/admin/handlers.rs`).
> With `nodes_total == 0` — i.e. the live-node closure returned an empty set,
> which is the normal state during the boot window before the registry refresh ticker
> first populates it, and after registry rows expire while Redis is still reachable —
> `await_applied` takes its empty-live-set arm and returns `Pending { nodes_applied: 0,
> nodes_total: 0, lagging: [] }` (`crates/hydra-server/src/cluster/events.rs`, the
> `if live_nodes.is_empty()` arm), which maps to **`202`**. It is deliberately *not*
> `Applied`: nothing was checked, so "converged" must not be asserted (fail-closed —
> this is the `Applied(200) → Pending(202)` flip from commit `c3eaa6f`). So on a
> healthy single-node-ish deployment you can see a **steady `202` with `nodes_total:
> 0`**: that is the honest "nobody was checked" answer, **not** an error and **not** a
> sign the invalidation failed. Watch `hydra_tenant_api_invalidate_pending_total` /
> the `fleet.lagging` list, not the status code alone. The tenant-plane equivalent
> (`POST /tenant/{tid}/api/v1/auth/cache/invalidate`) documents the same three states
> in `dev-docs/tenant-api-integration.md` §5.2.
>
> `event_id` is **`null`** for `single_node` and for the *publish-failure* variant of
> `unavailable` (nothing was enqueued), and `waited_ms` is hard-coded `0` on the
> `single_node` path (`FleetReport::single_node()` never calls `.measured()`), so
> neither field is a reliable convergence signal on its own.

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
> - **Catch-all does NOT make the dimension forgeable — it makes every client SHARE one bucket** (corrected 2026-09-29 after measuring; the old wording here said the opposite). A `0.0.0.0/0` or `::/0` entry trusts *every* candidate, and the resolver then falls back to the peer address (`resolve_client_ip`; its unit test is literally named `an_all_trusted_or_absent_xff_falls_back_to_the_peer`). So the per-IP dimension keys on the **peer** — behind an LB that means the LB's address for everyone. Measured with a catch-all: 14 requests carrying 14 different XFF values (and 14 different bad tokens, so the per-token dimension could not be what tripped) still hit `429` at the 11th, and a **fresh XFF + fresh token** was refused as well ⇒ every client shares the peer's bucket, and a handful of bad tokens locks the tenant API for **all** of them for `HYDRA_TENANT_API_LOCKOUT_SECS` (900 s by default). The node **warns loudly at startup** but does not refuse to start.
> - **The forgeable case is the narrow allowlist + a non-stripping LB** (the first bullet above): with `HYDRA_TRUSTED_PROXIES=127.0.0.1` — a *trusted* peer whose LB passes an attacker-supplied header through — 14 requests with 14 different `X-Forwarded-For` values produced **no** `429` at all, i.e. the caller rotated its own bucket. Skipping/overriding inbound XFF at the LB is what makes the allowlist safe.
> - **Observability.** Watch `hydra_tenant_api_auth_failures_total{reason}`: many **distinct IPs accelerating in unison** is the signature of a misconfigured (non-stripping) trusted proxy, not a single-source guesser.

`503 not_ready` means this node has no configuration snapshot yet — retry.
`429 rate_limited` carries `Retry-After`; invalidations are capped per tenant per
minute (default 10) because each one fans out to every node and re-hits the
tenant's `auth_url` from all of them.

> **Known limitation (no fix promised):** when a tenant-API request body exceeds the 1 MiB cap, the node replies `413` and closes the connection **without draining the rest of the body** — a client still uploading a large body may observe a connection reset before it reads the `413` body.
>
> **Measured 2026-09-30** (`integration/test_body_cap_drain.py`), which sharpens the wording in both directions: the node answered after consuming **1 081 344 of the 2 097 147 bytes** it was sent — i.e. it stops reading right at the cap and leaves ~1 MiB on the wire, which is the fact behind "does not drain" — while a client that **stops pushing as soon as the response becomes readable reads its `413 payload_too_large` normally** (`write_error=None`). The reset the note warns about therefore needs a client that **keeps pushing** after the response; the DATA plane behaves differently: it drains the rest of the upload first (**all 131 125 bytes consumed**, `413 request_body_too_large` always readable). If you are writing a client, stop writing when the connection becomes readable.

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
   was completely invisible. The series is emitted by the cluster events consumer,
   so it appears in **cluster mode only**: on a single-node instance there is no
   consumer, the gauge vector has no children, and the family is absent from
   `/metrics` (verified 2026-09-29 — an alert rule on a series that never appears
   is a silent monitoring gap, so it must be deployed with the cluster topology).

Measured 2026-09-29 (single node, a switchable auth service, tenant asking for
`expires_in=3600`):

| Case | Measured | Note |
|------|----------|------|
| allow cached, tenant asks 3600 s, `HYDRA_AUTH_ALLOW_TTL_MAX_SECS=2` | the gateway kept allowing for **2.07 s**, then refused — and re-asked the auth service exactly once | the knob bounds what a tenant may ask for |
| same flow with `…=3600` | still allowing 4 s later | control: the cap is what bounded the row above |
| explicit invalidation (`DELETE /api/v1/auth/cache`, `tenant_id`) with the allow cached for an hour | **refused on the very next request**; body `{"invalidated":1,…,"fleet":{"state":"single_node",…}}` | invalidation is the lever, not the TTL |
| a cached **DENY**, then the auth service starts allowing again | the node kept refusing for **30.41 s** (2 auth calls in total) | i.e. the fixed `deny_ttl` below — "denies are already short" is exactly 30 s, and this knob does not touch it |

So for an incident: **an explicit invalidation is immediate; without one, an allow
lives at most `min(tenant expires_in, cap)` and a deny at most the fixed `deny_ttl`.

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

> **v3 schema migration — sub-tenant usage attribution (2026-09-18).** `usage_record`
> gained a nullable `sub_tenant_id` column (derived at record time from the RAW api-key
> prefix). **Fresh** ClickHouse instances get it from `environment/clickhouse/init.sql`;
> an **already-initialised** instance needs a one-off
> `ALTER TABLE usage_record ADD COLUMN IF NOT EXISTS sub_tenant_id Nullable(String)`
> (**no backfill** — pre-existing rows stay NULL; `CREATE TABLE IF NOT EXISTS` does not
> add columns to an existing table). SQLite applies migration `0011` automatically.
> **Run it before the first v3 binary writes to that instance**: the ClickHouse insert
> names the column, and ClickHouse rejects an insert naming a column that does not exist
> (`NO_SUCH_COLUMN_IN_TABLE`, verified against the bundled instance on a throwaway table),
> so usage flushes for that instance fail until the `ALTER` is applied.

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

**Availability**: admin CRUD is available on **every cluster node** — each one serves its own admin
API and applies its own writes (ADR-0001 D-2/D-6), so point the admin UI or a script at any node. The
old rule ("only leader/standby; an edge 404s every admin path") went with the `edge` role.

### 5.5a Tenant self-service write path (v2, A′ — **as of 2026-10-05: applied by the receiving node**)

A tenant can CRUD its **own** sub-tenants and routes on the **data plane**, without an operator.

**Four data-plane endpoints** (tenant-token gated, on the data-plane port; the tenant sees a normal
response):

| endpoint | what it does |
| --- | --- |
| `PUT    /tenant/{tid}/api/v1/sub-tenants/{name}` | upsert by `(tenant_id, name)`; body `{key_prefix?, enabled}` |
| `DELETE /tenant/{tid}/api/v1/sub-tenants/{id}` | delete by immutable id (idempotent) |
| `PUT    /tenant/{tid}/api/v1/sub-tenant-routes` | upsert by `(sub_tenant_id, model_key)`; body `{sub_tenant_id, model_key?, provider_id, enabled}` |
| `DELETE /tenant/{tid}/api/v1/sub-tenant-routes/{id}` | delete by immutable id (idempotent) |

> **The four internal routes are GONE** (`/api/v1/internal/tenant-config/*`), together with the
> `x-hydra-tenant-token` header and the cluster-token-gated leader-only write face (ADR-0001 D-6,
> 乙-full). A request to that path is now an ordinary 404.

**What happens on a write**: the node that received the request authenticates the tenant (the same
token gate as the read endpoints), then:

1. **authorization binding** — the write target must be the authenticated tenant's own resources
   (`body.tenant_id != T` → `403` by pure string comparison before any lookup; a route whose
   `sub_tenant_id` is missing or foreign → `404`, deliberately indistinguishable);
2. **write** — the shared transactional write core (`admin/sub_tenant_write.rs`);
3. **reload + publish** — the snapshot is reloaded and the resulting config is published to the
   control plane (Arachne). The raft library forwards the head write to the leader, so the write
   needs this node to be able to COMMIT, not to be the leader;
4. **audit** — one structured record per write (below).

**Why there is no forwarding any more**: under raft the write does not need a leader to LAND, only
to be COMMITTED, and the identity gate was always at the entry node. The old model relayed the whole
request to the lease holder's internal endpoint with the tenant's bearer in a dedicated header; both
the endpoint family and the header are retired, and the tenant credential no longer travels.

**Two properties that were given up deliberately** (recorded, not hidden):

* **No freshness gate.** The old leader re-authenticated the tenant against ITS OWN snapshot (the
  newest one) before executing. Now the entry node validates against the snapshot it holds, which
  can be marginally behind the head. The write is still validated against the LIVE database inside
  the transaction (that is where quota, prefix-overlap and existence checks run), so the exposure is
  limited to "a tenant whose token was revoked microseconds ago may still be accepted once" — the
  same window the data-plane read gate already has, bounded by the snapshot poll. A freshness gate
  was considered and rejected (D-6).
* **A compromised entry node can impersonate any tenant.** Note this is NOT a new capability: any
  cluster node holds `HYDRA_ENCRYPTION_KEY` and a full config snapshot (it can decrypt provider keys
  and certificate private keys), so compromising ONE node was already a fleet-wide compromise. What
  changes is the shape: it is now "any tenant" rather than "tenant A becomes tenant B".

**Write-rate accounting (quantified)**: the per-tenant config-write throttle that used to live on
the leader's internal endpoint is gone with it. Writes are now bounded by the general tenant-API
per-tenant request budget, which is **per node** — so the cluster-wide ceiling for sub-tenant writes
is **N x single-window** for N nodes, not one window. That is a real relaxation of the anti-DoS
bound, accepted with the D-6 ruling (it was never a durability guarantee: the old window was
process-local and reset on failover anyway).

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

### 5.x When the tenant's `auth_url` cannot be reached (measured 2026-09-30)

`integration/test_auth_hop_failmode.py` runs this hop against a black-holed route (TEST-NET-1, so
nothing on the network is contacted) and against a refused port:

| Situation | What the tenant sees | Cost |
|---|---|---|
| `auth_url` unreachable (SYN dropped: dead route, firewalled peer) | `503 auth_upstream_unavailable` (`error.type: auth_error`) | **~2 s per request** — the documented 2000 ms round-trip timeout — and the provider is **never** called |
| `auth_url` refusing (nothing listening) | the same `503` | **~1 ms** |
| the auth service answers again | the **very next** request is served | — |
| auth service answers 401/403 | `401 denied` | cached, with the deny TTL (30 s) |
| auth service answers 5xx / unparseable 2xx | `503 auth_upstream_unavailable` | **never cached** |

Two operational consequences. **A dead auth route costs its timeout on EVERY request** (the outage
is deliberately not cached — measured: three requests in a row each paid 2 s; caching it would be
worse, because the tenant would stay blocked for the deny TTL *after* the service recovered — that
regression was planted and it turned the next requests into instant `401`s, including after
recovery). And **attribution is available**: `hydra_auth_upstream_error_total{tenant}` plus
`hydra_auth_decisions_total{tenant,verdict="denied",source="miss"}`.

> **`fail_mode` is not selectable (recorded, not fixed).** `design.md` §11.4 presents
> `[auth] fail_mode` as configuration and `http.rs` implements `FailMode::Open`
> ("availability-first": serve without a verdict when the auth service is down) — but `main.rs`
> takes `AuthConfig { ..AuthConfig::default() }`, whose default is `Closed`, and **no environment
> variable selects the open mode** (measured: `HYDRA_AUTH_FAIL_MODE=open`, `HYDRA_FAIL_MODE=open`
> and `HYDRA_AUTH_FAILMODE=open` all still answer `503`). Turning it on is a security decision
> (unauthenticated traffic during an auth outage), so it is recorded here rather than wired.

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
| `HYDRA_REQUEST_BODY_TIMEOUT_SECS` | 60 s | **408 Request Timeout** + 关闭连接（不排空——这个客户端本来就不在发送）。这是**整个 body** 的总期限，不是空闲期限：pingora 对 HTTP/1 的 body 读取只有 per-read 期限（每个字节都会重置它），HTTP/2 则完全没有读超时，所以只有总期限挡得住"发一半就不发了"的客户端。**可达性（2026-09-29 实测修正）**：body 读取发生在**租户解析与认证跳之后**（`proxy.rs`：`resolve_tenant` → 认证 → 读全 body），所以这个洞**不是**任意匿名客户端都能触发 —— 认证跳拒绝的请求会在大体上传之前就被答复（实测：认证上游不可达时，33 MiB 的客户端拿到 `503 auth_upstream_unavailable` 且 `size_upload=0`）。但只要该租户的 `auth_url` 对**任意** key 都放行（开发/宽松鉴权很常见，也可能是有意设计），任何知道租户域名的调用者都能走到这一步；且认证缓存会给**已放行过**的 key 续命（TLL 内即使 auth 服务已挂，仍能到达读 body 阶段）。因此这条仍是**跨租户可用性**风险，只是前提写清楚：它成立的条件是"该租户的鉴权服务放行这次请求"。默认值按 `max_request_body_hard`（32 MiB）标定：60 s ≈ 0.5 MB/s 传满上限，慢链路请显式调大。`0` 被拒绝（那会 408 掉所有带 body 的请求）。 |
| body 读取出错 | — | **400 `request_body_read_error`** + 关闭。读取错误曾被当作"body 正常结束"，把**截断的** body 当完整请求转发给上游（静默损坏路径）；现在 fail-closed。 |

**Trade-off**（terminate-mode）：~~更大的软上限意味着更多请求可以安全故障转移（有利于可用性）~~ **不再适用**——全 body 已缓存，所有候选都能 O(1) 重放。内存占用 = 并发请求数 × 平均 body 大小（500 并发 × 2MB avg ≈ 1GB）。如需降低内存峰值，调低 **`HYDRA_MAX_REQUEST_BODY_HARD`**（字节；超过即 413——设置它需要重启进程）。

> ~~H2 paths are truly zero-copy on the forward leg; H1 paths incur one kernel copy per chunk (Pingora core limitation, design §8.5).~~ **（已废弃）** Terminate-mode 放弃 kernel-level 零拷贝（body 经 userspace buffer 传给 reqwest），但保留"零 JSON 往返"（body 字节未被 serde 处理）。详见 `dev-docs/design-change-terminate-mode.md` §5。

---

## 9. Observability (design §17, implemented W5)

- **`/metrics`** (self-hosted, no sidecar): Prometheus exposition on the admin port, **gated by the
  admin bearer token — one rule, every node**. It used to depend on the node's ROLE (an `edge` served
  it token-free), so the exposure depended on how a node was configured rather than on one rule; the
  role is retired (ADR-0001 D-2). The series carry `tenant`/`provider`/`model` labels, i.e. customer
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
  `hydra_tokens_total`, `hydra_auth_decisions_total`, `hydra_auth_cache_size`,
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
| ~~Registry rows piling up~~ / ~~Registry reaping churn~~ | **RETIRED — do not write these rules** | They were `hydra_registry_nodes{state="dead"} > 5` and `increase(hydra_registry_reaped_total[1h]) > 20`. The node registry and its reaper are deleted (ADR-0001 T4.1: membership is the static member list, there is no heartbeat row to pile up and no reaping to churn), so **neither series is registered any more** and both rules were silently dead. What replaces them is the pair below: "is anyone the writer" and "is publishing getting through" — a registry-shaped question about dead rows has no meaning when there are no rows |
| **No node is the raft writer** | `sum(hydra_arachne_this_node_leader) == 0` | Nobody answers the raft write probe, so no config change can be committed anywhere: management writes fail closed with `503 config_not_published` and the data plane keeps serving the **already materialized** config on every node. Sustained means the cluster has lost its majority (< ⌈n/2⌉+1 members reachable) — **the fix is to get a majority up**, not to restart the node answering 503. **The series exists only on a cluster node** (`IntGaugeVec` labelled by `node`): a single-node deployment has no raft node, so this rule is silent there instead of firing forever on a 0 that means "not applicable" (ADR-0001 T4.1 deleted the registry whose rows this replaces) |
| Publishing a config change is failing | `increase(hydra_arachne_publish_total{result=~"quorum_unavailable|error"}[10m]) > 0` | A node tried to commit a config tree and the control plane refused it. `quorum_unavailable` = no majority (same outage as the row above, seen from the write side); `error` = a transport failure, a malformed tree or a bug — the node still holds the change **in its own SQLite** and keeps serving it, but no other node will ever see it (`StoreError::NotPublished`). Manage writes answered 503 while this happens; a later successful write re-publishes whatever the config is then |
| A config change was REFUSED (capacity) | `increase(hydra_arachne_publish_total{result="refused"}[10m]) > 0` | Encoding rejected the config **before a byte was sent**: a value over the library's 1 MiB cap, an id that cannot be keyed, or a failed seal. Retrying cannot help — this is a configuration change (shrink the entity, remove the offending id). It is the honest capacity alarm ADR-0001 risk R2 asks for; `hydra_arachne_config_bytes` shows the growth that leads to it, and no byte threshold is invented here because the limit is per VALUE, not on the tree |
| Leadership is flapping | `increase(hydra_arachne_leader_flips_total[15m]) > 5` | The answer to "is this node the writer" changed more than five times in 15 minutes, on some node. A sampled gauge cannot show this (by the time a scrape lands the answer is true again), which is why the flips are counted separately. Flapping means an unstable quorum (a node restarting in a loop, a saturated link, a CPU-starved leader) and it moves the commit point under every writer |
| Config snapshot stale | `hydra_config_snapshot_stale == 1` | A reload failed (post-write **or** the explicit `POST /api/v1/reload`); the in-memory snapshot is behind the DB. The state is nastier than it looks, and it was measured 2026-09-30 (`integration/test_snapshot_stale.py`) by putting a provider row the LOADER rejects directly into SQLite (the admin write boundary refuses such a row, so a hand-edited database or a restore is how this happens): the node **keeps serving** on the old snapshot, `POST /api/v1/reload` answers **400 `reload_failed`** ("old snapshot retained"), the gauge goes to 1, and — the trap the code documents in capitals — **every later admin write still answers 2xx while having no runtime effect**: a `PUT` that disables a tenant returned 200 and the tenant kept being served until a reload succeeded. Each tenant view carries `snapshot_stale: true` while this holds. **Recovery:** remove/fix the offending row and reload — the gauge returns to 0 and the pending write takes effect (measured: the disabled tenant then answered `403 tenant_disabled`). Until 2026-09-30 the explicit endpoint recorded **nothing**: a failing `/reload` left the gauge at 0 while a later successful `/reload` left an earlier 1 in place, so this alert could neither fire on the failure nor clear on the documented recovery |
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
| ~~Replication stalled (upgrade window)~~ | **RETIRED — do not write this rule** | It was `changes(hydra_control_snapshot_version[10m]) == 0 and hydra_control_poll_total{result="ok"} > 0`, and BOTH terms are dead: `hydra_control_snapshot_version` lost its recorder with the polling client (ADR-0001 T4.1; the gauge was still registered and exported as 0, so the rule looked fine) and `result="ok"` is a label value nothing has ever recorded — the only values emitted are `rate_limit_error` and `invalidation_trim_error`. A rule on it could never fire. The facts it was trying to express are covered by the two rows above |
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
  failover is available in cluster mode (the member list replaced the retired role selector) — no longer a v2 backlog
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

Cluster mode is opt-in (**set the member list**) and self-sustaining: the control plane is embedded
(Arachne/raft, no process to deploy) and **Redis is the only external service** the cluster needs
besides the usage store. Every node is identical — data plane, admin API, local SQLite and a raft
membership. The authoritative reference is **[`cluster.md`](cluster.md)** (node model, env table,
key space, deploy manifests, failover drill, member-change SOP, measured acceptance records); this
section is the runbook-level index.

### 13.1 Build

```bash
cargo build --release --features server,cluster-redis,arachne,usage-clickhouse
# single-node builds stay feature-free: cargo build --release --features server
```

**`arachne` is not optional for a cluster build.** Without it a node with `HYDRA_CLUSTER_PEERS` set
**refuses to start** ("this build has no control plane"), so a binary built from a narrower recipe
cannot join a cluster at all — measured 2026-10-05, and the reason `environment/build.sh` bundles all
four features.

### 13.2 Minimal cluster (compose)

```bash
cd environment
export HYDRA_ADMIN_TOKEN="$(openssl rand -hex 32)"         # required, >= 16 chars — on EVERY node
export HYDRA_ENCRYPTION_KEY="$(openssl rand 32 | base64)"   # SAME on every node
docker compose -f docker-compose.cluster.yml up -d
curl -H "Authorization: Bearer $HYDRA_ADMIN_TOKEN" http://localhost:8081/api/v1/tenants
```

Three members (`hydra-a`/`b`/`c`), identical except for identity, ports and volume. There is **no
edge service to scale**: membership is fixed and changing it is a raft membership change
(`cluster.md` §6.3). k3s / k8s manifests — including the `podManagementPolicy: Parallel` a
StatefulSet needs — and bare-metal systemd live in `dev-docs/cluster.md` §5.

### 13.3 Cluster environment variables (quick map)

| Variable | Notes |
|---|---|
> **The role selector is RETIRED (ADR-0001)**: the cluster decision is now "is `HYDRA_CLUSTER_PEERS` set" — the member list IS the decision, so there is no role variable to mistype and no silent fallback. Nothing in the product reads it any more, so the row was DELETED from this table (`check_documented_env.cjs` treats a config table as a promise: wire it or move it out). The rest of this section still describes the Redis-lease world; following ADR-0001 is scheduled in plan T4.3.

| `HYDRA_CLUSTER_PEERS` | **required in cluster mode**: the static member list, `id=host:port` per member, this node included. Its presence IS the cluster decision. The member ORDER is immutable: Arachne derives each member's numeric raft id from its position in the list |
| `HYDRA_NODE_ID` | **required in cluster mode**: this node's identity, and it must appear in the member list. No `HOSTNAME`/random fallback on purpose — a duplicate id means two nodes share one raft identity |
| `HYDRA_ARACHNE_LISTEN` | **required in cluster mode**: where this node's raft transport binds. It must equal **this node's own entry** in the member list, or the node refuses to start (`ListenMismatch`). Its port is unrelated to the admin port — the old "must equal the admin port" rule existed only so the leader hint was directly usable as an admin endpoint, and nothing uses it that way any more (the hint is display-only) |
| `HYDRA_CLUSTER_ID` | optional: names the cluster so a node refuses to adopt an Arachne data directory that belongs to a different one. Defaults to a hash of the MEMBER LIST (ADR-0001: the list is the cluster identity; the order matters) |
| `HYDRA_REDIS_URL` / `HYDRA_REDIS_MODE` | backbone; `single` wired — `sentinel`/`cluster` **and any unrecognised value** fail fast at startup (a typo must not silently mean `single`). **On a cluster-role node only**: the mode is read inside `if role.is_cluster()` (`main.rs`), so with the member list unset the value is not validated at all, and the only line that can mention the variable is the "cluster wiring is configured but …" ERROR (which never quotes the value). Pinned by `integration/test_startup_knobs.py` K1/K2 **and K12** |
| `HYDRA_ADMIN_TOKEN` | required in cluster mode: every node serves its own admin API, so it gates EACH node (not a relaying standby — that layer is retired) |
| `HYDRA_ENCRYPTION_KEY` | master key, identical fleet-wide |
| `HYDRA_USAGE_SINK=clickhouse` | mandatory in cluster mode (+ `HYDRA_CLICKHOUSE_URL`) |
| `HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS` | bound on **establishing** the TCP/TLS connection to a provider (default **10**); `0`/garbage falls back to the default. **Must be strictly below `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS`** — the node refuses to start otherwise, because the first-byte bound wraps the whole send (connect included) and would always fire first. What it buys (measured 2026-09-30 against a black-holed route, `integration/test_upstream_connect_bound.py`): without it a provider whose SYN goes nowhere burned the whole first-byte bound and was classified as a *post-send* failure, so the request returned `502 upstream_transport_error` **instead of failing over** to a healthy provider (`codes=[502,200,502,200,502,200]`, `retries=0`); with it the attempt fails in ≤10s as a connect error and the request **fails over** (`codes=[200×6]`, `retries=4`, and `hydra_upstream_first_byte_timeout_total` stays 0 for that provider). A healthy provider on a normal RTT establishes in milliseconds, so this bound only ever fires on a dead route. **What a dead route costs, measured 2026-09-30** (`integration/test_dead_route_cost.py`, shipped defaults 10s/30s, dead route = a dropped SYN): every affected request pays ~**10s** and is then served by a healthy peer (`10.0s, 0.0s, 10.0s, …` — one penalty per time the dead provider is chosen), the provider is taken out of the rotation after exactly **5** such failures (`hydra_candidate_skipped_total{reason="breaker_dead"}` starts at 1 per skipped request), and the penalties then **stop** (all later requests < 1s). Worst case for a two-provider SWRR rotation: ~5 × 10s of user-visible latency spread over the first ~9 requests. `DELETE /api/v1/breaker/{id}` clears the dead-set, so resetting **without fixing the route** buys those 10s penalties again — fix the route first |
| `HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS` | upstream time-to-first-byte bound per attempt (default 30); `0` is rejected (it falls back to the default rather than meaning 'instant'). Covers the response HEADERS only — a connect that never completes is bounded by `HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS` above, and the response BODY by `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS`. See the alert row in §9.1 |
| `HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN` | per-PEER budget for FAILED credential attempts on the admin port — **shared by BOTH gates** (the `admin` token and the internal `cluster` token) (default 10; `0`/garbage falls back). Past it the peer gets `429 too_many_failed_attempts` + `Retry-After` for the rest of the minute; a VALID token is always accepted, so this cannot lock an operator out. Watch `hydra_admin_auth_failures_total{result=~".+_throttled"}` — the label is `<gate>_denied` \| `<gate>_throttled` (gate = `admin` \| `cluster`), so a rule written against the old bare `denied`/`throttled` values would never match |
| `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` | max gap between two body chunks of one streamed upstream response (default 120); `0` is rejected. Replaced the removed 300s total exchange timeout — see §9.1 and §10.2 |
| `HYDRA_MAX_REQUEST_BODY_HARD` | hard cap on ONE downstream request body, in **bytes** (default 33554432 = 32 MiB); `0`/garbage falls back to the default. Exceeding it is `413 request_body_too_large` + close. Lower it to cut peak memory (memory ≈ concurrency × average body); raise it for bigger payloads. The **admin** API and the tenant API have their own 1 MiB caps, which are compile-time constants and NOT tunable (`MAX_ADMIN_BODY_BYTES`, `tenant_api/mod.rs` `MAX_BODY`) — both answer `413 request_body_too_large` / `413 payload_too_large` |
| `HYDRA_REQUEST_BODY_TIMEOUT_SECS` | TOTAL deadline for reading one downstream request body (default 60); `0` is rejected. Exceeding it is `408 request_body_timeout` — see the body-cap table in §6.7 |
| `HYDRA_SHUTDOWN_DRAIN_SECS` | seconds Pingora drains in-flight requests after SIGTERM (default 20); size `terminationGracePeriodSeconds` from it (see §13.5b) |

### 13.3b Variables the Arachne control plane RETIRED

These are **read by nothing** and are listed in the code's `RETIRED_CLUSTER_ENV`; a deployment that
still sets one gets a startup ERROR naming it, so nobody believes it still does something. Remove
them from the manifests:

`HYDRA_ROLE`, `HYDRA_EDGE`, `HYDRA_CLUSTER_TOKEN`, `HYDRA_CONTROL_URL`, `HYDRA_PUBLIC_URL`,
`HYDRA_CONTROL_POLL_MS`, `HYDRA_LEADER_LEASE_MS`, `HYDRA_REGISTRY_STALE_GRACE_SECS`,
`HYDRA_FAILOVER_GRACE_MS`, `HYDRA_FORWARD_TIMEOUT_SECS`.

`HYDRA_CLUSTER_TOKEN` joined them on **2026-10-05** and it is the odd one out: it was not a knob but
a **boot requirement**, and it is the only name here that a deployment is likely to still carry (it
used to be mandatory). It guarded the `/api/v1/internal/*` family — whose two members are retired —
so every cluster had to generate, distribute and rotate a secret that guarded nothing. With it gone,
`HYDRA_ADMIN_TOKEN` is the only token a cluster deployment needs.

What replaced them: membership is `HYDRA_CLUSTER_PEERS` (the list IS the cluster), leadership is
raft's (there is no lease to set a TTL for), config reaches every node by materializing the tree
(there is no control URL to point at and no poll interval to tune), and admin writes are applied by
the node that received them (there is no forward timeout).

### 13.4 Failover drill

> `/healthz/leader` is the **only token-free route** (an LB must be able to route to the writer
> without a secret). `/healthz` and `/readyz` were the `edge` role's token-free probes and are
> **deleted with it** — measured 2026-10-05: both answer **404** on the admin port and on the data
> port of every node. A probe uses `/api/v1/health` with the admin token.

```bash
# who is the writer? exactly one node answers 200
for p in 8081 8082 8083; do echo -n "$p: "; \
  curl -s -o /dev/null -w '%{http_code}\n' localhost:$p/healthz/leader; done
docker compose -f docker-compose.cluster.yml kill hydra-a     # the one that answered 200
for p in 8082 8083; do echo -n "$p: "; \
  curl -s -o /dev/null -w '%{http_code}\n' localhost:$p/healthz/leader; done   # 200 on one, ~1–2 s
docker compose -f docker-compose.cluster.yml start hydra-a     # rejoins as a member
```

**Measured 2026-10-05** (three real processes, `kill -9`): leader elected in **1.08 s / 1.63 s**
(two runs), never two nodes answering 200 during the handover, and the **data plane served
1200/1200 requests at 20 rps for 60 s across the kill**
(`integration/test_data_plane_failover_load.py`). Admin writes are accepted on **any** node now;
with a minority alive they answer `503 config_not_published` in 0.0 s while the survivors keep
serving their materialized config. Full checklist: `dev-docs/cluster.md` §6.

### 13.5 Redis outage behavior

The data plane keeps serving (each node's materialized config + local caches), and the **control
plane does not notice at all**: leadership comes from raft, not from a Redis lease. What a Redis
outage costs is the four data-plane roles it still carries — shared rate limits go fail-open,
breaker votes stop syncing, the auth L2 falls back to L1 alone, and invalidation propagation pauses
(entries expire by TTL). See `dev-docs/cluster.md` §4.2 for the full matrix.

**"Until Redis recovers" only held after 2026-09-30.** The pool was built with fred's
`Pool::new(…, policy: None, …)`, i.e. **no reconnect policy at all** — and a policy is not
part of fred's `Config`, so nothing else supplied one. Measured on a real leader whose Redis
link was severed for ~2 s and then restored:

| | before the fix | after |
|---|---|---|
| New TCP connections to Redis in the 90 s after the link returned | **0** | retried every 1 s while down, 2 up immediately |
| `/healthz/leader` | **503 for the whole 90 s** (lease never re-acquired ⇒ the fleet had no leader) | **200 within ~3 s** |
| Every Redis command (lease renew, registry, breaker sync, limit check) | `Timeout Error: Request timed out` on the 500 ms command timeout, forever | resumes as soon as the connection is back |
| Cluster rate limits | fail-open became **permanent** (see below) | fail-open lasts exactly as long as the outage |

fred's own debug log named the cause — `Checking reconnect state. Has policy: false` — and
the fix is `redis::reconnect_policy()` (retry forever, 1 s, jittered), now handed to
`Pool::new` and pinned by a unit test plus the drill `integration/test_cluster_limits.py`.
**A restart was the only recovery before this**, so a Redis blip silently demoted the fleet's
leader for good. Watch `hydra_control_poll_total{result="rate_limit_error"}`: it counts
fail-open decisions, and after the fix it *stops* growing once Redis is back (before, it
grew by one per matched request for the life of the process).

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

**What the drain window actually does (measured 2026-09-30, `integration/test_shutdown_drain.py`,
`HYDRA_SHUTDOWN_DRAIN_SECS=10` and `=1`):**

| Observation | Measured |
|---|---|
| A request IN FLIGHT when `SIGTERM` arrives | still receives a **complete `200`** — the drain finishes what was already accepted |
| The process's exit | **`drain + 5 s`**: 15.0 s with drain=10, 6.0 s with drain=1 (the +5 is Pingora's `graceful_shutdown_timeout_seconds`, the final runtime step). This is the arithmetic above, now with numbers behind it |
| The budget is a real bound | with drain=1 s a request needing 3 s is **cut at ~1.5 s**, not at 3 s |
| **NEW connections during the drain** | **refused — on the data plane AND on the admin port.** Within ~0.2 s of `SIGTERM`, `/healthz`, `/readyz` and `/metrics` stop answering entirely (measured: connection refused), and so does `/v1/…`. So the drain is **in-flight-only**: it does **not** keep accepting for the window, and there is **no probe and no scrape** for up to `drain + 5` s |
| A usage row still in the sink's batch at `SIGTERM` | persisted after the process is gone (the buffered batch is flushed) |

**Operational consequence of that table:** do **not** rely on the drain to serve traffic that
arrives after the signal. In Kubernetes the endpoint removal is asynchronous, so any request that
reaches the pod in that gap is **refused** — gate readiness (or use a `preStop` delay) *before*
`SIGTERM`, and expect a metrics scrape gap of up to `drain + 5` s. The grace period still matters,
for the opposite reason: it is what stops the `SIGKILL` from landing while an in-flight request is
still being finished.

**Why this must be set explicitly:** Pingora's DEFAULT `grace_period_seconds` is
`None` ⇒ 300s, far beyond a typical 30s Kubernetes grace period. The pod would be
`SIGKILL`ed while still draining, losing both the usage-sink flush and the
registry de-registration. The deployment manifests are owned by the operations
repository; this repository only reads the environment variable.

**Which signals flush the usage sink:** `SIGTERM`, `SIGINT` **and `SIGQUIT`**.
`SIGQUIT` is the one that matters in this repository's own deployments: the
systemd unit in §1.2 sets `KillSignal=SIGQUIT` (so a plain `systemctl restart`
sends it) and the rolling upgrade in §3 step 1 is `kill -SIGQUIT <pid>`. Pingora
handles `SIGQUIT` itself for socket handover and then exits through
`process::exit(0)`, which runs **no destructors** — so the flush and the registry
de-registration each need an explicit signal hook
(`main.rs::spawn_sink_flush_on_shutdown`). There is no registry to de-register from any more: that
  second hook went with the registry (ADR-0001 T4.1).

**Measured 2026-09-30, and the earlier wording here was an overclaim.** Removing the hook and
re-running `integration/test_shutdown_drain.py` did NOT lose a buffered row: the periodic sink
flush (5 s) fires inside every exit window (`drain + 5 s ≥ 6 s`), so the row landed anyway — and
with a sink that cannot flush at all (a dead ClickHouse) the batch was still reported as
`usage sink is shutting down with an un-flushable batch … LOST`, because the sink loop reacts to
its channel closing during teardown too. What the hook uniquely owns is doing the flush **before**
that teardown races it, plus the explicit `SIGTERM: flushing usage sinks` / `usage sinks flushed`
log record — and note that the `shutdown_unflushed` **counter dies with the process**, so the LOG
is the only post-mortem evidence that the shutdown drain ran at all. The hook is defence in depth,
not the sole line of defence the previous sentence claimed.

**The same `process::exit(0)` also means SQLite is never closed**, so
`hydra.db-wal` and `hydra.db-shm` stay next to the database after EVERY stop —
`SIGTERM` and `SIGQUIT` alike (measured 2026-09-30, case E of
`integration/test_backup_restore.py`). Harmless while the database file itself stays
in place (the next start replays the WAL), but it is why the restore step in §1.4
insists on deleting those two files: replacing `hydra.db` under a leftover `-wal`
makes the node refuse to boot (`database disk image is malformed`).

### 13.6 Cluster identity: `HYDRA_NODE_ID`, `HYDRA_CLUSTER_ID`, and the member list

There is no registry and no lease, so identity is no longer a row you can inspect — it is
**configuration**, and three values must agree:

1. **`HYDRA_NODE_ID` must appear in `HYDRA_CLUSTER_PEERS`**, and its POSITION in the list is its
   numeric raft id. Two nodes configured with the same id are a startup error (`SelfNotAMember`),
   not a silent split brain — the one class of bug the old topology could not rule out and this one
   can.
2. **Pods need STABLE names.** The `HYDRA_NODE_ID` → `HOSTNAME` → random fallback survives for
   single-node use, but **do not rely on it in a cluster**: a plain Deployment changes `HOSTNAME` on
   every restart, which moves a member away from the id its member list declares. Set
   `HYDRA_NODE_ID` explicitly (a StatefulSet's pinned name, or `$(POD_NAME)`).
3. **`HYDRA_CLUSTER_ID` is the cluster's identity, and its DEFAULT IS DERIVED FROM THE MEMBER LIST.**
   Arachne refuses to open a data directory whose recorded cluster id differs, which is what stops a
   node from silently joining the wrong raft group after a volume is reused or mis-mounted. Because
   the default is a hash of the list, **editing the list changes the identity**, and every existing
   data directory is then rejected at startup:

   ```
   META cluster_id mismatch: expected "hydra-0718fec1d91bd94b", got "hydra-47b808fb0733c41f"
   ```

   (measured 2026-10-05: a 3-member list changed to 4 members on a directory that had already been
   adopted). **If you ever intend to change membership — adding, removing or reordering a member —
   set `HYDRA_CLUSTER_ID` to a stable, human-chosen name from the very first deployment.** Doing it
   later means working out what the old default hash was and writing it explicitly, or the whole
   cluster refuses to start. The change procedure itself is in `dev-docs/cluster.md` §6.3.

**What a node can no longer answer: "is that peer alive?"** There is no heartbeat table and no
per-node RPC, so `/api/v1/cluster/status` reports `alive: null` for peers and the Admin UI renders
that as **unknown** — not as "down", which is a claim nobody measured. Membership is still exact (it
is the configured list); only liveness is unknowable. The retired registry, its reaper and its two
series (`hydra_registry_nodes`, `hydra_registry_reaped_total`) are gone — see §9.1's RETIRED rows —
and `HYDRA_REGISTRY_STALE_GRACE_SECS` is in §13.3b's retired list.

### 13.6b Recovery from a full stop (every member down)

The one thing to know before restarting anything: **a data directory that was never claimed needs a
MAJORITY up at the same time**, because the claim is a raft write and only the leader can make it.
A directory is claimed exactly once, so this is a fact about the FIRST start (or about PVCs that were
replaced), not about ordinary restarts.

| State of `/app/data/arachne` on the members | What to do |
|---|---|
| **Present** (the cluster has run before) | start normally; ORDER DOES NOT MATTER — each node reads its own directory locally (measured: ~500 ms with no peer up at all) |
| **Empty** (first install, or volumes wiped) | bring a MAJORITY up **together**: `docker compose up -d`; on Kubernetes `podManagementPolicy: Parallel`; on bare metal start two/three within the same 10-second window |
| One node up, the others cannot start yet | it exits after **10 s** with `no member adopted this node's Arachne data directory within 10s …`. **Restarting it does not help** — the missing thing is a majority, not a retry. Bring up a second member and start this one again |

The error message names both actions (`cluster.md` §5.6), and `integration/test_startup_knobs.py`
K13 asserts on the wire that it keeps doing so. A **different** error — `belongs to a different
cluster` / `cluster_id mismatch` — is a CONFIGURATION problem, not a start-order one: see §13.6
item 3.

### 13.7 Known limitations (as of this revision)

- ~~Disabled `limit_role` / `provider_key_binding` rows are not carried in
  config snapshots — after a failover they are lost from replicas.~~ **FIXED**:
  the snapshot contract carries the full fidelity rows (including disabled ones,
  `provider_key` identity and tenant access-token hashes), so a promoted replica
  is byte-faithful. See `dev-docs/cluster.md` and the snapshot wire v2 notes.
- `[auth] fail_mode` (`FailMode::Open`, design §11.4) is **implemented but not selectable**: no env var and no config file reaches `AuthConfig.fail_mode`, so the "availability-first" mode cannot be turned on (see §5.x). The default `Closed` is what ships.
- Breaker `threshold` (5) and `probe_interval` (10s) are **not configurable at all**: no env var and no config file exists (measured 2026-09-30: `grep -rn BREAKER_THRESHOLD crates/` and `grep -rn PROBE_INTERVAL crates/` are both empty, and the tree has no config-file loader) — `design.md`'s `[breaker]` sketch and `ops.md` §6.3's earlier "tune `threshold`" advice are therefore about knobs that do not exist. §6.3 now says so; the lever that works is `DELETE /api/v1/breaker/{id}`.
- `HYDRA_FAILOVER_GRACE_MS` was **never wired** and is now in the RETIRED list (§13.3b): raft
  elections decide the handover, so there is nothing for a grace window to do. `HYDRA_BREAKER_QUORUM`
  uses an in-code default (`1`) **and is read** (`main.rs`). `HYDRA_RATE_LIMIT_FAIL_MODE`
  **does not exist at all** (`grep -rn RATE_LIMIT_FAIL_MODE crates/` is empty): the Redis
  rate-limit path is *hard-coded* fail-open and **not configurable** — see
  `crates/hydra-server/src/redis/rate_limit.rs` (`failing open`) and its note "there is NO env
  override".
- Redis sentinel/cluster deployment modes fail fast (single mode wired), and so does **any other
  value** of `HYDRA_REDIS_MODE` — including a typo: `clustr` used to fall through to `single` (measured
  2026-10-01 on the wire: the node started and registered), which is the opposite of this section's
  promise. `HYDRA_REDIS_MODE=single`/`SINGLE`/unset are accepted; anything else stops the process with
  `unsupported HYDRA_REDIS_MODE '<what you wrote>' (supported: single)` — **on a cluster-role node**:
  the mode is read inside `if role.is_cluster()` (`main.rs`), so with the member list unset the
  value is not validated at all (measured 2026-10-01: the node serves, and the only line that can
  mention the variable is the "cluster wiring is configured but …" ERROR, which never quotes the
  value). Pinned by `integration/test_startup_knobs.py` K1/K2 and K12 — and the "on a cluster node"
  half of that pair now runs against a REAL three-member cluster, because a lone member of a
  three-member list cannot boot at all (a fresh data directory must be adopted by a majority within
  10 s; see `cluster.md` §5.6).
- **A node that cannot materialize is retried forever, at a decreasing rate**
  (1 s, 2 s, 4 s … capped at 60 s; a newer tree resets it). It used to give up after three attempts
  *per snapshot* — and since the polling client never re-delivered a version its memory watermark had
  passed, the node stayed unable to lead until a newer config write (which needs a working writer) or
  a restart. Watch `hydra_replica_materialize_retries_total{outcome="failed"}` (wired 2026-10-05 —
  until then the counter was registered with no recorder and this instruction pointed at a series
  that never moved): a sustained non-zero rate means a node cannot materialize the config tree and is
  therefore **not eligible to be the writer** — alert on it.
- The invalidation stream is trimmed by `MAXLEN` (10 000) every 30 s, and the
  generation is bumped — every node clears its whole auth cache (L1+L2) — **only
  when a dropped entry had not been applied by every live node**. Watch
  `hydra_invalidation_generation_bumps_total`: a sustained non-zero rate means a
  consumer is behind (or the event rate exceeds `maxlen / 30 s ≈ 333 events/s`
  with a consumer that cannot keep up). `hydra_invalidation_trimmed_total` counts
  the drops themselves, so "busy but converged" is distinguishable from "losing
  events". Both were previously invisible: the only signal was a log line.
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
