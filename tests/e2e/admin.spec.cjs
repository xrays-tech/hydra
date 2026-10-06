/**
 * Hydra admin UI — Playwright E2E spec (wave-6 §2.2 / design §19.3 / AGENTS.md).
 *
 * Covers:
 *   - T2.1 login with admin token
 *   - T2.2 create provider → list shows it → persisted via /api (DB row exists)
 *   - T2.3 tenant with auth_url + associate provider/model
 *   - T2.4 auth-cache invalidate
 *   - T2.5 breaker reset
 *
 * The suite ASSUMES a running hydra instance (start it with seed.sh first; see
 * tests/e2e/README.md). It does NOT spawn the binary itself: the binary needs
 * a real Pingora listener + a SQLite file, which is environment-specific.
 *
 * Selectors target the CURRENT admin-ui (sidebar #nav > button.nav-item[data-key],
 * generic modal form .modal-overlay > form.form-grid, inputs keyed by
 * [data-field], content table at #content table) as of commit d508daa.
 *
 * Config: HYDRA_BASE (default http://127.0.0.1:8081), HYDRA_ADMIN_TOKEN.
 */
// @ts-check
const { test, expect } = require('@playwright/test');
// Node builtins: the tenant cert case needs a real PEM pair on this node (the
// legacy-path form is READ AND SEALED by the server on write).
const fs = require('fs');
const os = require('os');
const path = require('path');
const { execFileSync } = require('child_process');

const BASE = process.env.HYDRA_BASE || 'http://127.0.0.1:8081';
const TOKEN = process.env.HYDRA_ADMIN_TOKEN || 'dev-admin-token-2026';
// Distinct prefix so parallel/repeated runs don't collide with seed data or
// each other.
const RUN_ID = 'pw-' + Date.now().toString(36);

