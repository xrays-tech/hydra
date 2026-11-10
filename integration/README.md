# Hydra — Integration Tests

End-to-end black-box tests for the **Hydra admin REST API** and the shared-DB cluster,
exercised against real Hydra processes and real PostgreSQL/MySQL + Redis services.

The test suite (`test_crud.py`) is **Python stdlib only** — no `pip install`,
no third-party deps. It mirrors the Rust in-process suite at
`crates/hydra-server/tests/admin_api.rs`, but validates the full HTTP stack
(network → Pingora → admin router → PostgreSQL/MySQL) the way an operator would.

SQLite is **retired**. Every drill that starts a real node needs
`HYDRA_TEST_DATABASE_URL` (a real base database; the integration helper creates a
unique schema/database per run) and the cluster drills need `HYDRA_TEST_REDIS_URL`.
The helpers are `integration/_shared_db.py` (schema/database provisioning) and
`integration/_redis_harness.py` (RESP + a cuttable relay).

## What it covers

Full **CRUD lifecycle** (POST → GET-list → GET-item → PUT → GET-confirm →
DELETE → GET-404) for all 7 config entities:

- providers
- provider-models
- provider-keys
- tenants
- tenant-providers (association — PUT skipped, no update endpoint)
- tenant-models (association — PUT skipped, no update endpoint)
- limit-roles

Plus edge cases and non-CRUD endpoints:

- provider-key masking (always masked `first10…last4`; `?reveal=1` accepted but no-op — P1-5)
- tenant domain UNIQUE
- UNIQUE constraint conflicts → 409 (5 cases)
- FK / CHECK constraint violations → 400 (3 cases)
- 401 without token / with wrong token
- 404 unknown path with `error.code == "not_found"`
- `GET /health`, `POST /reload`, `GET /breaker`

## Prerequisites

1. **Docker** daemon running (for `run.sh`) or a reachable PostgreSQL/MySQL.
2. **python3** on `PATH` (used for the helper scripts and the health poll).
3. **Image `hydra:latest`** built for `run.sh` — from the repo root:

   ```sh
   ./environment/build.sh
   ```

   (Cross-compiles the Linux release binary and builds the image.)
4. A real **test database** and, for the cluster drills, a real **test Redis**:

   ```sh
   export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
   # or: export HYDRA_TEST_DATABASE_URL=mysql://hydra:hydra@127.0.0.1:53306/hydra
   export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
   ```

   The database user needs `CREATE SCHEMA` (PostgreSQL) or `CREATE DATABASE`
   (MySQL). Each drill creates its own schema/database and drops it afterwards.

## Run

```sh
./integration/run.sh
```

That's it for the container-based CRUD suite. The runner will:

1. Verify `hydra:latest` exists.
2. Start a container (`hydra-it`) on port `8081` against `HYDRA_DB_URL`
   (required; SQLite is retired).
3. Poll `/api/v1/health` until the server is ready (fail-fast on boot crash).
4. Run `test_crud.py` (~86 assertions across all entities + edge cases).
5. Tear down the container on exit (even on Ctrl-C).

For the disposable local runner (no Docker, real DB):

```sh
export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
./integration/run-crud-local.sh
```

It provisions a unique schema/database, starts the debug binary with the matching
backend feature, runs the CRUD/error-contract/`init.py` checks, and drops the
schema/database on exit.

## Shared-DB cluster drill (no raft, no leader)

`integration/test_shared_db_cluster.py` starts THREE real nodes against ONE
PostgreSQL/MySQL database and ONE Redis and asserts:

- every admin `/api/v1/health` is 200 and every `/healthz/leader` is 404;
- a write on any node succeeds (no `config_not_published`, no leader);
- a write on B converges on A and C through Redis pub/sub;
- a write on C converges on A and B;
- SIGKILL B: A still writes, C still converges and serves the data plane, B
  restarts and catches up from the shared DB;
- with Redis blocked, A's DB commit still succeeds and C converges through the
  `HYDRA_CONFIG_POLL_MS` fallback;
- cleanup drops the per-run schema/database.

```sh
# PostgreSQL (the CI lane)
cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse,db-postgres --bin hydra
export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
python3 integration/test_shared_db_cluster.py

# MySQL locally: build db-mysql and point HYDRA_TEST_DATABASE_URL at MySQL
cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse,db-mysql --bin hydra
export HYDRA_TEST_DATABASE_URL=mysql://hydra:hydra@127.0.0.1:53306/hydra
python3 integration/test_shared_db_cluster.py
```

The drill uses a dedicated Redis DB index and a local relay; it does **not**
stop the Redis server or touch the `hydra-local` compose stack.

The load drill `integration/test_data_plane_failover_load.py` uses the same
topology: 20 rps at a survivor across a SIGKILL of another node, asserting zero
non-200 responses.

### Concurrent random-write fanout drill

`integration/test_shared_db_random_fanout.py` is the stress complement: three
nodes write different entity kinds **concurrently**, and every node must end up
with the same config. It creates providers, tenants (with provider/model/grants),
standalone tenant grants, limit roles, global roles, and tenant-API identity rows
(user-group/user/api-key) through each node's own admin/tenant API.

