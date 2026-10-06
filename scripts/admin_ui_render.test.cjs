#!/usr/bin/env node
/* admin-ui render-logic tests — runnable WITHOUT a browser.
 *
 * Why this exists next to `tests/e2e/admin.spec.cjs`: the Playwright suite needs
 * Chromium plus a freshly built and seeded instance, so it is the ONLY place some
 * UI behaviours were ever asserted — and that means they are unverified on any
 * machine (or in any session) that cannot run it. The behaviours that are pure
 * control flow over API responses do not need a browser: `app.js` is a plain
 * script whose renderers take a response and write into the DOM, so a small DOM
 * stub is enough to drive them and read back what they produced.
 *
 * What the stub does NOT do (so this file cannot fool anyone): no CSS, no layout,
 * no event dispatch, no real `fetch`. It asserts *which* nodes were built and what
 * text they carry — not how they look. Interaction stays in Playwright.
 *
 * The shipped `i18n.js` is loaded for real, so assertions match the shipped
 * English copy and a renamed/removed key fails here as well.
 *
 * Run: node scripts/admin_ui_render.test.cjs    (CI runs it via `node --test`)
 */
"use strict";
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const ADMIN_UI = path.join(__dirname, "..", "admin-ui");

/* --------------------------------------------------------------- DOM stub --- */
function makeDocument() {
  const cache = new Map();
  const make = (tag) => {
    const node = {
      nodeType: 1,
      tagName: String(tag).toUpperCase(),
      childNodes: [],
      attributes: {},
      dataset: {},
      style: {},
      className: "",
      textContent: "",
      innerHTML: "",
      firstElementChild: null,
      classList: { add() {}, remove() {}, toggle() {}, contains: () => false },
      setAttribute(k, v) { this.attributes[k] = String(v); },
      getAttribute(k) {
        return Object.prototype.hasOwnProperty.call(this.attributes, k) ? this.attributes[k] : null;
      },
      addEventListener() {},
      removeEventListener() {},
      remove() { if (this.parentNode) this.parentNode.removeChild(this); },
      focus() {},
      blur() {},
      appendChild(c) { c.parentNode = this; this.childNodes.push(c); return c; },
      prepend(c) { c.parentNode = this; this.childNodes.unshift(c); return c; },
      insertBefore(c) { return this.prepend(c); },
      removeChild(c) {
        const i = this.childNodes.indexOf(c);
        if (i >= 0) this.childNodes.splice(i, 1);
        return c;
      },
      querySelector(sel) { return findIn(this, String(sel)); },
      querySelectorAll() { return []; },
    };
    Object.defineProperty(node, "firstChild", { get() { return this.childNodes[0] || null; } });
    return node;
  };
  const doc = {
    documentElement: { lang: "" },
    body: make("body"),
    createElement: (tag) => make(tag),
    createTextNode: (s) => ({ nodeType: 3, textContent: String(s), childNodes: [] }),
    getElementById: (id) => doc.querySelector("#" + id),
    addEventListener() {},
    removeEventListener() {},
    querySelector(sel) {
      // One stable node per selector: `$("#x")` handed out twice must be the same
      // node, exactly like the real document.
      if (!cache.has(sel)) cache.set(sel, make("div"));
      return cache.get(sel);
    },
    querySelectorAll() { return []; },
  };
  return doc;
}

/** First descendant matching a simple `#id` / `.class` / `tag` / `[attr]` /
 *  `[attr="v"]` selector. */
