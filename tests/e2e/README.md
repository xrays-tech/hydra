# Hydra Admin UI — Playwright E2E suite (wave-6 §2.2)

End-to-end browser tests for the embedded `/admin/*` UI. Covers the AGENTS.md
"web apps must be verified via Playwright" gate.

## What it exercises

`admin.spec.cjs` walks the real UI through:

- **T2.1** `e2e_login_with_admin_token` — wrong token rejected, correct token
  unlocks the dashboard.
- **T2.2** `e2e_create_provider_flow` — UI "New provider" → row appears in the
  list → `/api/v1/providers/<id>` confirms DB persistence (write-through +
  `reload_all`).
- **T2.3** `e2e_tenant_with_auth_url` — tenant with `auth_url` created via UI,
  then associated to a provider + model via the TenantAccess / TenantModels
  sections.
- **T2.4** `e2e_invalidate_auth_cache` — UI triggers `DELETE
  /api/v1/auth/cache`; the response count is shown.
- **T2.5** `e2e_breaker_reset` — UI force-resets a provider id; toast confirms.
- **T2.6** `e2e_reload` — UI triggers `POST /api/v1/reload`; new snapshot
  counts surface.
- **T2.7** `e2e_key_prefix_binding_crud` — Key Bindings section: create a
  prefix→provider binding via UI → row appears → `/api` confirms persistence
  → edit (disable) → delete → 404 via `/api`.

## Prerequisites

1. **Node + Playwright** (only needed for the E2E run, not for the Rust build):
   ```bash
   npm init -y >/dev/null 2>&1 || true   # only if you have no package.json
   npm install --save-dev @playwright/test
   npx playwright install chromium       # downloads the browser
   ```
   The repo deliberately ships **no `package.json`** at the root (the UI
   itself has zero JS build step — see `admin-ui/`). Create one locally or run
   from a CI image that has Playwright pre-installed.

2. **`jq`** for `seed.sh`.

3. **A running hydra binary** with:
   - a writable SQLite file (`HYDRA_DB_URL`),
   - the admin token exported (`HYDRA_ADMIN_TOKEN`),
   - the admin listener on `127.0.0.1:8081` (default) and reachable.

   There is no requirement on a real upstream or a real auth service for the
   UI CRUD flow (the proxy is not exercised here). If you also want to drive
   the proxy end-to-end, stand up a `wiremock` upstream + a `wiremock`
   auth_url per `dev-docs/design.md` §11.3.

## Run

```bash
# 1. Build + start hydra (in one terminal or under your supervisor):
cargo build --release --features server
export HYDRA_ADMIN_TOKEN=dev-admin-token-2026
export HYDRA_DB_URL='sqlite::memory:'     # or a file path for persistence
# Optional: HYDRA_LISTEN, HYDRA_TLS_LISTEN (adds an HTTPS listener),
#           HYDRA_ADMIN_ADDR, RUST_LOG
./target/release/hydra &

# 2. Wait for it, then seed one row in each table so the UI has something
#    to show on first load:
HYDRA_ADMIN_ADDR=127.0.0.1:8081 HYDRA_ADMIN_TOKEN=dev-admin-token-2026 \
  ./tests/e2e/seed.sh

# 3. Run Playwright:
HYDRA_BASE=http://127.0.0.1:8081 HYDRA_ADMIN_TOKEN=dev-admin-token-2026 \
  npx playwright test --config=playwright.config.cjs
```

## Notes

- The suite is **non-parallel** (`workers: 1`) and uses a per-run ID prefix
  (`pw-<timestamp>`) so repeated runs don't collide with seed data or each
  other.
- `seed.sh` is **idempotent** — it deletes the seed rows before re-inserting.
- The wrong-token test (T2.1b) intentionally exercises the fail-closed path.
- If `HYDRA_BASE` is unreachable, the suite fails fast in `beforeAll` with a
  pointer to this README.
- The old `stats_autorefresh.cjs` harness has been **replaced by a real spec**,
  `stats_autorefresh.spec.cjs`. It was not an `@playwright/test` file — requiring
  it started a static HTTP server *and* Chromium, so the runner could never
  collect it, and merely renaming it would have executed all of that at COLLECTION
  time. It also could not simply be dropped: at the time it was retired, nothing
  else covered the stats auto-refresh interval (the plan's note that its coverage
  was "superseded by T9.5/T10.4" was wrong — those are the leader banner and the
  provider edit/delete paths). The new spec drives the same five behaviours
  against the real binary with Playwright's clock API (10s cycles without waiting
  10s), and the manual script is gone so there is only one owner.
- The default admin token in these specs is `dev-admin-token-2026`. It must be
  at least 16 chars (`MIN_ADMIN_TOKEN_LEN`): the older `dev-admin-token` was 15
  bytes, and the binary refuses to start with it — every run then failed before
  the first browser ever launched.
- The `admin-ui/` directory has **no `package.json`** and no build step (T1.4).
  The `package.json` you create for Playwright is for the test harness only,
  not for the UI; CI can flag any `package.json` under `admin-ui/` as a
  regression.

## ⚠ Rebuilding after a UI-only edit

The admin UI is embedded with `include_dir!`, and cargo does **not** track the
contents of a directory a proc macro reads. `crates/hydra-server/build.rs` now
declares `cargo:rerun-if-changed=../../admin-ui`, so an edit under `admin-ui/`
rebuilds the crate — without it, `cargo build --release` would happily re-link a
binary carrying the PREVIOUS UI and these tests would exercise the old app and
pass. (This is not hypothetical: it silently invalidated a counter-proof run.)

Two more traps that cost real time here, both worth knowing before debugging a
"mystery" e2e failure:

- **A previous instance still holds the port.** The new process exits with a bind
  error while the readiness probe is answered by the OLD one, so the suite runs
  against a stale binary. Kill by pid before starting, and assert the served
  bundle is the one you built — `curl -H "Authorization: Bearer $TOKEN"
  $HYDRA_BASE/admin/app.js | grep <your-marker>`.
- **`hasText` is a SUBSTRING match.** Two labels where one contains the other
  (`pw-orig-123` vs `pw-orig-123-renamed`) both match, so an assertion meant to
  prove "the stale name is gone" silently proves nothing. Use non-overlapping
  values in tests that compare before/after strings.

## Why no `webServer` in `playwright.config.cjs`

Pingora's listener + SQLite + env-var secrets are environment-specific. Letting
the test runner spawn the binary would couple browser tests to build artefacts
and port allocation. The orchestrator/CI is the right place to manage the
process lifecycle.
