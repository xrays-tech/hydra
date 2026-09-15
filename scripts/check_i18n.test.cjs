#!/usr/bin/env node
/* TDD test for scripts/check_i18n.js — bidirectional i18n validation.
 *
 * Runs the checker against controlled fixture admin-ui dirs (in os.tmpdir)
 * and asserts the exit-code contract:
 *   - clean fixture            -> exit 0
 *   - code refs missing key    -> exit non-zero  (代码→en)
 *   - en key not referenced    -> exit non-zero  (en→引用 / dead key)
 *   - locale missing a key     -> exit non-zero  (en→zh/fr/de, pre-existing)
 *
 * Run: node scripts/check_i18n.test.cjs
 */
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const SCRIPT = path.join(__dirname, "check_i18n.js");

/* Convert flat dotted keys ("a.b.c": v) into the nested table shape the real
 * i18n.js uses ({ a: { b: { c: v } } }). */
function nestKeys(flat) {
  const out = {};
  for (const [k, v] of Object.entries(flat)) {
    const parts = k.split(".");
    let node = out;
    for (let i = 0; i < parts.length - 1; i++) {
      const p = parts[i];
      if (!(p in node) || typeof node[p] !== "object") node[p] = {};
      node = node[p];
    }
    node[parts[parts.length - 1]] = v;
  }
  return out;
}

/* Minimal i18n.js with the full interface the checker sandbox needs. */
function makeI18nFile(en, zh, fr, de) {
  return `
"use strict";
const I18N = {
  en: ${JSON.stringify(nestKeys(en))},
  zh: ${JSON.stringify(nestKeys(zh || {}))},
  fr: ${JSON.stringify(nestKeys(fr || {}))},
  de: ${JSON.stringify(nestKeys(de || {}))},
};
const LANGUAGES = ["en", "zh", "fr", "de"];
function lookup(dict, key) {
  if (!dict) return undefined;
  const parts = key.split(".");
  let node = dict;
  for (const p of parts) {
    if (node && typeof node === "object" && p in node) node = node[p];
    else return undefined;
  }
  return typeof node === "string" ? node : undefined;
}
let LANG = "en";
function currentLang() { return LANG; }
function setLang(code) { LANG = code; }
function t(key, vars) {
  let s = lookup(I18N[LANG], key);
  if (s === undefined) s = lookup(I18N.en, key);
  if (s === undefined) s = key;
  if (vars && typeof s === "string") {
    for (const [k, v] of Object.entries(vars)) s = s.split("{" + k + "}").join(String(v));
  }
  return s;
}
`;
}

function makeFixture(name, { en, zh, fr, de, codeFiles }) {
  const dir = path.join(os.tmpdir(), "check_i18n_" + name);
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  fs.writeFileSync(path.join(dir, "i18n.js"), makeI18nFile(en, zh, fr, de));
  for (const [fname, content] of Object.entries(codeFiles)) {
    fs.writeFileSync(path.join(dir, fname), content);
  }
  return dir;
}

function runCheck(dir) {
  try {
    const out = execFileSync("node", [SCRIPT, dir], { encoding: "utf8" });
    return { status: 0, out };
  } catch (e) {
    return { status: e.status === undefined ? 1 : e.status, out: (e.stdout || "") + (e.stderr || "") };
  }
}

let failures = 0;
function assert(name, cond, detail) {
  if (cond) console.log("PASS  " + name);
  else { failures++; console.error("FAIL  " + name + (detail ? "  -> " + detail : "")); }
}

const full = { "stats.auto": "Auto", "stats.refresh": "Refresh", "stats.tip": "Tip" };

/* 1. clean — every en key referenced, every code ref exists in en, locales complete. */
const clean = makeFixture("clean", {
  en: full,
  zh: { "stats.auto": "自动", "stats.refresh": "刷新", "stats.tip": "提示" },
  fr: { "stats.auto": "Auto", "stats.refresh": "Actualiser", "stats.tip": "Astuce" },
  de: { "stats.auto": "Auto", "stats.refresh": "Aktualisieren", "stats.tip": "Tipp" },
  codeFiles: {
    "app.js": `
"use strict";
function render() {
  const a = t("stats.auto");
  const b = t("stats.refresh");
  const label = "stats.tip"; // referenced as a value (not a direct t() call)
  return [a, b, label];
}
`,
  },
});
{
  const r = runCheck(clean);
  assert("clean fixture exits 0", r.status === 0, "status=" + r.status + " out=" + r.out);
}

/* 2. 代码→en — code references a key that does not exist in en. */
const missing = makeFixture("missing", {
  en: full, zh: full, fr: full, de: full,
  codeFiles: { "app.js": `
"use strict";
function render() { return [t("stats.auto"), t("stats.bogus")]; }
` },
});
{
  const r = runCheck(missing);
  assert("code-missing-key exits non-zero", r.status !== 0, "status=" + r.status + " out=" + r.out);
  assert("code-missing-key reports the missing key", r.out.includes("stats.bogus"), "out=" + r.out);
}

/* 3. en→引用 — en has a key that is never referenced by code (dead key). */
const dead = makeFixture("dead", {
  en: { ...full, "unused.dead": "Dead" },
  zh: { ...full, "unused.dead": "死" },
  fr: { ...full, "unused.dead": "Mort" },
  de: { ...full, "unused.dead": "Tot" },
  codeFiles: { "app.js": `
"use strict";
function render() { return [t("stats.auto"), t("stats.refresh"), "stats.tip"]; }
` },
});
{
  const r = runCheck(dead);
  assert("dead-en-key exits non-zero", r.status !== 0, "status=" + r.status + " out=" + r.out);
  assert("dead-en-key reports the dead key", r.out.includes("unused.dead"), "out=" + r.out);
}

/* 4. en→zh/fr/de (pre-existing) — en has a key that a locale is missing. */
const locMissing = makeFixture("locomissing", {
  en: full,
  zh: { "stats.auto": "自动", "stats.refresh": "刷新" }, // stats.tip missing in zh
  fr: full, de: full,
  codeFiles: { "app.js": `
"use strict";
function render() { return [t("stats.auto"), t("stats.refresh"), "stats.tip"]; }
` },
});
{
  const r = runCheck(locMissing);
  assert("locale-missing exits non-zero", r.status !== 0, "status=" + r.status + " out=" + r.out);
  assert("locale-missing reports the missing key", r.out.includes("stats.tip"), "out=" + r.out);
}

console.log(failures === 0 ? "\nALL CHECK_I18N TESTS PASSED" : "\n" + failures + " CHECK_I18N TEST(S) FAILED");
process.exit(failures ? 1 : 0);