function selectorMatches(node, sel) {
  if (sel.startsWith("#")) return node.attributes.id === sel.slice(1);
  if (sel.startsWith(".")) return String(node.className || "").split(/\s+/).includes(sel.slice(1));
  const attr = sel.match(/^\[([\w-]+)(?:="([^"]*)")?\]$/);
  if (attr) {
    const name = attr[1];
    let v = node.attributes[name];
    // `el(..., { dataset: { field: "x" } })` sets `dataset`, exactly like the real
    // DOM, where `dataset.field` IS the `data-field` attribute.
    if (v === undefined && name.startsWith("data-")) {
      const key = name.slice(5).replace(/-([a-z])/g, (_, c) => c.toUpperCase());
      v = node.dataset[key];
    }
    return attr[2] === undefined ? v !== undefined : v === attr[2];
  }
  return String(node.tagName || "").toLowerCase() === sel.toLowerCase();
}
function findIn(root, sel) {
  for (const c of root.childNodes || []) {
    if (c.nodeType !== 1) continue;
    if (selectorMatches(c, sel)) return c;
    const deeper = findIn(c, sel);
    if (deeper) return deeper;
  }
  return null;
}

/** Text of a node, mirroring the real DOM closely enough to be trustworthy.
 *
 * A node with element children contributes ONLY its children's text — the stub
 * never sets `textContent` alongside children (`el()` writes either `text` or
 * children), and the real `textContent` getter concatenates descendants rather
 * than adding the parent's own value on top. The earlier version summed both, so
 * every leaf was counted twice and the result was not the DOM's semantics. */
function textOf(node) {
  if (!node) return "";
  const kids = node.childNodes || [];
  if (!kids.length) return node.textContent || "";
  let out = "";
  for (const c of kids) out += (out ? " " : "") + textOf(c);
  return out;
}

/** Descendants (self included) whose className contains `cls` as a whole word. */
function byClass(node, cls, out = []) {
  if (!node) return out;
  if (String(node.className || "").split(/\s+/).includes(cls)) out.push(node);
  for (const c of node.childNodes || []) if (c.nodeType === 1) byClass(c, cls, out);
  return out;
}

/* ------------------------------------------------------------------ loader --- */
/**
 * Load the admin UI's scripts the way `index.html` does — one shared global
 * scope, four separate scripts — and run the named page renderer.
 *
 * `vm.createContext` + one `runInContext` per file is the faithful model of four
 * `<script>` tags: top-level `function` declarations become global properties (so
 * the fake `api` can replace the real one), while two scripts declaring the same
 * `const` would throw here exactly as they would in a browser.
 *
 * @param {{apiImpl?: Function, fetchImpl?: Function, entry?: string|null, args?: any[], callExpr?: string, globals?: object, sourceTransform?: (src: string, file: string) => string}} opts
 */
function loadUi({ apiImpl, entry, sourceTransform, args, fetchImpl, callExpr, globals }) {
  const doc = makeDocument();
  // `api()` refuses to run without a ticket, so the stub hands one out (it is never
  // sent anywhere: `fetch` is stubbed too).
  const storage = {
    getItem: (k) => (k === "hydra-admin-token" ? "test-token" : null),
    setItem() {},
    removeItem() {},
  };
  const context = vm.createContext({
    document: doc,
    window: {},
    navigator: { language: "en-US" },
    localStorage: storage,
    sessionStorage: storage,
    setTimeout: () => 0,
    clearTimeout: () => {},
    setInterval: () => 0,
    clearInterval: () => {},
    fetch:
      fetchImpl ||
      (() => {
        throw new Error("fetch stub: the test must not reach the network");
      }),
    // `generateToken` needs real bytes; the real crypto is available in Node.
    crypto: require("crypto").webcrypto || {}, 
    console,
  });

  for (const file of ["i18n.js", "api-docs.js", "stats.js", "app.js"]) {
    let src = fs.readFileSync(path.join(ADMIN_UI, file), "utf8");
    // Applied to every file: each caller's pattern is specific to the file it means
    // to revert, and a transform that silently matched the wrong script would make
    // its falsification vacuous.
    if (sourceTransform) src = sourceTransform(src, file);
    vm.runInContext(src, context, { filename: file });
  }

  // `entry` goes into the evaluated expression, so it must be an identifier and
  // nothing else — no call, no member access, no string.
  if (!/^[A-Za-z_$][\w$]*$/.test(entry)) throw new Error("bad entry name: " + entry);

  // `apiImpl === undefined` keeps the REAL `api()` — which is what the error-rendering
  // cases need, driven through `fetchImpl` instead.
  if (apiImpl !== undefined) context.api = apiImpl;
  for (const [k, v] of Object.entries(globals || {})) context[k] = v;
  if (callExpr !== undefined) {
    // Escape hatch for entry points that need a value from INSIDE the context
    // (`CRUD.tenants`), which cannot be passed in from Node.
    return { doc, context, run: vm.runInContext(callExpr, context, { filename: "run:expr" }) };
  }
  if (entry === undefined || entry === null) return { doc, context, run: null };
  context.__args = args;
  const call = Array.isArray(args) ? entry + "(...__args)" : entry + "()";
  const run = vm.runInContext(call, context, { filename: "run:" + entry });
  return { doc, context, run };
}

/* ------------------------------------------------------------- fixtures ----- */
const HEALTH_OK = { status: "ok", db: "ok", breaker_dead: 0, tenants: 2, providers: 3 };
const CLUSTER_502 = () =>
  Object.assign(new Error("502 cluster_unavailable: cannot read the cluster registry (Redis unreachable?)"), {
    status: 502,
    code: "cluster_unavailable",
  });

/** `/health` fine, `/cluster/status` 502 — the state the fix is about. */
async function apiHealthyHealthFailingCluster(method, url) {
  if (url === "/health") return HEALTH_OK;
  if (url === "/cluster/status") throw CLUSTER_502();
  throw new Error("unexpected api call: " + method + " " + url);
}

/** `/health` fails, `/cluster/status` answers the single-node payload. */
async function apiFailingHealthSingleNodeCluster(method, url) {
  if (url === "/health") {
    throw Object.assign(new Error("503 storage_busy: db is busy"), { status: 503, code: "storage_busy" });
  }
  if (url === "/cluster/status") {
    return { cluster: false, mode: "single", node_id: "", this_node_leader: false, lease_holder: null, nodes: [] };
  }
  throw new Error("unexpected api call: " + method + " " + url);
}

/** A fetch Response stub: only what `api()` touches. */
function fakeResponse({ status = 200, statusText = "", headers = {}, body = "" } = {}) {
  return {
    ok: status >= 200 && status < 300,
    status,
    statusText,
    headers: {
      get: (k) => {
        const want = String(k).toLowerCase();
        for (const [hk, hv] of Object.entries(headers)) if (hk.toLowerCase() === want) return hv;
        return null;
      },
    },
    text: async () => body,
  };
}

/** A fetch that answers exactly once and records what it was asked for. */
function fetchOnce(resp) {
  const seen = [];
  const fn = async (url, opts) => {
    seen.push({ url, opts });
    return resp;
  };
  fn.seen = seen;
  return fn;
}

/* ------------------------------------------------------------------ runner -- */
let failures = 0;
function assert(name, cond, detail) {
  if (cond) console.log("PASS  " + name);
  else {
    failures++;
    console.error("FAIL  " + name + (detail ? "  -> " + detail : ""));
  }
}

(async () => {
  /* 1. A failed cluster probe must not take the Health page down.
   *    `Promise.all` used to reject on the cluster 502 and discard the `/health`
   *    payload that had already arrived, leaving the Status grid on its skeleton
   *    while the process itself was healthy. */
  {
    const { doc, run, context } = loadUi({ apiImpl: apiHealthyHealthFailingCluster, entry: "renderHealth" });
    let threw = null;
    try {
      await run;
    } catch (e) {
      threw = e;
    }
    const stats = doc.querySelector("#health-stats");
    const cards = byClass(stats, "stat");
    assert(
      "a failed cluster probe still renders the health cards",
      !threw && cards.length === 5,
      `threw=${threw && threw.message} cards=${cards.length}`,
    );
    assert(
      "no skeleton is left in the health grid",
      byClass(stats, "skeleton").length === 0,
      "skeletons=" + byClass(stats, "skeleton").length,
    );
    // Label → VALUE pairing, not "these strings appear somewhere in the grid":
    // the old form passed as long as `ok` and `3` were present anywhere, so a
    // revert that rendered every card as "—" (or swapped labels) could still be
    // green. The labels are read from the shipped locale, so a rename is caught.
    const label = (key) => vm.runInContext(`t(${JSON.stringify(key)})`, context);
    const cardValue = (grid, labelText) => {
      const card = byClass(grid, "stat").find((c) => {
        const sl = findIn(c, ".sl");
        return sl && textOf(sl) === labelText;
      });
      const sv = card && findIn(card, ".sv");
      return sv ? textOf(sv) : undefined;
    };
    assert(
      "the health cards pair each label with the /health value",
      cardValue(stats, label("custom.health.providers")) === "3" &&
        cardValue(stats, label("custom.health.tenants")) === "2" &&
        cardValue(stats, label("custom.health.statusVal")) === "ok",
      JSON.stringify(byClass(stats, "stat").map((c) => textOf(c))),
    );

    /* 2. ...and the cluster failure must be visible AS a failure, not as
     *    "single-node" (single-node is `200 {cluster:false}`, a different fact). */
    const nodes = doc.querySelector("#cluster-nodes");
    assert(
      "the cluster failure is stated with its message",
      /cannot read the cluster registry/i.test(textOf(nodes)),
      textOf(nodes).trim().slice(0, 160),
    );
    assert(
      "the cluster failure is NOT misreported as single-node mode",
      !/HYDRA_ROLE/.test(textOf(nodes)),
      textOf(nodes).trim().slice(0, 160),
    );
    assert(
      "the cluster panel shows 'unavailable' instead of a skeleton",
      /unavailable/i.test(textOf(doc.querySelector("#cluster-stats"))) &&
        byClass(doc.querySelector("#cluster-stats"), "skeleton").length === 0,
      textOf(doc.querySelector("#cluster-stats")).trim().slice(0, 140),
    );
    assert(
      "a cluster probe failure does not toast (only this page's own probe does)",
      byClass(doc.querySelector("#toast-root"), "toast").length === 0,
      "toasts=" + byClass(doc.querySelector("#toast-root"), "toast").length,
    );

    /* 3. The raw view must carry BOTH outcomes, labelled. */
    const json = doc.querySelector("#health-json").innerHTML;
    assert(
      "the raw-JSON view keeps both outcomes",
      json.includes("health") && json.includes("cluster_unavailable"),
      json.slice(0, 140),
    );
  }

  /* 4. A failed `/health` probe is reported where the numbers would be, and the
   *    cluster panel still renders from its own (successful) probe. */
  {
    const { doc, run } = loadUi({ apiImpl: apiFailingHealthSingleNodeCluster, entry: "renderHealth" });
    let threw = null;
    try {
      await run;
    } catch (e) {
      threw = e;
    }
    const stats = doc.querySelector("#health-stats");
    assert("a failed /health probe does not throw out of the renderer", !threw, threw && threw.message);
    assert(
      "a failed /health probe leaves no skeleton",
      byClass(stats, "skeleton").length === 0,
      "skeletons=" + byClass(stats, "skeleton").length,
    );
    assert("...says 'unavailable'", /unavailable/i.test(textOf(stats)), textOf(stats).trim().slice(0, 140));
    assert("...and names the error", /storage_busy/.test(textOf(stats)), textOf(stats).trim().slice(0, 140));
    assert(
      "...and toasts exactly once",
      byClass(doc.querySelector("#toast-root"), "toast").length === 1,
      "toasts=" + byClass(doc.querySelector("#toast-root"), "toast").length,
    );
    assert(
      "the cluster panel still renders from its own successful probe",
      /single-node/i.test(textOf(doc.querySelector("#cluster-stats"))),
      textOf(doc.querySelector("#cluster-stats")).trim().slice(0, 140),
    );
  }

  /* 5. Reverse falsification — the OLD shape must fail case 1 above. Reverting
   *    `allSettled` to `all` is the whole fix, so it is the honest revert. */
  {
    let replaced = 0;
    const { doc, run } = loadUi({
      apiImpl: apiHealthyHealthFailingCluster,
      entry: "renderHealth",
      sourceTransform: (src) =>
        src.replace("Promise.allSettled([", () => {
          replaced++;
          return "Promise.all([";
        }),
    });
    assert("the falsification target exists exactly once", replaced === 1, "replaced=" + replaced);
    let threw = null;
    try {
      await run;
    } catch (e) {
      threw = e;
    }
    const cards = byClass(doc.querySelector("#health-stats"), "stat");
    assert(
      "REVERSE: with Promise.all the health cards do NOT render",
      !!threw || cards.length === 0,
      `threw=${threw && threw.message} cards=${cards.length}`,
    );
  }

  /* 6. CLASS-LEVEL GUARD: no top-level name may be declared by two admin-ui
   *    scripts. The four scripts share ONE global scope (index.html loads them as
   *    plain <script> tags), so a repeated name is not a style issue: the later
   *    declaration silently replaces the earlier one, and the earlier one becomes
   *    dead code that still looks alive. `fmtNum` was exactly that — see case 7. */
  {
    const decls = new Map();
    for (const file of ["i18n.js", "api-docs.js", "stats.js", "app.js"]) {
      const src = fs.readFileSync(path.join(ADMIN_UI, file), "utf8");
      const names = new Set();
      for (const m of src.matchAll(/^(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(/gm)) names.add(m[1]);
      for (const m of src.matchAll(/^(?:const|let|var)\s+([A-Za-z_$][\w$]*)/gm)) names.add(m[1]);
      for (const n of names) decls.set(n, (decls.get(n) || []).concat(file));
    }
    const dupes = [...decls.entries()].filter(([, fs_]) => fs_.length > 1);
    assert(
      "no top-level name is declared by two admin-ui scripts",
      dupes.length === 0,
      dupes.map(([n, fs_]) => `${n}: ${fs_.join(",")}`).join(" | "),
    );
  }

  /* 7. The stats page must actually use ITS compact formatter.
   *    `stats.js` declares `fmtNum` (1234 -> 1.23k, 1500000 -> 1.5M, documented at
   *    its definition) for the token cards and the chart values; `app.js` declared
   *    the SAME global name for a different job (array length / "—" placeholder).
   *    `app.js` loads last, so the stats page silently got the counts formatter and
   *    printed `1234567` where the author wrote `1.23M` — dead code that looked
   *    alive. Renaming is the fix; the collision guard above keeps it that way. */
  const STATS_PAYLOAD = {
    generated_at: "2026-09-29T12:00:00Z",
    totals: {
      requests: 1234567,
      tokens: 2500000,
      tokens_prompt: 1000000,
      tokens_completion: 1500000,
      tenants: 3,
      providers: 4,
    },
    by_tenant: [{ name: "acme", requests: 1234567, tokens: 2500000, tokens_prompt: 1000000, tokens_completion: 1500000 }],
    by_provider: [],
  };
  {
    const { doc, run } = loadUi({ apiImpl: async () => STATS_PAYLOAD, entry: "renderStatsData", args: [STATS_PAYLOAD] });
    let threw = null;
    try {
      await run;
    } catch (e) {
      threw = e;
    }
    const totals = textOf(doc.querySelector("#stats-totals"));
    assert("the stats page renders its cards", !threw && /1\.23M/.test(totals), `threw=${threw && threw.message} text=${totals.trim().slice(0, 120)}`);
    assert("...with the token total abbreviated", /2\.5M/.test(totals), totals.trim().slice(0, 120));
    // Scoped to the CHART nodes: the totals grid lives inside `#content` too and
    // already carries "1.23M", so asserting on all of `#content` made this case
    // vacuous — reverting the chart value to the raw formatter still passed.
    const chartValues = byClass(doc.querySelector("#content"), "chart-value").map(textOf).join(" | ");
    assert(
      "...and the chart value uses the compact formatter too",
      /1\.23M/.test(chartValues) && /2\.5M/.test(chartValues),
      "chart values: " + chartValues,
    );
  }

  /* 8. Reverse falsification: undo the rename (so `stats.js` declares the shared
   *    global again) and the stats page must go back to raw digits. */
  {
    const { doc, run } = loadUi({
      apiImpl: async () => STATS_PAYLOAD,
      entry: "renderStatsData",
      args: [STATS_PAYLOAD],
      sourceTransform: (src) => src.replace(/\bfmtNumCompact\b/g, "fmtNum"),
    });
    await run;
    const totals = textOf(doc.querySelector("#stats-totals"));
    assert(
      "REVERSE: without the rename the stats cards print raw digits",
      /1234567/.test(totals) && !/1\.23M/.test(totals),
      totals.trim().slice(0, 120),
    );
  }

  /* 9-13. `api()` error rendering. These drive the REAL `api()` through a stub
   *       `fetch`: the function that turns a failed response into the string an
   *       operator reads is exactly where the message used to degenerate, and it
   *       needs no browser and no server. */
  async function apiError(opts) {
    const { run } = loadUi({
      fetchImpl: fetchOnce(fakeResponse(opts.response)),
      entry: "api",
      args: ["GET", "/stats/usage"],
      sourceTransform: opts.sourceTransform,
    });
    try {
      await run;
      return { message: null, err: null };
    } catch (e) {
      return { message: e && e.message, err: e };
    }
  }

  /* 9. An empty body must not become "429 429: ". HTTP/2 carries no reason phrase
   *    (`statusText` is ""), and an ingress answering 429 with no body is normal. */
  {
    const { message, err } = await apiError({
      response: { status: 429, statusText: "", headers: { "Retry-After": "3" } },
    });
    assert("an empty 429 body still produces a message", !!message, `msg=${JSON.stringify(message)}`);
    assert("...without printing the status twice", !/429\D+429/.test(String(message)), `msg=${JSON.stringify(message)}`);
    assert("...without a dangling colon", !/:\s*$/.test(String(message)), `msg=${JSON.stringify(message)}`);
    assert(
      "...and Retry-After becomes a retry hint",
      err && err.retryAfterSec === 3 && /retry in 3s/.test(String(message)),
      `msg=${JSON.stringify(message)} retryAfterSec=${err && err.retryAfterSec}`,
    );
  }

  /* 10. A non-JSON body (an ingress HTML page) must not be dumped into a toast. */
  {
    const html =
      "<html><head><title>502 Bad Gateway</title></head><body>" + "<div>noise</div>".repeat(400) + "</body></html>";
    const { message } = await apiError({ response: { status: 502, statusText: "", body: html } });
    assert("an HTML error body is reduced to its title", /502 Bad Gateway/.test(String(message)), `msg=${JSON.stringify(String(message).slice(0, 120))}`);
    assert("...and no markup survives", !/<html|<div>|<\/title>/.test(String(message)), `msg=${JSON.stringify(String(message).slice(0, 120))}`);
    assert("...so the toast stays short", String(message).length < 200, "len=" + String(message).length);
  }

  /* 11. A JSON envelope is still used verbatim (no regression in the good path). */
  {
    const { message } = await apiError({
      response: {
        status: 429,
        statusText: "",
        headers: { "Retry-After": "3" },
        body: JSON.stringify({ error: { code: "too_many_failed_attempts", message: "too many failed attempts from this address" } }),
      },
    });
    assert(
      "a JSON error envelope is rendered with its code and message",
      String(message).startsWith("429 too_many_failed_attempts: too many failed attempts from this address") &&
        /retry in 3s/.test(String(message)),
      `msg=${JSON.stringify(message)}`,
    );
  }

  /* 12. Reverse falsification: drop the status dedup and "429 429" is back. */
  {
    let replaced = 0;
    const { message } = await apiError({
      response: { status: 429, statusText: "" },
      sourceTransform: (src) =>
        src.replace("String(code) === String(status) ? String(status) : `${status} ${code}`", () => {
          replaced++;
          return "`${status} ${code}`";
        }),
    });
    assert("the dedup falsification target exists exactly once", replaced === 1, "replaced=" + replaced);
    assert(
      "REVERSE: without the dedup the status is printed twice",
      /429\D+429/.test(String(message)),
      `msg=${JSON.stringify(message)}`,
    );
  }

  /* 13. Reverse falsification: stop reading Retry-After and the hint disappears. */
  {
    let replaced = 0;
    const { message, err } = await apiError({
      response: { status: 429, statusText: "", headers: { "Retry-After": "3" } },
      sourceTransform: (src) =>
        src.replace('parseRetryAfter(resp.headers && resp.headers.get("Retry-After"))', () => {
          replaced++;
          return "null";
        }),
    });
    assert("the Retry-After falsification target exists exactly once", replaced === 1, "replaced=" + replaced);
    assert(
      "REVERSE: without reading the header there is no retry hint",
      err && err.retryAfterSec === null && !/retry in/.test(String(message)),
      `msg=${JSON.stringify(message)} retryAfterSec=${err && err.retryAfterSec}`,
    );
  }

  /* 14. CLASS-LEVEL GUARD — the admin UI sends the WHOLE record on edit, and the
   *     server writes EVERY column of the body it receives (a missing field is
   *     deserialised as `None` and the column is set to NULL).
   *
   *     So a form that omits a column the server writes unconditionally does not
   *     "leave it alone": editing anything at all CLEARS it. That is how a rename
   *     silently switched off a provider's concurrency cap (P2-1) and how a tenant
   *     edit wiped its legacy `cert_file`/`cert_key` paths (P3) — the same defect
   *     twice, which is why the check reads the server's UPDATE statements instead
   *     of a hand-maintained list. */
  const WHOLE_RECORD_TABLES = { providers: "provider", tenants: "tenant" };
  function coverageGaps(sourceTransform) {
    // A trailing backslash + newline inside the Rust literal is a string
    // continuation: flatten those so each statement is one line.
    // them, then read the column lists of the statements that touch `updated_at`
    // (the targeted single-column UPDATEs — access_token_hash, cert ciphertext —
    // are written by their own paths and must NOT be form fields).
    const dbSrc = fs
      .readFileSync(path.join(__dirname, "..", "crates", "hydra-server", "src", "db.rs"), "utf8")
      .replace(/\\\r?\n\s*/g, " ")
      .replace(/\s+/g, " ");
    const colsByTable = new Map();
    for (const m of dbSrc.matchAll(/UPDATE (tenant|provider) SET ([^"]*?) WHERE /g)) {
      const cols = m[2]
        .split(",")
        .map((x) => x.trim().replace(/\s*=.*$/, ""))
        .filter(Boolean);
      if (!cols.includes("updated_at")) continue;
      const set = colsByTable.get(m[1]) || new Set();
      for (const c of cols) set.add(c);
      colsByTable.set(m[1], set);
    }
    const { context } = loadUi({ sourceTransform });
    const fieldsByKey = JSON.parse(
      vm.runInContext(
        "JSON.stringify(Object.fromEntries(Object.entries(CRUD).map(([k, v]) => [k, (v.fields || []).map((f) => f.name)])))",
        context,
      ),
    );
    const gaps = [];
    for (const [key, table] of Object.entries(WHOLE_RECORD_TABLES)) {
      const cols = colsByTable.get(table);
      if (!cols) {
        gaps.push(`${key}: no whole-record UPDATE found for table ${table}`);
        continue;
      }
      for (const c of cols) {
        if (c === "updated_at") continue; // collectBody re-sends it from the record
        if (!(fieldsByKey[key] || []).includes(c)) gaps.push(`${key}.${c}`);
      }
    }
    return { gaps, colsByTable, fieldsByKey };
  }
  {
    const real = coverageGaps();
    assert(
      "whole-record UPDATEs were found for both tables",
      real.colsByTable.size === 2 && [...real.colsByTable.values()].every((v) => v.size > 2),
      JSON.stringify([...real.colsByTable].map(([k, v]) => [k, [...v].sort()])),
    );
    assert(
      "every unconditionally written column is a form field",
      real.gaps.length === 0,
      real.gaps.join(", "),
    );
  }
  {
    // Reverse falsification, for BOTH historical instances of this defect: drop the
    // field that was missing in each and the same check must report it.
    // The whole field ENTRY (it may span lines), not just its first line: a
    // half-removed object makes the source unparseable, which would look like a
    // failed revert rather than a caught gap.
    const cases = [
      ["the tenants cert path field", /^\s*\{ name: "cert_file"[\s\S]*?\},\n/m],
      ["the providers concurrency field", /^\s*\{ name: "max_concurrency"[\s\S]*?\},\n/m],
    ];
    for (const [label, re] of cases) {
      let replaced = 0;
      const t = coverageGaps((src) =>
        src.replace(re, () => {
          replaced++;
          return "";
        }),
      );
      assert(
        `REVERSE: dropping ${label} is caught by the coverage check`,
        replaced === 1 && t.gaps.length > 0,
        `replaced=${replaced} gaps=${t.gaps.join(", ")}`,
      );
    }
  }

  /* 15. ...and a field that EXISTS but is not PREFILLED is just as broken: the PUT
   *     re-sends whatever the input holds, so an unprefilled field sends "" and
   *     clears the column anyway. This drives `openForm` (the real modal builder)
   *     against a record, which is the only way to see what an edit would submit. */
  {
    const RECORD = {
      id: "acme",
      name: "ACME",
      domain: "acme.example.com",
      auth_url: "https://auth.acme.example.com/v1/verify",
      cert_file: "/certs/acme.crt",
      cert_key: "/certs/acme.key",
      enabled: true,
    };
    const openEdit = (sourceTransform) => {
      const { doc, run } = loadUi({
        apiImpl: async () => [],
        callExpr: "openForm(CRUD.tenants, __record)",
        globals: { __record: RECORD },
        sourceTransform,
      });
      return { doc, run };
    };

    const { doc, run } = openEdit();
    await run;
    const root = doc.querySelector("#modal-root");
    const valueOf = (name) => {
      const n = findIn(root, `[data-field="${name}"]`);
      return n ? n.value : undefined;
    };
    assert("the edit modal prefills the legacy cert paths", valueOf("cert_file") === "/certs/acme.crt" && valueOf("cert_key") === "/certs/acme.key", `cert_file=${JSON.stringify(valueOf("cert_file"))} cert_key=${JSON.stringify(valueOf("cert_key"))}`);
    assert("...and the ordinary fields too", valueOf("name") === "ACME" && valueOf("domain") === "acme.example.com", `name=${JSON.stringify(valueOf("name"))}`);

    let replaced = 0;
    const reverted = openEdit((src) =>
      src.replace(
        'const val = isEdit ? record[f.name] : (f.value !== undefined ? f.value : (f.type === "checkbox" ? false : ""));',
        () => {
          replaced++;
          return 'const val = isEdit ? "" : (f.value !== undefined ? f.value : (f.type === "checkbox" ? false : ""));';
        },
      ),
    );
    await reverted.run;
    const rroot = reverted.doc.querySelector("#modal-root");
    const rval = findIn(rroot, '[data-field="cert_file"]');
    assert("the prefill falsification target exists exactly once", replaced === 1, "replaced=" + replaced);
    assert(
      "REVERSE: without the prefill an edit would submit an empty cert path",
      !rval || rval.value === "",
      `cert_file=${JSON.stringify(rval && rval.value)}`,
    );
  }

  /* 16. `readValue`'s `opt` mapping: a whitespace-only input must be sent as
   *     `null` ("not given"), never as a value. Three of these fields are read by
   *     the server as an explicit CLEAR when present-but-blank
   *     (`resolve_tenant_cert_write` → `CertWrite::Clear`; the access token's
   *     `trim().is_empty() → Some(None)`), so one stray space in the PEM textarea
   *     deleted a tenant's certificate while the label beside it said "leave blank
   *     to keep". The value itself must NOT be trimmed (PEM content round-trips
   *     byte-for-byte). */
  {
    const { context } = loadUi({});
    const read = (map, value, type) =>
      vm.runInContext(
        `String(readValue({ name: "x", map: ${JSON.stringify(map)} }, { type: ${JSON.stringify(type || "text")}, value: ${JSON.stringify(value)}, checked: false }))`,
        context,
      );
    assert("opt + empty stays null", read("opt", "") === "null", read("opt", ""));
    assert("opt + spaces becomes null (not a clear)", read("opt", "   ") === "null", JSON.stringify(read("opt", "   ")));
    assert("opt + newline/tab becomes null", read("opt", "\n\t ") === "null", JSON.stringify(read("opt", "\n\t ")));
    assert("opt + a real path is passed through", read("opt", "/certs/acme.crt") === "/certs/acme.crt", JSON.stringify(read("opt", "/certs/acme.crt")));
    assert(
      "opt does NOT trim the value it sends (PEM must round-trip)",
      read("opt", "  /certs/acme.crt  ") === "  /certs/acme.crt  ",
      JSON.stringify(read("opt", "  /certs/acme.crt  ")),
    );
    assert(
      "textarea treats spaces the same way (cert_pem is a textarea)",
      read("opt", "  ", "textarea") === "null",
      JSON.stringify(read("opt", "  ", "textarea")),
    );
  }

  /* 17. Reverse falsification: put the old mapping back and the whitespace case
   *     must be caught again. */
  {
    let replaced = 0;
    const { context } = loadUi({
      sourceTransform: (src) =>
        src.replace('case "opt": return v.trim() === "" ? null : v;', () => {
          replaced++;
          return 'case "opt": return v === "" ? null : v;';
        }),
    });
    const v = vm.runInContext(
      'String(readValue({ name: "cert_pem", map: "opt" }, { type: "textarea", value: "   ", checked: false }))',
      context,
    );
    assert("the opt falsification target exists exactly once", replaced === 1, "replaced=" + replaced);
    assert(
      "REVERSE: without the trim a spacing-only input is sent as a clear",
      v === "   ",
      JSON.stringify(v),
    );
  }

  console.log(failures === 0 ? "\nALL ADMIN-UI RENDER TESTS PASSED" : "\n" + failures + " ADMIN-UI RENDER TEST(S) FAILED");
  process.exit(failures ? 1 : 0);
})();
