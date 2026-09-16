/**
 * Playwright config for the Hydra admin UI E2E suite.
 *
 * The suite assumes a RUNNING hydra instance (see tests/e2e/README.md).
 * We do NOT define `webServer` here: the binary needs a real Pingora listener
 * + a SQLite file, and the orchestrator/CI starts it with environment-specific
 * env vars (DB URL, admin token, mock upstream, mock auth). Start it yourself
 * and then `npx playwright test`.
 *
 * @type {import('@playwright/test').PlaywrightTestConfig}
 */
module.exports = {
  testDir: './tests/e2e',
  timeout: 30_000,
  expect: { timeout: 5_000 },
  fullyParallel: false,           // shared single-instance DB; avoid write races
  // `trace: 'on-first-retry'` records nothing when retries is 0, so the CI
  // diagnostics artefact promised a trace it could never contain.
  retries: process.env.CI ? 1 : 0,
  workers: 1,
  reporter: process.env.CI ? [['github'], ['list']] : 'list',
  use: {
    baseURL: process.env.HYDRA_BASE || 'http://127.0.0.1:8081',
    trace: 'on-first-retry',
    headless: true,
    actionTimeout: 5_000,
    navigationTimeout: 10_000,
  },
  projects: [
    {
      name: 'chromium',
      use: { browserName: 'chromium' },
    },
  ],
};
