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
    const full = path.join(dir, fname);
    fs.mkdirSync(path.dirname(full), { recursive: true }); // nested paths: the recursion case below
    fs.writeFileSync(full, content);
  }
  return dir;
}

function runCheck(dir, env = {}) {
  try {
    const out = execFileSync("node", [SCRIPT, dir], {
      encoding: "utf8",
      // Focused fixtures hold one file and three keys; the scan FLOORS are for the real UI and have
      // their own case below (which passes explicit values), so overriding them here cannot disable
      // them — the discipline the other guards' harnesses already follow.
      env: {
        ...process.env,
        I18N_MIN_FILES: "0",
        I18N_MIN_EN_KEYS: "0",
        I18N_MIN_T_KEYS: "0",
        // The UNSCANNED_UI record list is REPLACED (not extended) by this override, so a fixture never
        // inherits the repository's record for `style.css` (which would then be reported STALE for a
        // directory that has no such file). Cases that need a record pass their own JSON.
        I18N_UNSCANNED_UI: "{}",
        ...env,
      },
    });
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

/* 5. 代码→en through a TEMPLATE INTERIOR — a key referenced only as
 *    `${t("k")}` is a real reference and must be validated. The scanner used to
 *    discard the whole template, so this exited 0 with the key missing. */
const tplMissing = makeFixture("tplmissing", {
  en: full, zh: full, fr: full, de: full,
  codeFiles: { "app.js": `
"use strict";
function render(n) {
  const [a, b, c] = [t("stats.auto"), t("stats.refresh"), t("stats.tip")];
  return \`\${t("stats.bogus")} \${n} \${a}\${b}\${c}\`;
}
` },
});
{
  const r = runCheck(tplMissing);
  assert("template-interior missing key exits non-zero", r.status !== 0, "status=" + r.status + " out=" + r.out);
  assert("template-interior missing key is reported", r.out.includes("stats.bogus"), "out=" + r.out);
}

/* 6a. en→引用 through a SINGLE template interior — a key referenced ONLY there
 *     is referenced, not dead. (Pre-fix scanner: DEAD-EN-KEY, because the whole
 *     template was discarded — direction 2 and direction 3 were both blind.) */
const tplOnly = makeFixture("tplonly", {
  en: full, zh: full, fr: full, de: full,
  codeFiles: { "app.js": `
"use strict";
function render(n) {
  const [a, b] = [t("stats.auto"), t("stats.refresh")];
  return \`\${a}\${b}\${n} \${t("stats.tip")}\`;
}
` },
});
{
  const r = runCheck(tplOnly);
  assert("template-only reference is not a dead key", r.status === 0, "status=" + r.status + " out=" + r.out);
}

/* 6b. Brace/nesting torture: a `}` inside a nested string, a `{` inside a plain
 *     string and a nested `${...}` must not end the interior early — if any did,
 *     `stats.tip` would fall out of the reference set and be reported dead.
 *     NOTE: the pre-fix scanner passes this one BY ACCIDENT (its naive backtick
 *     pairing exposes the nested interior to normal scanning), so 6a is the
 *     falsifying case for the dead-key direction; 6b guards extractBraced's
 *     depth handling on its own. */
const tplNested = makeFixture("tplnested", {
  en: full, zh: full, fr: full, de: full,
  codeFiles: { "app.js": `
"use strict";
function render(n, f) {
  const [a, b] = [t("stats.auto"), t("stats.refresh")];
  return \`\${f("}")}|\${ \`\${t("stats.tip")}\` }|\${a}\${b}\${"{"}|\${n}\`;
}
` },
});
{
  const r = runCheck(tplNested);
  assert("nested/build-time braces do not swallow the interior", r.status === 0, "status=" + r.status + " out=" + r.out);
}

/* 7. Nested templates inside a `${...}` interior.
 *
 * A review claimed `extractBraced` parses a nested template as a simple quoted
 * literal, so the interior would be truncated and keys after it silently never
 * validated (a false negative — the very bug class case 5 covers). Measured
 * against the shipped scanner: NOT TRUE. These four cases pin the real behaviour
 * so the claim cannot come back as an untested worry.
 *
 * The refs to auto/refresh/tip keep the dead-key direction quiet, so each case
 * isolates the template question. */
const tplRefs = 'const [a, b, c] = [t("stats.auto"), t("stats.refresh"), "stats.tip"];\n';
const nested = [
  {
    name: "nested template then a call",
    code: 'const s = `x${ ok ? `b${n}c` : t("stats.bogus") }y`;\n',
    expectMissing: "stats.bogus",
  },
  {
    name: "call after a nested template and a regex holding a brace",
    code: 'const s = `x${ ok ? `b${n}` : /}/.test(y) ? t("stats.bogus") : "" }z`;\n',
    expectMissing: "stats.bogus",
  },
];
for (const c of nested) {
  const dir = makeFixture("tpl_" + c.name.replace(/\W+/g, "_"), {
    en: full, zh: full, fr: full, de: full,
    codeFiles: { "app.js": '"use strict";\n' + tplRefs + c.code },
  });
  const r = runCheck(dir);
  assert(`nested-template case "${c.name}" reports the key after it`, r.status !== 0 && r.out.includes(c.expectMissing), "status=" + r.status + " out=" + r.out.trim().slice(0, 120));
}

/* 7b. A `t("...")` in TEMPLATE TEXT (outside any `${...}`) is not a call and must
 *     NOT be reported — that would be a false positive. */
{
  const dir = makeFixture("tpl_text_only", {
    en: full, zh: full, fr: full, de: full,
    codeFiles: { "app.js": '"use strict";\n' + tplRefs + 'const s = `x${ ok ? `b${n}c` : "" } t("stats.bogus")`;\n' },
  });
  const r = runCheck(dir);
  assert("a call inside template TEXT is not reported", r.status === 0, "status=" + r.status + " out=" + r.out.trim().slice(0, 120));
}

/* 7c. The dead-key direction through a nested interior: a key referenced ONLY in
 *     `${ ok ? `w` : t("stats.tip") }` is referenced, not dead. */
{
  const dir = makeFixture("tpl_nested_only_ref", {
    en: full, zh: full, fr: full, de: full,
    codeFiles: { "app.js": '"use strict";\nconst [a, b] = [t("stats.auto"), t("stats.refresh")];\nconst s = `q${ ok ? `w` : t("stats.tip") }`;\n' },
  });
  const r = runCheck(dir);
  assert("a key referenced only inside a nested interior is not dead", r.status === 0, "status=" + r.status + " out=" + r.out.trim().slice(0, 120));
}/* Round 123: the scan must be RECURSIVE, and it must refuse to pass when it read almost nothing.
 * Before this, only the top level of admin-ui/ was read, so a UI module under a subdirectory had
 * its `t("…")` references checked against nothing at all. */
{
  const dir = makeFixture("nested", {
    en: full,
    zh: { "stats.auto": "自动", "stats.refresh": "刷新", "stats.tip": "提示" },
    fr: { "stats.auto": "Auto", "stats.refresh": "Actualiser", "stats.tip": "Astuce" },
    de: { "stats.auto": "Auto", "stats.refresh": "Aktualisieren", "stats.tip": "Tipp" },
    codeFiles: {
      "app.js": `"use strict";\nconst a = t("stats.auto"), b = t("stats.refresh"), c = t("stats.tip");\n`,
      // The nested module references a key that does not exist in en (nor in any locale) — the
      // recursion is what makes this visible.
      "modules/panel.js": `"use strict";\nconst x = t("stats.brand.new.key");\n`,
    },
  });
  const res = runCheck(dir);
  assert(
    "a t() reference in a SUBDIRECTORY is checked (recursive scan)",
    res.status !== 0 && /stats\.brand\.new\.key/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 160)}`,
  );
}
{
  // CONTROL: the same nested layout with the key present in every locale passes — so the case
  // above fails because of the missing key, not because a nested file breaks the scan.
  const dir = makeFixture("nested_ok", {
    en: { ...full, "stats.nested": "Nested" },
    zh: { "stats.auto": "自动", "stats.refresh": "刷新", "stats.tip": "提示", "stats.nested": "嵌套" },
    fr: { "stats.auto": "Auto", "stats.refresh": "Actualiser", "stats.tip": "Astuce", "stats.nested": "Imbriqué" },
    de: { "stats.auto": "Auto", "stats.refresh": "Aktualisieren", "stats.tip": "Tipp", "stats.nested": "Verschachtelt" },
    codeFiles: {
      "app.js": `"use strict";\nconst a = t("stats.auto"), b = t("stats.refresh"), c = t("stats.tip");\n`,
      "modules/panel.js": `"use strict";\nconst x = t("stats.nested");\n`,
    },
  });
  const res = runCheck(dir);
  assert("CONTROL: the same nested layout with the key present passes", res.status === 0, `status=${res.status} ${res.out.trim().slice(0, 160)}`);
}
{
  // The scan floors, with explicit values (so the harness override above cannot hide them).
  const dir = makeFixture("floors", {
    en: full,
    zh: { "stats.auto": "自动", "stats.refresh": "刷新", "stats.tip": "提示" },
    fr: { "stats.auto": "Auto", "stats.refresh": "Actualiser", "stats.tip": "Astuce" },
    de: { "stats.auto": "Auto", "stats.refresh": "Aktualisieren", "stats.tip": "Tipp" },
    codeFiles: { "app.js": `"use strict";\nconst a = t("stats.auto"), b = t("stats.refresh"), c = t("stats.tip");\n` },
  });
  const res = runCheck(dir, { I18N_MIN_FILES: "99" });
  assert(
    "the file floor turns a tiny scan into CANNOT VERIFY (never OK)",
    res.status === 2 && /CANNOT VERIFY/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 160)}`,
  );
}



