/**
 * Hydra admin UI — i18n language switching E2E (2026-08-27 i18n plan, Task 5).
 *
 * Assumes a RUNNING hydra instance (same prerequisites as admin.spec.cjs).
 * Default browser locale is en-US → the UI starts in English; these tests
 * switch via the topbar #lang-select and assert the re-render + persistence
 * (localStorage "hydra-admin-lang").
 */
// @ts-check
const { test, expect } = require('@playwright/test');

const BASE = process.env.HYDRA_BASE || 'http://127.0.0.1:8081';
const TOKEN = process.env.HYDRA_ADMIN_TOKEN || 'dev-admin-token';

async function signIn(page) {
  await page.goto(`${BASE}/admin/`);
  await page.locator('#login-overlay').waitFor({ state: 'visible' });
  await page.fill('#login-token', TOKEN);
  await page.click('#login-btn');
  await expect(page.locator('#login-overlay')).toBeHidden({ timeout: 5000 });
}

test.describe('Hydra admin UI — i18n', () => {
  test('switch to 中文 → nav/title re-render → persisted across reload → back to English', async ({ page }) => {
    await signIn(page);
    // Starts in English (en-US default browser locale).
    await expect(page.locator('#nav button.nav-item[data-key="providers"]')).toContainText('Providers');

    // Switch to Chinese via the topbar selector.
    await page.selectOption('#lang-select', 'zh');
    await expect(page.locator('#nav button.nav-item[data-key="providers"]')).toContainText('提供方');
    await expect(page.locator('#page-title')).toContainText('提供方');
    await expect(page.locator('#token-status')).toContainText('已认证');

    // Persisted across reload (the token is in-memory → sign in again).
    await page.reload();
    await page.locator('#login-overlay').waitFor({ state: 'visible' });
    await page.fill('#login-token', TOKEN);
    await page.click('#login-btn');
    await expect(page.locator('#login-overlay')).toBeHidden({ timeout: 5000 });
    await expect(page.locator('#nav button.nav-item[data-key="providers"]')).toContainText('提供方');

    // Back to English.
    await page.selectOption('#lang-select', 'en');
    await expect(page.locator('#nav button.nav-item[data-key="providers"]')).toContainText('Providers');
  });
});
