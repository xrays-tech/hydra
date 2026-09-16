// Ported from the retired manual script `scripts/stats_autorefresh.cjs`
// (2026-09-16 plan, T8 + F-7).
//
// That script was NOT an `@playwright/test` file: requiring it started a static
// HTTP server AND Chromium, so it could never be collected by the runner. It was
// moved out of `tests/e2e/` for that reason — but the plan (and this repo's
// README) then claimed its coverage was "superseded by T9.5/T10.4", which is
// FALSE: T9.5 is the leader banner and T10.4 is the provider edit/delete
// `clearsFK` paths. The stats auto-refresh behaviour below had no other test, so
// this file is the actual supersession: the same five assertions, driven as a
// normal spec against the REAL binary (no static server, no mocked API).
//
// It uses Playwright's clock API: the switch re-arms a 10s interval, and waiting
// 10 real seconds per cycle would make this the slowest test in the suite.
// @ts-check
const { test, expect } = require('@playwright/test');

const BASE = process.env.HYDRA_BASE || 'http://127.0.0.1:8081';
const TOKEN = process.env.HYDRA_ADMIN_TOKEN || 'dev-admin-token-2026';

/** Sign in via the UI overlay (same convention as the other specs). */
async function signIn(page) {
  await page.goto(`${BASE}/admin/`, { waitUntil: 'domcontentloaded' });
  await page.locator('#login-overlay').waitFor({ state: 'visible' });
  await page.fill('#login-token', TOKEN);
  await page.click('#login-btn');
  await expect(page.locator('#login-overlay')).toBeHidden({ timeout: 5000 });
}

test.describe('Hydra admin UI — stats auto-refresh (F-7)', () => {
  test('the interval re-fetches, and leaving the page stops it', async ({ page }) => {
    // Count only `/stats/usage`; every other request goes to the REAL server, so
    // the login, the reload and the auth path are the production ones.
    let usage = 0;
    await page.route('**/api/v1/stats/usage*', async (route) => {
      usage += 1;
      await route.continue();
    });

    // Install the fake clock BEFORE the page loads so every timer it creates is
    // controlled by this test.
    await page.clock.install();
    await signIn(page);

    // Stats page: the first render fetches.
    await page.locator('#nav button.nav-item[data-key="stats"]').click();
    await expect(page.locator('#stats-totals')).toBeVisible({ timeout: 5000 });
    await expect.poll(() => usage, { timeout: 5000 }).toBeGreaterThanOrEqual(1);
    const initial = usage;

    // Arming the switch does not itself fetch — it starts the interval.
    await page.locator('#stats-autorefresh').check();
    const atCheck = usage;

    // Two cycles (2 × 10s) ⇒ at least two more requests.
    await page.clock.runFor(10000);
    await page.clock.runFor(10000);
    await expect
      .poll(() => usage, { timeout: 5000 })
      .toBeGreaterThanOrEqual(atCheck + 2);
    expect(initial).toBeGreaterThanOrEqual(1);

    // Leaving the page must STOP the interval: otherwise it re-renders itself
    // over whatever page the operator is actually looking at.
    await page.locator('#nav button.nav-item[data-key="providers"]').click();
    await expect(page.locator('#nav button.nav-item[data-key="providers"]')).toHaveClass(
      /active/,
    );
    const afterNav = usage;
    await page.clock.runFor(30000); // three stats cycles
    // Give the (runner-side) network a moment without advancing the page clock.
    await page.waitForTimeout(300);
    expect(usage).toBe(afterNav);
    await expect(page.locator('#stats-totals')).toBeHidden();

    // Coming BACK re-arms it: the switch is a user preference, not per-visit.
    await page.locator('#nav button.nav-item[data-key="stats"]').click();
    await expect(page.locator('#stats-totals')).toBeVisible({ timeout: 5000 });
    const atReturn = usage;
    await page.clock.runFor(10000);
    await expect
      .poll(() => usage, { timeout: 5000 })
      .toBeGreaterThan(atReturn);
  });
});