/* Round 132: an unterminated template literal swallows the rest of the file, so every `t()` after it
 * disappears — the scan used to shrink silently (the `MIN_T_KEYS` floor caught the symptom). The
 * guard now names the cause and refuses to report "consistent". */
{
  const dir = makeFixture("unterminated", {
    en: { "stats.auto": "Auto" },
    zh: { "stats.auto": "自动" },
    fr: { "stats.auto": "Auto" },
    de: { "stats.auto": "Auto" },
    codeFiles: {
      "panel.js": 'const a = t("stats.auto");\nconst broken = `never closed\nconst b = t("stats.auto");\n',
    },
  });
  const res = runCheck(dir);
  assert(
    "an unterminated template literal is reported as CANNOT VERIFY (exit 2)",
    res.status === 2 && /never closes/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 160)}`,
  );
}

/* Round 188: the checked locale set is DISCOVERED from the UI (`LANGUAGES` ∩ `I18N`), not the literal
 * `["zh", "fr", "de"]`. Before this, a language ADDED to the UI was silently unchecked while the OK
 * line still printed "4 locales" — the hard-coded-list-eats-an-object hole. */
function makeLocaleFixture(name, { dict, languages }) {
  const dir = path.join(os.tmpdir(), "check_i18n_" + name);
  fs.rmSync(dir, { recursive: true, force: true });
  fs.mkdirSync(dir, { recursive: true });
  const body = Object.entries(dict)
    .map(([lang, keys]) => `  ${lang}: ${JSON.stringify(nestKeys(keys))},`)
    .join("\n");
  fs.writeFileSync(path.join(dir, "i18n.js"), `
