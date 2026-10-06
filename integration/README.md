# Hydra — Integration Tests

End-to-end black-box tests for the **Hydra admin REST API**, exercised against
a real Hydra server running in a throwaway Docker container.

The test suite (`test_crud.py`) is **Python stdlib only** — no `pip install`,
no third-party deps. It mirrors the Rust in-process suite at
`crates/hydra-server/tests/admin_api.rs`, but validates the full HTTP stack
(network → Pingora → admin router → SQLite) the way an operator would.

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
- `tenant.auth_url` mandatory (POST without → 400)
- UNIQUE constraint conflicts → 409 (5 cases)
- FK / CHECK constraint violations → 400 (3 cases)
- 401 without token / with wrong token
- 404 unknown path with `error.code == "not_found"`
- `GET /health`, `POST /reload`, `DELETE /auth/cache` (×2), `GET /breaker`

## Prerequisites

1. **Docker** daemon running.
2. **python3** on `PATH` (used for both the test suite and the health poll).
3. **Image `hydra:latest`** built — from the repo root:

   ```sh
   ./environment/build.sh
   ```

   (Cross-compiles the Linux release binary and builds the image.)

## Run

```sh
./integration/run.sh
```

That's it. The runner will:

1. Verify `hydra:latest` exists.
2. Start a container (`hydra-it`) on port `8081` with an isolated SQLite volume.
3. Poll `/api/v1/health` until the server is ready (fail-fast on boot crash).
4. Run `test_crud.py` (~86 assertions across all entities + edge cases).
5. Tear down the container and volume on exit (even on Ctrl-C).

Exit code is the test suite's exit code (`0` = all passed, `1` = ≥1 failure).

## Configuration (env vars)

| Variable | Default | Purpose |
|---|---|---|
| `HYDRA_IT_PORT` | `8081` | Host port mapped to the container's admin listener |
| `HYDRA_IT_TOKEN` | `hydra-it-token-2026` | Admin bearer token passed to both the container and the test. **Must be >= 16 characters** — the server refuses to start on a shorter admin token (`main.rs`), and `run.sh` now checks the length up front instead of reporting a boot failure |
| `HYDRA_IT_KEEP` | _(unset)_ | Set to any non-empty value to **keep** the container + volume running after the test (for debugging) |
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
  host) and `HYDRA_DB_URL=sqlite:/app/data/hydra.db?mode=rwc` (note: `main.rs`
  reads `HYDRA_DB_URL`, **not** the `HYDRA_DATABASE_URL` the Dockerfile sets).
- `run.sh` generates a fresh `HYDRA_ENCRYPTION_KEY` (base64 32 bytes) per run and
  passes it to the container. The binary fail-closes without it (provider
  api-keys are AES-256-GCM encrypted at rest); a throwaway per-run key is correct
  here because the DB and its keys are torn down on exit.
- `run.sh` tears down both the container and its named volume on exit, so each
  run starts from a clean DB — no cross-run state leakage.