/** Bearer-authed JSON fetch against /api/v1 — used to assert DB persistence. */
async function api(method, path, { body } = {}) {
  const res = await fetch(`${BASE}/api/v1${path}`, {
    method,
    headers: {
      Authorization: `Bearer ${TOKEN}`,
      'content-type': 'application/json',
    },
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await res.text();
  let json = null;
  if (text) {
    try { json = JSON.parse(text); } catch { json = text; }
  }
  return { status: res.status, json };
}

/** Sidebar nav button for a section key (current UI: #nav .nav-item[data-key]). */
function navItem(page, key) {
  return page.locator(`#nav button.nav-item[data-key="${key}"]`);
}

/** "New <singular>" toolbar button in the section panel head. */
function newButton(page, name) {
  return page.getByRole('button', { name });
}

/** Sign in via the UI overlay. */
async function signIn(page) {
  await page.goto(`${BASE}/admin/`);
  await page.locator('#login-overlay').waitFor({ state: 'visible' });
  await page.fill('#login-token', TOKEN);
  await page.click('#login-btn');
  // Overlay hides on success.
  await expect(page.locator('#login-overlay')).toBeHidden({ timeout: 5000 });
  await expect(page.locator('#token-status')).toContainText('authenticated');
}

/** Create a provider through the UI and return what was filled in.
 *
 *  Selectors follow the conventions this suite already relies on
 *  (`#modal-root`, `[data-field=…]`, `.modal-foot button.btn.primary`) — NOT the
 *  invented ones (`tr[data-id]`, `button[data-action]`, `#modal`, `#confirm-ok`)
 *  that do not exist in `admin-ui/`.
 *
 *  EVERY field T2.2 fills is parameterizable: T2.2 asserts the exact id, key,
 *  endpoint and weight afterwards, so hard-coding them here would break four of
 *  its assertions.
 */
async function createProviderViaUi(
  page,
  {
    id = null, // empty ⇒ the server generates one
    name = `pw-${Date.now()}`,
    key = `prov-${Date.now()}`,
    endpoint = 'http://127.0.0.1:9/',
    weight = null, // null ⇒ leave the form default
  } = {},
) {
  await newButton(page, 'New provider').click();
  if (id) await page.fill('#modal-root [data-field="id"]', id);
  await page.fill('#modal-root [data-field="key"]', key);
  await page.fill('#modal-root [data-field="name"]', name);
  await page.fill('#modal-root [data-field="endpoint"]', endpoint);
  if (weight !== null) {
    await page.fill('#modal-root [data-field="weight"]', String(weight));
  }
  await page.locator('#modal-root .modal-foot button.btn.primary').click();
  await expect(page.locator('#modal-root .modal-overlay')).toBeHidden({ timeout: 5000 });
  return { id, name, key, endpoint, weight };
}

test.describe('Hydra admin UI — CRUD E2E', () => {
  test.beforeAll(async () => {
    // Liveness guard: fail fast with a clear message if the server isn't up.
    const { status } = await api('GET', '/health');
    if (status !== 200) {
      throw new Error(
        `hydra admin API not reachable at ${BASE}/api/v1/health (status ${status}). ` +
        'Start the binary and run tests/e2e/seed.sh first; see tests/e2e/README.md.',
      );
    }
  });

  test('T2.1 login with admin token', async ({ page }) => {
    await signIn(page);
    // Reload button is gated (only visible after auth).
    await expect(page.locator('#reload-btn')).toBeVisible();
  });

  test('T2.1b wrong token is rejected', async ({ page }) => {
    await page.goto(`${BASE}/admin/`);
    await page.fill('#login-token', 'definitely-wrong');
    await page.click('#login-btn');
    await expect(page.locator('#login-error')).toBeVisible({ timeout: 5000 });
    await expect(page.locator('#login-error')).toContainText(/401/);
  });

  // T5 (audit G5) — the ticket lives in sessionStorage for THIS tab.
  test('T2.1c a reload keeps the session (token in sessionStorage)', async ({ page }) => {
    await signIn(page);
    const before = await page.evaluate(() => sessionStorage.getItem('hydra-admin-token'));
    expect(before).toBeTruthy();
    await page.reload();
    await page.locator('#login-overlay').waitFor({ state: 'hidden' });
    await expect(page.locator('body')).toHaveAttribute('data-state', 'ready');
    await expect(page.locator('#reload-btn')).toBeVisible();
  });

  test('T2.1d sign-out is not resurrected by a reload, and a stale ticket fails closed', async ({ page }) => {
    await signIn(page);
    await page.locator('#logout-btn').click();
    await page.locator('#login-overlay').waitFor({ state: 'visible' });
    await page.reload();
    await page.locator('#login-overlay').waitFor({ state: 'visible' });   // not resurrected
    const stored = await page.evaluate(() => sessionStorage.getItem('hydra-admin-token'));
    expect(stored).toBeNull();

    // A ticket the server rejects must fail closed: stay on the login view,
    // clear the key, and show the expired message.
    await page.evaluate(() => sessionStorage.setItem('hydra-admin-token', 'definitely-not-valid'));
    await page.reload();
    await page.locator('#login-overlay').waitFor({ state: 'visible' });
    await expect(page.locator('#login-error')).toContainText(/Session expired|会话已失效/i);
    expect(await page.evaluate(() => sessionStorage.getItem('hydra-admin-token'))).toBeNull();
  });

  // T9.5 (§7-5) — the leader banner. This suite runs against a SINGLE node
  // (`cluster: null`), so the honest assertion here is the NEGATIVE one: no
  // banner on a non-cluster instance, i.e. the feature cannot misfire and tell an
  // operator they are on a standby when they are not. The positive path needs a
  // multi-node fleet and is covered by the cluster tests.
  test('T9.5 no leader banner on a single-node instance', async ({ page }) => {
    await signIn(page);
    await expect(page.locator('#leader-banner')).toHaveCount(0);
    // Give the 30s poll a chance to (wrongly) appear, then re-assert: the banner
    // is created asynchronously after login, so checking once is not enough.
    await page.waitForTimeout(1500);
    await expect(page.locator('#leader-banner')).toHaveCount(0);
    // And `/cluster/status` really does report a single node, so the assertion
    // above is about the banner and not about a failing endpoint.
    const status = await page.evaluate(async () => {
      const r = await fetch('/api/v1/cluster/status', {
        headers: {
          Authorization: 'Bearer ' + sessionStorage.getItem('hydra-admin-token'),
        },
      });
      return { code: r.status, body: await r.json() };
    });
    expect(status.code).toBe(200);
    expect(status.body.cluster).toBe(false);
  });

  // T9.5 (§7-5) — the POSITIVE path: on a non-leader the banner must actually
  // appear, name this node and link to the active leader. A single-node instance
  // cannot produce that state (it has no registry, so `/cluster/status` answers
  // `cluster: false`), so the endpoint is stubbed here with a realistic payload.
  // The endpoint's own contract is covered by the Rust suite; what is under test
  // here is the banner.
  test('T9.5b a non-leader shows the banner with a jump link, and the leader hides it', async ({ page }) => {
    const fleet = (nodeId, holder) => ({
      cluster: true,
      mode: 'leader',
      node_id: nodeId,
      this_node_leader: nodeId === holder,
      lease_holder: holder,
      nodes: [
        {
          node_id: 'node-a',
          role: 'leader',
          control_url: 'http://leader.example:8081',
          alive: true,
          is_lease_holder: holder === 'node-a',
          is_self: nodeId === 'node-a',
        },
        {
          node_id: 'node-b',
          role: 'leader',
          control_url: 'http://standby.example:8082',
          alive: true,
          is_lease_holder: holder === 'node-b',
          is_self: nodeId === 'node-b',
        },
      ],
    });
    let payload = fleet('node-b', 'node-a'); // this node is a STANDBY
    await page.route('**/api/v1/cluster/status', async (route) => {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(payload),
      });
    });

    await signIn(page);
    const banner = page.locator('#leader-banner');
    await expect(banner).toBeVisible();
    await expect(banner).toContainText('node-b'); // which node is not the leader
    await expect(banner.locator('a')).toHaveAttribute(
      'href',
      'http://leader.example:8081', // ...and where the leader is
    );

    // A language switch must re-render it: the text interpolates the node id, so
    // the banner deliberately does NOT use `data-i18n` (there is no variable
    // interpolation in `applyStaticI18n`).
    await page.selectOption('#lang-select', 'zh');
    await expect(banner).toContainText('node-b');
    await expect(banner).toContainText('不是 leader');
    await page.selectOption('#lang-select', 'en');

    // On the leader ITSELF there must be no banner. Reloading re-runs login and
    // the banner refresh (the ticket is in sessionStorage for this tab).
    payload = fleet('node-a', 'node-a');
    await page.reload();
    await page.locator('#login-overlay').waitFor({ state: 'hidden' });
    await expect(page.locator('#leader-banner')).toHaveCount(0);
  });

  // T9.5c — a failed `/cluster/status` probe must not blank the Health page.
  //
  // `renderHealth` awaited `Promise.all([/health, /cluster/status])`, so a 502 from
  // the cluster probe discarded the `/health` payload that had ALREADY arrived: the
  // Status panel kept its skeleton forever while the process itself was healthy.
  // The asymmetry is the tell — `refreshLeaderBanner` polls this same endpoint and
  // already tolerates the failure (`catch { return; }`), while the health page let
  // it take the whole page down.
  //
  // The 502 here is `cluster_unavailable` = "cannot read the cluster registry (Redis
  // unreachable?)", NOT the single-node answer: single-node mode is
  // `200 {cluster:false}` (asserted in T9.5 above, `cluster_api.rs:92-95`), so the
  // UI must show it as a degradation and must NOT print the "HYDRA_ROLE unset" copy.
  //
  // Falsification: restore `Promise.all` and the `.stat` count below fails (the grid
  // still holds its skeleton) while the "HYDRA_ROLE" assertion also fails (the
  // cluster panel never renders at all).
  test('T9.5c a failed cluster probe does not blank the Health page', async ({ page }) => {
    await page.route('**/api/v1/cluster/status', (route) =>
      route.fulfill({
        status: 502,
        contentType: 'application/json',
        body: JSON.stringify({
          error: {
            code: 'cluster_unavailable',
            message: 'cannot read the cluster registry (Redis unreachable?)',
          },
        }),
      }));

    await signIn(page);
    await navItem(page, 'health').click();

    // 1) The Status panel rendered from its own (successful) probe.
    await expect(page.locator('#health-stats .stat')).toHaveCount(5);
    await expect(page.locator('#health-stats .skeleton')).toHaveCount(0);

    // 2) The cluster probe failure is visible AS a failure...
    await expect(page.locator('#cluster-stats')).toContainText(/unavailable/i);
    await expect(page.locator('#cluster-nodes')).toContainText(/cannot read the cluster registry/i);
    // ...and is not misreported as single-node mode.
    await expect(page.locator('#cluster-nodes')).not.toContainText(/HYDRA_ROLE/);
    await expect(page.locator('#cluster-stats .skeleton')).toHaveCount(0);

    // 3) The raw-JSON view carries BOTH outcomes, labelled.
    await expect(page.locator('#health-json')).toContainText(/"health"/);
    await expect(page.locator('#health-json')).toContainText(/cluster_unavailable/);
  });

  test('T2.2 create provider via UI → appears in list → persisted via /api', async ({ page }) => {
    await signIn(page);

    // Open the Providers section and the New form.
    await navItem(page, 'providers').click();
    const id = `${RUN_ID}-prov`;
    // Shared helper (T10.4); every value T2.2 asserts on is passed explicitly.
    await createProviderViaUi(page, {
      id,
      key: `${RUN_ID}-key`,
      name: 'Playwright Provider',
      endpoint: 'https://pw-upstream.example.com',
      weight: 2,
    });

    // The modal hides on success and a toast appears.
    await expect(page.locator('.modal-overlay')).toBeHidden({ timeout: 5000 });
    await expect(page.locator('#toast-root .toast').last()).toContainText(/Created provider/);

    // The list now contains the row (id + endpoint).
    await expect(page.locator('#content table tbody')).toContainText(id);
    await expect(page.locator('#content table tbody')).toContainText('pw-upstream.example.com');

    // DB persistence: a direct /api GET by id returns the row.
    const { status, json } = await api('GET', `/providers/${id}`);
    expect(status).toBe(200);
    expect(json.key).toBe(`${RUN_ID}-key`);
    expect(json.endpoint).toBe('https://pw-upstream.example.com');
    expect(json.weight).toBe(2);
  });

  // T10.4 — the two `clearsFK` call sites in the providers section.
  //
  // `clearsFK: ["providers"]` means a provider create/edit/delete must DROP the
  // foreign-key cache, because that cache is what the FK `<select>`s are built
  // from (`ensureFK('providers')` → `FK.providers`). The observable consequence
  // is therefore NOT "the modal closed" and NOT "the row list refreshed" (that
  // list is re-fetched from the API regardless) — it is that the DEPENDENT
  // DROPDOWN shows the new name instead of the stale one. The option text is
  // `${r.name || r.id} · ${r.id}`.
  // T2.2c — a UI edit must not silently DROP the provider's admission limits.
  //
  // The providers form used to omit the three concurrency columns, while an edit
  // sends the WHOLE record and the server writes every column of the body it
  // receives: a plain rename therefore stored `NULL` in all three, and
  // `proxy.rs` uses `max_concurrency` as the admission gate — so the operator's
  // concurrency cap was silently switched off by editing a name. The columns are
  // invisible in the list, so nothing else in this suite could notice.
  //
  // Falsification: delete the three `max_*` entries from `CRUD.providers.fields`
  // in `admin-ui/app.js` and the `toHaveValue` assertions below time out (the
  // fields would not exist), while the API read-back assertions fail on null.
  test('T2.2c a UI edit keeps the provider concurrency limits', async ({ page }) => {
    const stamp = Date.now();
    const key = `pw-conc-${stamp}`;
    // `created_at`/`updated_at` are REQUIRED fields of the entity the handler
    // deserialises into (the server defaults them when empty — the UI's
    // `collectBody` sends exactly these). Omitting them is a 400
    // `invalid_json: missing field created_at`, which is how this case failed the
    // first time it was ever RUN (see the plan §2ah).
    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',
        updated_at: '',
        key,
        name: `pw-conc-${stamp}`,
        endpoint: 'http://127.0.0.1:9/',
        weight: 1,
        max_concurrency: 7,
        max_queue_depth: 3,
        queue_wait_timeout_ms: 1500,
      },
    });
    expect(created.json && created.json.id, `create failed: ${JSON.stringify(created)}`).toBeTruthy();
    const id = created.json.id;

    await signIn(page);
    await navItem(page, 'providers').click();
    const row = page.locator('#content table tbody tr', { hasText: key });
    await expect(row).toHaveCount(1);
    await row.locator('button[title="Edit"]').click();

    // 1) The form must CARRY the values (this is the part that was missing).
    await expect(page.locator('#modal-root [data-field="max_concurrency"]')).toHaveValue('7');
    await expect(page.locator('#modal-root [data-field="max_queue_depth"]')).toHaveValue('3');
    await expect(page.locator('#modal-root [data-field="queue_wait_timeout_ms"]')).toHaveValue('1500');

    // 2) The edit that used to clear them: rename, save.
    await page.fill('#modal-root [data-field="name"]', `pw-conc-renamed-${stamp}`);
    await page.locator('#modal-root .modal-foot button.btn.primary').click();
    await expect(page.locator('#modal-root .modal-overlay')).toBeHidden({ timeout: 5000 });

    // 3) The server must still hold the limits.
    const after = await api('GET', `/providers/${id}`);
    expect(after.status).toBe(200);
    expect(after.json.max_concurrency).toBe(7);
    expect(after.json.max_queue_depth).toBe(3);
    expect(after.json.queue_wait_timeout_ms).toBe(1500);
  });

  // T2.2d — a write that the runtime did NOT adopt must be reported.
  //
  // `POST /reload` answers `400 reload_failed` and keeps the OLD snapshot when the
  // running process cannot adopt the new config. The UI swallowed that in an empty
  // `catch { /* reload best-effort */ }` inside `writeAndReload`, so the operator saw
  // "Provider updated" and every later view (which reads the DB rows) agreed — while
  // the proxy kept serving the previous config. The DB write is real; the silence
  // was the lie by omission.
  //
  // The stub is a route interception, so this case needs no real failing store.
  // Falsification: put the error toast back INSIDE `writeAndReload` (or restore the
  // empty catch) and the assertion below fails — either no toast at all, or the
  // optimistic "updated" toast is the last one and the warning is buried above it.
  test('T2.2d a failed runtime reload is surfaced after the success toast', async ({ page }) => {
    const stamp = Date.now();
    const key = `pw-reload-${stamp}`;
    const created = await api('POST', '/providers', {
      body: {
        id: '',
        // See T2.2c: the entity's timestamp fields are required by the handler.
        created_at: '',
        updated_at: '',
        key,
        name: `pw-reload-${stamp}`,
        endpoint: 'http://127.0.0.1:9/',
        weight: 1,
      },
    });
    expect(created.json && created.json.id, `create failed: ${JSON.stringify(created)}`).toBeTruthy();
    const id = created.json.id;

    await page.route('**/api/v1/reload', (route) =>
      route.request().method() === 'POST'
        ? route.fulfill({
            status: 400,
            contentType: 'application/json',
            body: JSON.stringify({ error: { code: 'reload_failed', message: 'store reload failed' } }),
          })
        : route.continue());

    await signIn(page);
    await navItem(page, 'providers').click();
    const row = page.locator('#content table tbody tr', { hasText: key });
    await expect(row).toHaveCount(1);
    await row.locator('button[title="Edit"]').click();
    const renamed = `pw-reload-renamed-${stamp}`;
    await page.fill('#modal-root [data-field="name"]', renamed);
    await page.locator('#modal-root .modal-foot button.btn.primary').click();

    // The warning must be the NEWEST toast, not one buried under "Provider updated".
    await expect(page.locator('#toast-root .toast').last()).toContainText(/runtime reload FAILED/, {
      timeout: 5000,
    });

    // ...and the write itself really did land, which is why hiding it is worse than
    // reporting it: the row changed, the runtime did not.
    const after = await api('GET', `/providers/${id}`);
    expect(after.status).toBe(200);
    expect(after.json.name).toBe(renamed);
  });

  test('T2.2b edit and delete a provider through the UI (both clearsFK paths)', async ({ page }) => {
    // No `beforeEach` in this suite (only a `beforeAll` liveness probe) and a
    // fresh context starts on the login overlay: sessionStorage deliberately does
    // NOT carry over between tests (that is T5's design).
    await signIn(page);
    // The two names must NOT be substrings of one another: Playwright's
    // `hasText` is a SUBSTRING match, so `x` and `x-renamed` would both match the
    // renamed row and the assertion below could never distinguish them.
    const stamp = Date.now();
    const name = `pw-fk-orig-${stamp}`;
    const renamed = `pw-fk-renamed-${stamp}`;

    // 1) Populate the FK cache: the bindings section's provider dropdown is
    //    sourced from it.
    await navItem(page, 'provider-key-bindings').click();
    await newButton(page, 'New binding').click();
    const select = page.locator('#modal-root [data-field="provider_id"]');
    await expect(select).toBeVisible();
    // Close without submitting: the CACHE is what we wanted warm.
    await page.keyboard.press('Escape');
    await expect(page.locator('#modal-root .modal-overlay')).toBeHidden({ timeout: 5000 });

    // 2) Create a provider and EDIT its name.
    await navItem(page, 'providers').click();
    await createProviderViaUi(page, { name });
    const row = page.locator('#content table tbody tr', { hasText: name });
    await expect(row).toHaveCount(1);

    await row.locator('button[title="Edit"]').click();
    await page.fill('#modal-root [data-field="name"]', renamed);
    await page.locator('#modal-root .modal-foot button.btn.primary').click();
    // The modal must CLOSE (a submit path that throws leaves it open — the
    // historical failure this suite exists to catch).
    await expect(page.locator('#modal-root .modal-overlay')).toBeHidden({ timeout: 5000 });
    // ...and the list reflects the edit without a manual reload.
    await expect(page.locator('#content table tbody tr', { hasText: renamed })).toHaveCount(1);

    // 3) THE clearsFK ASSERTION: the dependent dropdown must show the NEW name.
    //    With a stale FK cache it still offers the old one.
    await navItem(page, 'provider-key-bindings').click();
    await newButton(page, 'New binding').click();
    const options = page.locator('#modal-root [data-field="provider_id"] option');
    await expect(options.filter({ hasText: renamed })).toHaveCount(1);
    await expect(options.filter({ hasText: name })).toHaveCount(0);
    await page.keyboard.press('Escape');
    await expect(page.locator('#modal-root .modal-overlay')).toBeHidden({ timeout: 5000 });

    // 4) DELETE — the second `clearsFK` call site.
    await navItem(page, 'providers').click();
    await page
      .locator('#content table tbody tr', { hasText: renamed })
      .locator('button[title="Delete"]')
      .click();
    // The confirm dialog's danger button (real markup: `btn danger solid`).
    await page.locator('#modal-root .modal-overlay button.btn.danger.solid').click();
    await expect(page.locator('#content table tbody tr', { hasText: renamed })).toHaveCount(0);

    // ...and the dropdown cache was dropped again: the deleted provider is gone
    // from the dependent select.
    await navItem(page, 'provider-key-bindings').click();
    await newButton(page, 'New binding').click();
    await expect(
      page.locator('#modal-root [data-field="provider_id"] option').filter({ hasText: renamed }),
    ).toHaveCount(0);
    await page.keyboard.press('Escape');
  });

  test('T2.3 tenant with auth_url + associate provider/model', async ({ page }) => {
    await signIn(page);

    // Create a tenant via the UI (auth_url is required).
    await navItem(page, 'tenants').click();
    await newButton(page, 'New tenant').click();
    const tid = `${RUN_ID}-tenant`;
    await page.fill('[data-field="id"]', tid);
    await page.fill('[data-field="name"]', 'Playwright Tenant');
    await page.fill('[data-field="domain"]', `${RUN_ID}.example.com`);
    await page.fill('[data-field="auth_url"]', 'https://auth.pw.example.com/v1/verify');
    await page.locator('.modal-foot button.btn.primary').click();
    await expect(page.locator('.modal-overlay')).toBeHidden({ timeout: 5000 });
    await expect(page.locator('#content table tbody')).toContainText(tid);
    await expect(page.locator('#content table tbody')).toContainText('auth.pw.example.com');

    // Create a provider + model + key to associate.
    const pid = `${RUN_ID}-tp`;
    const mid = `${RUN_ID}-tm`;
    await api('POST', '/providers', {
      body: {
        id: pid, key: `${RUN_ID}-pk`, name: 'P', endpoint: 'https://up.example.com',
        weight: 1, created_at: '', updated_at: '',
      },
    });
    await api('POST', '/provider-models', {
      body: { id: mid, key: `${RUN_ID}-model`, name: 'M', provider_id: pid, status: 1 },
    });

    // Associate via the TenantAccess section (tenant/provider are FK selects).
    await navItem(page, 'tenant-providers').click();
    await newButton(page, 'New access').click();
    const tpid = `${RUN_ID}-tpa`;
    await page.fill('[data-field="id"]', tpid);
    await page.selectOption('[data-field="tenant_id"]', tid);
    await page.selectOption('[data-field="provider_id"]', pid);
    await page.locator('.modal-foot button.btn.primary').click();
    await expect(page.locator('#content table tbody')).toContainText(tpid);

    // And the TenantModels gate (tenant is an FK select, model_key is text).
    await navItem(page, 'tenant-models').click();
    await newButton(page, 'New model gate').click();
    const tmid = `${RUN_ID}-tma`;
    await page.fill('[data-field="id"]', tmid);
    await page.selectOption('[data-field="tenant_id"]', tid);
    await page.fill('[data-field="model_key"]', `${RUN_ID}-model`);
    await page.locator('.modal-foot button.btn.primary').click();
    await expect(page.locator('#content table tbody')).toContainText(tmid);

    // Persistence via /api.
    const { status, json } = await api('GET', `/tenant-providers/${tpid}`);
    expect(status).toBe(200);
    expect(json.tenant_id).toBe(tid);
    expect(json.provider_id).toBe(pid);
  });

  // T2.3b — a UI edit must not LOSE a tenant's legacy cert paths.
  //
  // `admin-ui`'s tenant form has the PEM CONTENT fields (action-based at the
  // handler: blank keeps, `cert_pem:""` clears) but the legacy PATH columns
  // (`cert_file`/`cert_key`) are ordinary record columns written unconditionally by
  // `UPDATE tenant SET … cert_file = ?` — so before the fix, any edit through the
  // UI stored NULL in both, and a pre-0007-style row (paths, no content) silently
  // lost its certificate. There is no way to see that in the list, and the API
  // read-back is the only place it shows.
  //
  // `scripts/check_e2e_contracts.cjs` proves the FORM carries the fields; only this
  // runs the round trip. Falsification: drop `cert_file`/`cert_key` from
  // `CRUD.tenants.fields` and the `toHaveValue` assertion below times out, while
  // the final read-back sees null.
  test('T2.3b a UI edit keeps the tenant legacy cert paths', async ({ page }) => {
    const stamp = Date.now();
    const id = `pw-cert-${stamp}`;
    const domain = `pw-cert-${stamp}.example.com`;

    // A real, parseable PEM pair: the server READS these files on write and seals
    // the content, so garbage would fail the write (and the post-write reload).
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'pw-cert-'));
    const certFile = path.join(dir, 'tenant.crt');
    const certKey = path.join(dir, 'tenant.key');
    try {
      execFileSync('openssl', [
        'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-days', '1',
        '-subj', `/CN=${domain}`,
        '-keyout', certKey, '-out', certFile,
      ], { stdio: 'ignore' });

      // Create with the legacy paths ONLY (no cert_pem): the write converts them.
      const created = await api('POST', '/tenants', {
        body: {
          id,
          created_at: '',
          updated_at: '',
          name: `pw-cert-${stamp}`,
          domain,
          auth_url: 'https://auth.example.com/v1/verify',
          cert_file: certFile,
          cert_key: certKey,
          enabled: true,
        },
      });
      expect(created.status, `tenant create failed: ${JSON.stringify(created)}`).toBe(201);
      const before = await api('GET', `/tenants/${id}`);
      expect(before.json.cert_file).toBe(certFile);

      await signIn(page);
      await navItem(page, 'tenants').click();
      const row = page.locator('#content table tbody tr', { hasText: id });
      await expect(row).toHaveCount(1);
      await row.locator('button[title="Edit"]').click();

      // The form must CARRY the paths (this is the part that was missing).
      await expect(page.locator('#modal-root [data-field="cert_file"]')).toHaveValue(certFile);
      await expect(page.locator('#modal-root [data-field="cert_key"]')).toHaveValue(certKey);

      // The edit that used to clear them: rename, save.
      await page.fill('#modal-root [data-field="name"]', `pw-cert-renamed-${stamp}`);
      await page.locator('#modal-root .modal-foot button.btn.primary').click();
      await expect(page.locator('#modal-root .modal-overlay')).toBeHidden({ timeout: 5000 });

      // The server must still hold them (NULL before the fix).
      const after = await api('GET', `/tenants/${id}`);
      expect(after.status).toBe(200);
      expect(after.json.name).toBe(`pw-cert-renamed-${stamp}`);
      expect(after.json.cert_file, 'a UI edit cleared cert_file').toBe(certFile);
      expect(after.json.cert_key, 'a UI edit cleared cert_key').toBe(certKey);
    } finally {
      fs.rmSync(dir, { recursive: true, force: true });
    }
  });

  test('T2.4 auth-cache invalidate', async ({ page }) => {
    await signIn(page);
    await navItem(page, 'auth-cache').click();
    await page.fill('#inv-tenant', 't-seed');
    await page.fill('#inv-keys', 'sk-nonexistent-aaa, sk-nonexistent-bbb');
    await page.getByRole('button', { name: /Invalidate/ }).click();
    // No entries match ⇒ invalidated: 0, but no error.
    await expect(page.locator('#inv-result')).toContainText(/Invalidated 0/);
  });

  test('T2.5 breaker view + force reset', async ({ page, request }) => {
    // Force a provider into the dead-set via the admin API (the probe task
    // would otherwise take too long to trip deterministically).
    const pid = `${RUN_ID}-brk`;
    await api('POST', '/providers', {
      body: {
        id: pid, key: `${RUN_ID}-brkk`, name: 'B', endpoint: 'https://brk.example.com',
        weight: 1, created_at: '', updated_at: '',
      },
    });
    // Push failures directly through the breaker via the API is not exposed,
    // so we use the UI's "force reset by id" path on a non-dead id — this
    // still exercises the DELETE /breaker/<id> path and the dead-set view.
    await signIn(page);
    await navItem(page, 'breaker').click();
    await page.fill('#breaker-reset-id', pid);
    await page.locator('#content button.btn.primary').click();
    // Toast confirms the reset (id may or may not have been dead).
    await expect(page.locator('#toast-root .toast').last()).toContainText(/Reset/);
  });

  test('T2.6 reload endpoint surfaces new snapshot counts', async ({ page }) => {
    await signIn(page);
    const toast = page.locator('#toast-root .toast').last();
    await page.click('#reload-btn');
    await expect(toast).toContainText(/Reloaded \d+ providers,\s*\d+ tenants/);
  });

  test('T2.7 key-prefix binding CRUD via UI', async ({ page }) => {
    await signIn(page);

    // Seed a provider to bind to (the FK select source).
    const pid = `${RUN_ID}-bind-prov`;
    await api('POST', '/providers', {
      body: {
        id: pid, key: `${RUN_ID}-bindk`, name: 'Bind Provider', endpoint: 'https://bind.example.com',
        weight: 1, created_at: '', updated_at: '',
      },
    });

    // Create a binding via the UI.
    await navItem(page, 'provider-key-bindings').click();
    await newButton(page, 'New binding').click();
    const bid = `${RUN_ID}-bind`;
    await page.fill('[data-field="id"]', bid);
    await page.fill('[data-field="key_prefix"]', `${RUN_ID}_`);
    await page.selectOption('[data-field="provider_id"]', pid);
    await page.locator('.modal-foot button.btn.primary').click();

    // The modal hides on success and the list shows the new row.
    await expect(page.locator('.modal-overlay')).toBeHidden({ timeout: 5000 });
    await expect(page.locator('#content table tbody')).toContainText(bid);
    await expect(page.locator('#content table tbody')).toContainText(`${RUN_ID}_`);

    // DB persistence via /api.
    const { status, json } = await api('GET', `/provider-key-bindings/${bid}`);
    expect(status).toBe(200);
    expect(json.key_prefix).toBe(`${RUN_ID}_`);
    expect(json.provider_id).toBe(pid);
    expect(json.enabled).toBe(true);

    // Edit: disable the binding via the row Edit button.
    const row = page.locator(`#content table tbody tr:has-text("${bid}")`);
    await row.locator('button[title="Edit"]').click();
    await page.uncheck('[data-field="enabled"]');
    await page.locator('.modal-foot button.btn.primary').click();
    await expect(page.locator('.modal-overlay')).toBeHidden({ timeout: 5000 });
    const { json: updated } = await api('GET', `/provider-key-bindings/${bid}`);
    expect(updated.enabled).toBe(false);

    // Delete via the row Delete button + confirm dialog.
    await page.locator(`#content table tbody tr:has-text("${bid}") button[title="Delete"]`).click();
    await page.locator('.modal-overlay button.btn.danger.solid').click();
    await expect(page.locator('#content table tbody')).not.toContainText(bid);
    const { status: delStatus } = await api('GET', `/provider-key-bindings/${bid}`);
    expect(delStatus).toBe(404);
  });

  // T2.8 — an error payload must keep its status, its message and its RETRY HINT.
  //
  // `api()` used to render a non-JSON/empty body verbatim (an ingress 429 with no
  // reason phrase became the literal string "429 429: ") and never read the
  // `Retry-After` the server computes for its rate-limited answers. Stubbed here
  // because the real limit needs 60s of failed attempts — the shape under test is
  // the rendering, not the throttle.
  test('T2.8 a 429 keeps its code, message and Retry-After hint', async ({ page }) => {
    await page.route('**/api/v1/stats/usage', (route) =>
      route.fulfill({
        status: 429,
        contentType: 'application/json',
        body: JSON.stringify({
          error: { code: 'too_many_failed_attempts', message: 'too many failed attempts from this address' },
        }),
        headers: { 'Retry-After': '3' },
      }));

    await signIn(page);
    await navItem(page, 'stats').click();

    const toast = page.locator('#toast-root .toast').last();
    await expect(toast).toContainText(/too_many_failed_attempts/, { timeout: 5000 });
    await expect(toast).toContainText(/too many failed attempts from this address/);
    await expect(toast).toContainText(/retry in 3s/);
    await expect(toast).not.toContainText(/429 429/);
  });
});