"use strict";
const I18N = {
${body}
};
const LANGUAGES = ${JSON.stringify(languages)};
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
function t(key) { return lookup(I18N[LANG], key) ?? lookup(I18N.en, key) ?? key; }
`);
  fs.writeFileSync(path.join(dir, "app.js"), 'el.textContent = t("a.b");\n');
  return dir;
}

{
  // A NEW locale that is complete: it must be CHECKED and counted (5 locales).
  const dir = makeLocaleFixture("five-locales", {
    dict: {
      en: { "a.b": "B" },
      zh: { "a.b": "B-zh" },
      fr: { "a.b": "B-fr" },
      de: { "a.b": "B-de" },
      es: { "a.b": "B-es" },
    },
    languages: ["en", "zh", "fr", "de", "es"],
  });
  const res = runCheck(dir);
  assert(
    "a fifth, complete locale is checked and counted",
    res.status === 0 && /5 locales \(en, zh, fr, de, es\)/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

{
  // A NEW locale with a MISSING key: it must FAIL (before the fix this passed silently).
  const dir = makeLocaleFixture("five-locales-missing", {
    dict: {
      en: { "a.b": "B" },
      zh: { "a.b": "B-zh" },
      fr: { "a.b": "B-fr" },
      de: { "a.b": "B-de" },
      es: {},
    },
    languages: ["en", "zh", "fr", "de", "es"],
  });
  const res = runCheck(dir);
  assert(
    "...and a MISSING key in that new locale is reported (it was invisible before)",
    res.status === 1 && /MISSING es\s+a\.b/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

{
  const dir = makeLocaleFixture("declared-no-dict", {
    dict: { en: { "a.b": "B" }, zh: { "a.b": "B-zh" } },
    languages: ["en", "zh", "it"],
  });
  const res = runCheck(dir);
  assert(
    "a language DECLARED in LANGUAGES with no dictionary is reported",
    res.status === 1 && /LANGUAGE-DECLARED-NO-DICTIONARY\s+it/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

{
  const dir = makeLocaleFixture("dict-not-selectable", {
    dict: { en: { "a.b": "B" }, zh: { "a.b": "B-zh" }, pt: { "a.b": "B-pt" } },
    languages: ["en", "zh"],
  });
  const res = runCheck(dir);
  assert(
    "a dictionary LANGUAGES never offers is reported",
    res.status === 1 && /DICTIONARY-NOT-SELECTABLE\s+I18N\.pt/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

/* Round 189: the scan read `*.js` + `*.html` BY NAME, so a UI module in any other source language
 * (`.mjs`, `.cjs`, `.jsx`, `.ts`, `.tsx`) was invisible — a key it alone referenced was never checked
 * against `en`, exactly the hole the locale list had. The extension set is explicit now AND the
 * directory is audited: every file under `admin-ui/` must be SCANNED or RECORDED with its reason. */
{
  const dir = makeFixture("unscanned-tsx", {
    en: { "a.b": "B" },
    zh: { "a.b": "B-zh" },
    fr: { "a.b": "B-fr" },
    de: { "a.b": "B-de" },
    codeFiles: { "app.js": 'el.textContent = t("a.b");\n' },
  });
  fs.writeFileSync(path.join(dir, "widget.vue"), 'export const W = () => t("brand.new.key");\n');
  const res = runCheck(dir);
  assert(
    "a UI source in a language this scan does not read is reported, not ignored",
    res.status === 1 && /UNSCANNED-UI-FILE\s+widget\.vue/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

{
  const dir = makeFixture("unscanned-recorded", {
    en: { "a.b": "B" },
    zh: { "a.b": "B-zh" },
    fr: { "a.b": "B-fr" },
    de: { "a.b": "B-de" },
    codeFiles: { "app.js": 'el.textContent = t("a.b");\n' },
  });
  fs.writeFileSync(path.join(dir, "widget.vue"), "export const W = 1;\n");
  const res = runCheck(dir, {
    I18N_UNSCANNED_UI: JSON.stringify({ "widget.vue": "a single-file component with no t() call" }),
  });
  assert(
    "CONTROL: the same file RECORDED passes (and the record is not stale)",
    // Round 196: the OK line now NAMES the recorded files (that is the point — the success path must
    // say what it leaned on), so the assertion is about findings, not about the string appearing:
    // a recorded file must produce no issue line and no stale-record line.
    res.status === 0 && !/STALE-UNSCANNED|UNSCANNED-UI-FILE/.test(res.out)
      && /deliberately unscanned and recorded \(widget\.vue\)/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

{
  const dir = makeFixture("stale-unscanned-record", {
    en: { "a.b": "B" },
    zh: { "a.b": "B-zh" },
    fr: { "a.b": "B-fr" },
    de: { "a.b": "B-de" },
    codeFiles: { "app.js": 'el.textContent = t("a.b");\n' },
  });
  const res = runCheck(dir, {
    I18N_UNSCANNED_UI: JSON.stringify({ "gone.vue": "was a single-file component" }),
  });
  assert(
    "...and a RECORD that no longer applies is reported as stale",
    res.status === 1 && /STALE-UNSCANNED-RECORD\s+gone\.vue/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

{
  const dir = makeFixture("tsx-is-scanned", {
    en: { "a.b": "B" },
    zh: { "a.b": "B-zh" },
    fr: { "a.b": "B-fr" },
    de: { "a.b": "B-de" },
    codeFiles: { "app.js": 'el.textContent = t("a.b");\n' },
  });
  fs.writeFileSync(path.join(dir, "widget.tsx"), 'export const W = () => t("brand.new.key");\n');
  const res = runCheck(dir);
  assert(
    "...while a `.tsx` module IS scanned (its key is checked against en, and reported when missing)",
    res.status === 1 && /CODE-REF-NO-EN\s+brand\.new\.key/.test(res.out),
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

{
  const dir = makeFixture("scanned-html-only", {
    en: { "a.b": "B" },
    zh: { "a.b": "B-zh" },
    fr: { "a.b": "B-fr" },
    de: { "a.b": "B-de" },
    codeFiles: { "index.html": '<div data-i18n="a.b"></div>\n', "app.js": 'el.textContent = t("a.b");\n' },
  });
  const res = runCheck(dir);
  assert(
    "CONTROL: the ordinary file shapes (.js + .html + i18n.js) need no record at all",
    res.status === 0,
    `status=${res.status} ${res.out.trim().slice(0, 200)}`,
  );
}

console.log(failures === 0 ? "\nALL CHECK_I18N TESTS PASSED" : "\n" + failures + " CHECK_I18N TEST(S) FAILED");
process.exit(failures ? 1 : 0);
