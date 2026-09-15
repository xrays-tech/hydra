/**
 * Stats auto-refresh (F-7) — Playwright verification with the fake-clock API.
 *
 * Self-contained: it serves admin-ui/ over a local static HTTP server and mocks
 * /api/v1/* via page.route (no hydra binary needed), then drives the real
 * stats.js through the clock API:
 *
 *   - check "auto" → advance the clock 2 cycles (2 × 10s) → the stats request
 *     fires at least twice (auto-refresh reschedules itself);
 *   - uncheck "auto" → advance the clock → request count stays unchanged.
 *
 * Run:  node tests/e2e/stats_autorefresh.cjs
 */
"use strict";
const http = require("http");
const fs = require("fs");
const path = require("path");

// Resolve the (global) playwright module without a local node_modules.
const PW = (() => {
  try { return require("playwright"); }
  catch {
    const g = require("child_process").execSync("npm root -g").toString().trim();
    return require(g + "/playwright");
  }
})();
const { chromium } = PW;

const ADMIN = path.resolve(__dirname, "..", "..", "admin-ui");
const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
};

function startServer() {
  const server = http.createServer((req, res) => {
    const urlPath = decodeURIComponent((req.url || "/").split("?")[0]);
    let filePath;
    if (urlPath === "/" || urlPath === "/admin" || urlPath === "/admin/") {
      filePath = path.join(ADMIN, "index.html");
    } else if (urlPath.startsWith("/admin/")) {
      const rel = urlPath.slice("/admin/".length);
      filePath = path.normalize(path.join(ADMIN, rel));
      if (!filePath.startsWith(ADMIN)) { res.writeHead(403); res.end("forbidden"); return; }
    } else {
      filePath = path.join(ADMIN, "index.html");
    }
    fs.readFile(filePath, (err, data) => {
      if (err) { res.writeHead(404, { "content-type": "text/plain" }); res.end("not found: " + urlPath); return; }
      res.writeHead(200, { "content-type": MIME[path.extname(filePath)] || "application/octet-stream" });
      res.end(data);
    });
  });
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server)));
}

/* Node-side real-time wait — flushes the browser's fetch + the route handler
 * (which runs in Node) without advancing the page's fake clock. */
const settle = (ms = 25) => new Promise((r) => setTimeout(r, ms));

(async () => {
  const server = await startServer();
  const base = "http://127.0.0.1:" + server.address().port;
  const browser = await chromium.launch({ headless: true });
  let failures = 0;
  const assert = (name, cond, detail) => {
    if (cond) console.log("PASS  " + name);
    else { failures++; console.error("FAIL  " + name + (detail ? "  -> " + detail : "")); }
  };

  try {
    const ctx = await browser.newContext();
    const page = await ctx.newPage();
    page.on("pageerror", (e) => console.error("PAGE ERROR:", e.message));

    // Mock the API and count /stats/usage requests.
    let usageCount = 0;
    await page.route("**/api/v1/**", (route) => {
      const url = route.request().url();
      const body = {
        generated_at: "2026-01-01T00:00:00Z",
        totals: { requests: 10, tokens: 20, tokens_prompt: 5, tokens_completion: 5, tenants: 1, providers: 1 },
        by_tenant: [], by_provider: [],
      };
      if (url.includes("/stats/usage")) { usageCount++; return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify(body) }); }
      if (url.includes("/health")) return route.fulfill({ status: 200, contentType: "application/json", body: JSON.stringify({ ok: true }) });
      return route.fulfill({ status: 200, contentType: "application/json", body: "{}" });
    });

    // Install the fake clock BEFORE the page loads so every timer is controlled.
    await page.clock.install();
    await page.clock.setFixedTime(new Date("2026-01-01T00:00:00Z"));

    await page.goto(base + "/admin/", { waitUntil: "domcontentloaded" });
    await page.locator("#login-overlay").waitFor({ state: "visible", timeout: 5000 });
    await page.fill("#login-token", "test-token");
    await page.click("#login-btn");
    await page.locator("#login-overlay").waitFor({ state: "hidden", timeout: 5000 });

    // Navigate to the Stats page and let the initial render settle.
    await page.locator('#nav button.nav-item[data-key="stats"]').click();
    await page.locator("#stats-totals").waitFor({ state: "visible", timeout: 5000 });
    await settle();
    const initial = usageCount;
    assert("initial stats render made a request", initial >= 1, "initial=" + initial);

    // Toggle auto-refresh ON (does not itself fetch — only arms the timer).
    await page.locator("#stats-autorefresh").check();
    await settle();
    const atCheck = usageCount;

    // Advance the clock 2 cycles (2 x 10s) — each tick re-fetches.
    await page.clock.runFor(10000);
    await settle();
    await page.clock.runFor(10000);
    await settle();
    const afterAuto = usageCount;
    assert("auto-refresh fired >= 2 requests over 2 cycles", afterAuto >= atCheck + 2,
      "afterAuto=" + afterAuto + " atCheck=" + atCheck);

    // Uncheck auto-refresh — the interval is cleared, no more re-fetches.
    await page.locator("#stats-autorefresh").uncheck();
    await settle();
    const atUncheck = usageCount;
    await page.clock.runFor(10000);
    await settle();
    await page.clock.runFor(10000);
    await settle();
    assert("after uncheck, request count is unchanged", usageCount === atUncheck,
      "usageCount=" + usageCount + " atUncheck=" + atUncheck);

    // Web-change sanity (app.js + i18n.js): the create-form submit button must
    // render "New provider" (common.form.new), NOT the raw missing key
    // "common.action.create". Confirms the i18n fix through the real UI.
    await page.locator('#nav button.nav-item[data-key="providers"]').click();
    await page.getByRole("button", { name: "New provider" }).first().click();
    await page.locator(".modal-overlay form").waitFor({ state: "visible", timeout: 5000 });
    const btnLabel = (await page.locator(".modal-foot button.btn.primary .btn-label").first().textContent()) || "";
    assert('create button label is "New provider" (i18n fix)', btnLabel.trim() === "New provider", "label=" + btnLabel);
  } finally {
    await browser.close();
    server.close();
  }

  console.log(failures === 0 ? "\nSTATS AUTO-REFRESH: ALL PASSED" : "\nSTATS AUTO-REFRESH: " + failures + " FAILED");
  process.exit(failures ? 1 : 0);
})().catch((e) => { console.error("TEST ERROR", e); process.exit(1); });