Every node is started with `HYDRA_CONFIG_POLL_MS=60000`, so convergence inside the
drill's short window can only come from Redis pub/sub — the poll fallback cannot
explain it. The drill prints the seed and every operation.

```sh
export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
# optional: HYDRA_FANOUT_SEED=<int> (default 20261110)
# optional: HYDRA_FANOUT_ROUNDS=<n> (default 10)
python3 integration/test_shared_db_random_fanout.py
```

What it proves:

- every concurrent CREATE/UPDATE/DELETE answers 2xx (no 5xx / `config_not_published`);
- all three nodes converge to one `config_version` after every phase;
- every created row is visible on every node, each linked tenant's
  snapshot-backed `/api/v1/tenants/{id}/models` contains its model, and a
  representative data-plane request works on all nodes;
- each node's `hydra_config_reload_total{source="pubsub"}` increases while the
  `poll` counter does not;
- updates (including disabling a user: 403 `user_disabled` everywhere) and
  deletions (tenant catalog 404, revoked keys 401, grants gone from the catalog)
  fan out as completely as creates.

The shared process/request/seed helpers live in `integration/_cluster_harness.py`
and are used by both this drill and `test_shared_db_cluster.py`.

### Database-outage drill

`integration/test_shared_db_outage.py` puts a raw TCP relay in front of the
shared database (`integration/_db_harness.py`) and cuts it under a running node.
The node keeps its normal database name/credentials, but its host:port is the
relay.

```sh
export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
python3 integration/test_shared_db_outage.py
# MySQL: export HYDRA_TEST_DATABASE_URL=mysql://... and build db-mysql
```

It asserts:

- before the cut, the seeded tenant/provider/model is served by `/v1/models`;
- while the DB is unreachable, an admin write fails **5xx** (never 2xx) and does
  not advance `config_version`, while the data plane still serves the stale
  committed snapshot with 200;
- the process never exits, the failure is not `config_not_published`;
- after `restore()`, the pool reconnects, the same write succeeds within the
  bounded 30 s budget, `config_version` advances, and a post-recovery
  provider-model/tenant-model change appears on the data plane.

### Compose database-start smoke

CI's `scripts` job not only renders the compose files with `config -q`; it starts
the real `environment/docker-compose.yml` database services:

- `postgres` (default backend), waits for `pg_isready`, runs `SELECT 1`;
- `mysql` under `--profile mysql`, waits for `mysqladmin ping`, runs `SELECT 1`;
- tears both down with their volumes, including on failure.

Hydra itself is not started and no real secret is required. This catches a
service definition that renders but cannot boot (bad health command, volume or
environment), which `config -q` cannot see.

## Configuration (env vars)

| Variable | Default | Purpose |
|---|---|---|
| `HYDRA_TEST_DATABASE_URL` | _(required)_ | Real base PostgreSQL/MySQL URL; each drill creates a unique schema/database |
| `HYDRA_TEST_REDIS_URL` | _(required for cluster drills)_ | Real Redis; the in-process double is forbidden |
| `HYDRA_IT_PORT` | `8081` | Host port mapped to the container's admin listener (`run.sh`) |
| `HYDRA_IT_TOKEN` | `hydra-it-token-2026` | Admin bearer token passed to both the container and the test. **Must be >= 16 characters** — the server refuses to start on a shorter admin token (`main.rs`) |
| `HYDRA_IT_KEEP` | _(unset)_ | Set to any non-empty value to **keep** the container running after the test (for debugging) |
| `HYDRA_IT_HEALTH_TIMEOUT` | `30` | Seconds to wait for the server to become healthy |
| `HYDRA_IT_IMAGE` | `hydra:latest` | Image to run |

### Running the test against an already-running Hydra

`test_crud.py` is standalone and can target any Hydra admin listener:

```sh
HYDRA_BASE_URL="http://my-hydra:8081" \
HYDRA_ADMIN_TOKEN="<your-token>" \
python3 integration/test_crud.py
```

## Notes

- The container boots with `HYDRA_ADMIN_ADDR=0.0.0.0:8081` (overriding the
  `127.0.0.1:8081` default, which is localhost-only and unreachable from the
  host) and the `HYDRA_DB_URL` you supply (note: `main.rs` reads `HYDRA_DB_URL`,
  **not** the `HYDRA_DATABASE_URL` the Dockerfile sets).
- `run.sh` generates a fresh `HYDRA_ENCRYPTION_KEY` (base64 32 bytes) per run and
  passes it to the container. The binary fail-closes without it (provider
  api-keys are AES-256-GCM encrypted at rest); a throwaway per-run key is correct
  here because the database rows are torn down on exit.
- Shell run scripts (`run-crud-local.sh`, `scripts/e2e-local.sh`,
  `scripts/handover.test.sh`) provision a per-run schema/database through
  `python3 integration/_shared_db.py` and drop it on exit.
