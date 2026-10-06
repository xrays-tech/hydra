#!/usr/bin/env node
/* Admin-UI i18n 词条完整性检查（2026-08-27 i18n 计划 Task 2–4 复用）。
 *
 * 加载 admin-ui/i18n.js（以 new Function 注入浏览器桩，避免 eval 严格模式
 * 作用域隔离），做四向校验，任一违规即非零退出：
 *   1. en→zh/fr/de   现有：en 中每个 key 在 zh/fr/de 都必须存在；
 *   2. 代码→en       新增：代码里 t("...") 字面量（含动态 t("crud."+f.fk+".nav")，
 *                     以及模板字面量 `${...}` 内部——那里也是真实代码）
 *                     引用的每个 key 必须在 I18N.en 中存在（防代码引用缺失词条）；
 *   3. en→引用      新增：I18N.en 中每个 key 都必须被代码引用（防死键）。
 *
 * 用法：node scripts/check_i18n.js [adminUiDir]   （缺省为仓库 admin-ui/）
 *
 * 实现细节见文件底部：字符串字面量用状态机扫描（正确处理 引号/模板/注释/正则），
 * 不依赖正则配对引号（空串 ""、字符串拼接都会破坏朴素正则）。
 */
"use strict";
const fs = require("fs");
const { records, audit } = require("./recorded_exceptions.cjs");
const path = require("path");

/* ---------------------------------------------------------------------------
 * Robust JS string-literal scanner.
 * Tracks: double/single-quoted strings, template literals, line/block comments,
 * and regex literals (distinguished from division by the preceding token).
 * Returns: { literals: Set<string>, code: string } where `code` is the source
 * with comments removed (strings preserved) — used for t()/fk regexes.
 * ------------------------------------------------------------------------- */
/* Extract the source text of a `${ ... }` interior from a template literal.
 * `i` points just past the opening `${`; returns { inner, end } where `end` is
 * the index just past the matching `}`.
 *
 * Brace depth alone is NOT enough — a `}` inside a nested string, template or
 * regex would close the interior early and a `{` inside one would close it
 * never — so this mirrors the main scanner's lexical rules.
 */
function extractBraced(src, i) {
  const n = src.length;
  let out = "";
  let depth = 1;
  const lastSig = () => {
    for (let k = out.length - 1; k >= 0; k--) if (!/\s/.test(out[k])) return out[k];
    return "";
  };
  while (i < n) {
    const c = src[i];
    const c2 = src[i + 1];
    if (c === "/" && c2 === "/") {
      while (i < n && src[i] !== "\n") { out += src[i]; i++; }
      continue;
    }
    if (c === "/" && c2 === "*") {
      out += "/*"; i += 2;
      while (i < n && !(src[i] === "*" && src[i + 1] === "/")) { out += src[i]; i++; }
      out += "*/"; i += 2;
      continue;
    }
    if (c === '"' || c === "'" || c === "`") {
      const q = c;
      out += c; i++;
      while (i < n) {
        if (src[i] === "\\") { out += src[i] + (src[i + 1] || ""); i += 2; continue; }
        if (src[i] === q) { out += q; i++; break; }
        if (q !== "`" && src[i] === "\n") { out += q; i++; break; } // unterminated
        if (q === "`" && src[i] === "$" && src[i + 1] === "{") {
          out += "${"; i += 2;
          const sub = extractBraced(src, i);
          out += sub.inner + "}"; i = sub.end;
          continue;
        }
        out += src[i]; i++;
      }
      continue;
    }
    const ps = lastSig();
    if (c === "/" && (ps === "" || !/[A-Za-z0-9_$)\]}]/.test(ps))) {
      out += "/"; i++;
      let inClass = false;
      while (i < n) {
        if (src[i] === "\\") { out += src[i] + (src[i + 1] || ""); i += 2; continue; }
        if (src[i] === "[") inClass = true;
        else if (src[i] === "]") inClass = false;
        out += src[i];
        if (src[i] === "/" && !inClass) { i++; break; }
        if (src[i] === "\n") break;
        i++;
      }
      while (i < n && /[a-z]/.test(src[i])) { out += src[i]; i++; }
      continue;
    }
    if (c === "{") depth++;
    else if (c === "}") { depth--; if (depth === 0) return { inner: out, end: i + 1 }; }
    out += c; i++;
  }
  return { inner: out, end: n }; // unterminated template — take the rest
}

function scanSource(src) {
  const literals = new Set();
  let code = "";
  // A template literal that never closes swallows the rest of the file, so every `t()` after it
  // disappears — reported instead of silently shrinking the scan (the `MIN_T_KEYS` floor catches the
  // symptom, this names the cause). Round 132.
  let unterminated = false;
  const n = src.length;
  let i = 0;
  let prevSig = ""; // last significant char, to decide regex-vs-division
  const isRegStart = () => {
    if (prevSig === "") return true;
    return !/[A-Za-z0-9_$)\]}]/.test(prevSig);
  };
  while (i < n) {
    const c = src[i];
    const c2 = src[i + 1];
    // line comment
    if (c === "/" && c2 === "/") {
      while (i < n && src[i] !== "\n") i++;
      code += "\n";
      continue;
    }
    // block comment
    if (c === "/" && c2 === "*") {
      i += 2;
      while (i < n && !(src[i] === "*" && src[i + 1] === "/")) i++;
      i += 2;
      continue;
    }
    // single/double quoted string
    if (c === "\"" || c === "'") {
      const q = c;
      i++;
      let buf = "";
      while (i < n && src[i] !== q) {
        if (src[i] === "\\") { buf += src[i + 1]; i += 2; continue; }
        if (src[i] === "\n") break; // unterminated line string
        buf += src[i];
        i++;
      }
      i++; // consume closing quote
      literals.add(buf);
      code += q + buf + q;
      continue;
    }
    // Template literal. The text chunks are not keys (a key built at runtime is
    // unverifiable by construction), but `${...}` interiors ARE real code and
    // must be scanned: discarding the whole template made the checker report
    // "code↔en consistent" while a key referenced only as `${t("a.b")}` was
    // MISSING from i18n.js, and symmetrically reported an existing key as DEAD.
    // Both validation directions were blind inside every template literal.
    if (c === "`") {
      i++;
      code += "``";
      let closed = false;
      while (i < n) {
        if (src[i] === "\\") { i += 2; continue; }
        if (src[i] === "`") { i++; closed = true; break; }
        if (src[i] === "$" && src[i + 1] === "{") {
          i += 2;
          const sub = extractBraced(src, i);
          const scanned = scanSource(sub.inner);
          for (const l of scanned.literals) literals.add(l);
          code += "\n" + scanned.code + "\n";
          i = sub.end;
          continue;
        }
        i++;
      }
      if (!closed) unterminated = true;
      continue;
    }
    // regex literal
    if (c === "/" && isRegStart()) {
      i++;
      let inClass = false;
      // No brace bookkeeping here: `inClass` is what decides where the literal
      // ends, and a `/` inside `{...}` cannot occur. (A `depth` counter used to be
      // incremented and decremented here without ever being read.)
      while (i < n) {
        if (src[i] === "\\") { i += 2; continue; }
        if (src[i] === "[") inClass = true;
        else if (src[i] === "]") inClass = false;
        if (src[i] === "/" && !inClass) { i++; break; }
        if (src[i] === "\n") break;
        i++;
      }
      while (i < n && /[a-z]/.test(src[i])) i++; // flags
      code += "/x/";
      continue;
    }
    if (!/\s/.test(c)) prevSig = c;
    code += c;
    i++;
  }
  return { literals, code, unterminated };
}

/* Collect flat i18n keys (dotted paths ending in a string value) from a table. */
function collectKeys(node, prefix, out) {
  for (const k of Object.keys(node)) {
    const q = prefix ? prefix + "." + k : k;
    if (typeof node[k] === "string") out.push(q);
    else if (node[k] && typeof node[k] === "object") collectKeys(node[k], q, out);
  }
  return out;
}

/** The dictionary that is the source of truth AND the fallback (see the UI's own header). */
const SOURCE_LANG = "en";

/**
 * Files under `admin-ui/` that this scan does NOT read, each with the reason it carries no `t()` call.
 * `i18n.js` is exempt by NAME (it is the dictionary); everything else must be here, so a UI module in
 * a language this scan does not understand cannot slip in unrecorded (round 189).
 */
const UNSCANNED_UI_OK = records(process.env.I18N_UNSCANNED_UI, [
  ["style.css", "a stylesheet: it carries class names and layout, never a `t()` call"],
]);

/** The source languages a UI module can be written in (see the round-189 note in `checkI18n`). */
const SCANNED_EXTENSIONS = /\.(js|mjs|cjs|jsx|ts|tsx|html)$/;
/** The dictionary itself: it DEFINES the keys, so it is not a reference site. */
const DICTIONARY_FILE = path.join("i18n.js");

function loadI18n(dir) {
  const code = fs.readFileSync(path.join(dir, "i18n.js"), "utf8");
  return new Function(
    "localStorage", "navigator", "document", "window",
    code + "\nreturn { I18N, lookup, LANGUAGES };",
  )(
    { getItem: () => null, setItem: () => {} },
    { language: "en-US", clipboard: null },
    { documentElement: { lang: "" }, querySelectorAll: () => [] },
    {},
  );
}

function loadApiDocs(dir) {
  const file = path.join(dir, "api-docs.js");
  if (!fs.existsSync(file)) return null;
  const code = fs.readFileSync(file, "utf8");
  try {
    return new Function(
      "localStorage", "navigator", "document", "window",
      code + "\nreturn { API_DOCS, apiSummaryKey };",
    )(
      { getItem: () => null, setItem: () => {} },
      { language: "en-US", clipboard: null },
      { documentElement: { lang: "" }, querySelectorAll: () => [] },
      {},
    );
  } catch {
    return null; // not parseable — skip dynamic apidocs keys
  }
}

/**
 * Run the full bidirectional i18n check against an admin-ui directory.
 * @returns {{issues: Array<{type:string, key:string, lang?:string}>, enKeys: string[]}}
 */
function checkI18n(adminUiDir) {
  const i18n = loadI18n(adminUiDir);
  const en = i18n.I18N[SOURCE_LANG];
  const enKeys = collectKeys(en, "", []);
  const enSet = new Set(enKeys);



  // Code files, RECURSIVELY: the SCANNED_EXTENSIONS below, minus the dictionary itself.
  //
  // The first version read only the top level of `admin-ui/`, so a UI module added under a
  // subdirectory would be invisible: its `t("brand.new.key")` would not be checked against
  // `I18N.en` at all and the page would render the raw key.
  //
  // Round 189 closed the NEXT hole in the same family: the rule was `*.js` + `*.html` by name, so a
  // UI module in any other source language (`.mjs`, `.cjs`, `.jsx`, `.ts`, `.tsx`) was invisible in
  // exactly the same way — a key it alone referenced was never checked against `en`. The extension
  // set is now explicit AND the directory is audited: every file under `admin-ui/` must be either
  // SCANNED or RECORDED in UNSCANNED_UI_OK with the reason it carries no `t()` call.
  const files = [];
  const uiFiles = [];
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(full);
        continue;
      }
      uiFiles.push(path.relative(adminUiDir, full));
      if (entry.name === path.basename(DICTIONARY_FILE)) continue;
      if (SCANNED_EXTENSIONS.test(entry.name)) files.push(full);
    }
  };
  walk(adminUiDir);


  const scan = scanSource(files.map((f) => fs.readFileSync(f, "utf8")).join("\n"));
  const { literals, code } = scan;

  // (2) 代码→en: t("key") / t("key", …) literals — standalone `t`, first string
  // arg, NOT concatenated (a leading string of `t("prefix" + x)` is not a key).
  const tLits = new Set();
  const tRe = /(^|[^\w$])t\(\s*(['"])([^'"]+)\2\s*[,)]/g;
  let m;
  while ((m = tRe.exec(code))) tLits.add(m[3]);

  // Dynamic t("crud."+f.fk+".nav") — resolve each `fk: "X"` field reference.
  const fkVals = new Set();
  const fkRe = /(^|[^\w$])fk:\s*(['"])([^'"]+)\2/g;
  while ((m = fkRe.exec(code))) fkVals.add(m[3]);
  const crudNav = new Set([...fkVals].map((f) => "crud." + f + ".nav"));

  // Dynamic apidocs keys (apidocs.summary.* / apidocs.tag.*) from api-docs.js.
  const apidocSummary = new Set();
  const apidocTag = new Set();
  const ad = loadApiDocs(adminUiDir);
  if (ad && Array.isArray(ad.API_DOCS) && typeof ad.apiSummaryKey === "function") {
    for (const tag of ad.API_DOCS) {
      apidocTag.add("apidocs.tag." + tag.tag);
      for (const e of tag.endpoints || []) apidocSummary.add(ad.apiSummaryKey(e));
    }
  }

  // Reference set: any string literal in code (keys passed as label:/title:/
  // placeholder:/data-i18n: etc.) plus the dynamic key producers.
  const referenced = new Set([...literals, ...apidocSummary, ...apidocTag, ...crudNav]);

  const issues = [];

  // The audit described above: a file this scan does not read must be RECORDED, and a record that no
  // longer applies (the file moved, was renamed, or became scannable) is stale. `i18n.js` is exempt by
  // name because it is the dictionary; everything else needs a reason in UNSCANNED_UI_OK.
  const { unrecorded: unrecordedUi, stale: staleUiRecords } = audit({
    records: UNSCANNED_UI_OK,
    needed: uiFiles.filter(
      (rel) => !SCANNED_EXTENSIONS.test(rel) && rel !== path.relative(adminUiDir, path.join(adminUiDir, DICTIONARY_FILE)),
    ),
    applies: (rel) =>
      uiFiles.includes(rel) && !SCANNED_EXTENSIONS.test(rel) && rel !== path.relative(adminUiDir, DICTIONARY_FILE),
  });
  for (const rel of unrecordedUi) {
    issues.push({ type: "unscanned-file", file: rel });
  }
  for (const rel of staleUiRecords) {
    issues.push({ type: "stale-unscanned-record", file: rel });
  }

  // THE LOCALE SET IS DISCOVERED (round 188). It used to be the literal `["zh", "fr", "de"]`, so a
  // language ADDED to `LANGUAGES`/`I18N` was silently unchecked while the OK line still printed
  // "4 locales" — the same "a hard-coded list eats an object" hole this session keeps finding. Three
  // relations are checked now: declared-but-no-dictionary, dictionary-nobody-can-select, and (by
  // construction) every non-source locale in `I18N` gets the key-completeness check.
  const dictLangs = Object.keys(i18n.I18N || {});
  const declaredLangs = (Array.isArray(i18n.LANGUAGES) ? i18n.LANGUAGES : dictLangs)
    .map((l) => (typeof l === "string" ? l : l && l.code))
    .filter(Boolean);
  for (const lang of declaredLangs) {
    if (!dictLangs.includes(lang)) {
      issues.push({ type: "declared-no-dictionary", lang });
    }
  }
  for (const lang of dictLangs) {
    if (!declaredLangs.includes(lang)) {
      issues.push({ type: "dictionary-not-selectable", lang });
    }
  }
  const checkedLangs = dictLangs.filter((l) => l !== SOURCE_LANG);

  // (1) source→every other locale (pre-existing, now discovered).
  for (const lang of checkedLangs) {
    for (const k of enKeys) {
      if (typeof i18n.lookup(i18n.I18N[lang], k) !== "string") {
        issues.push({ type: "locale-missing", lang, key: k });
      }
    }
  }

  // (2) 代码→en: every code-referenced key must exist in en.
  for (const k of new Set([...tLits, ...crudNav])) {
    if (!enSet.has(k)) issues.push({ type: "code-missing", key: k });
  }

  // (3) en→引用: every en key must be referenced by code (防死键).
  for (const k of enKeys) {
    if (!referenced.has(k)) issues.push({ type: "dead-key", key: k });
  }

  // `stats` is what the CLI floors: a scan that read almost nothing must not report "consistent".
  return {
    issues,
    enKeys,
    langs: [SOURCE_LANG, ...checkedLangs],
    stats: {
      files: files.length,
      tKeys: tLits.size,
      unterminated: scan.unterminated,
      // The UI-source audit's own numbers, so the success path can report what it judged (round 196):
      // these were computed and then never printed, i.e. a reader could not tell a run in which every
      // file was scanned from one that leaned on recorded exceptions.
      unscannedJudged: uiFiles.filter((rel) => !SCANNED_EXTENSIONS.test(rel)).length,
      unscannedRecorded: UNSCANNED_UI_OK.size,
    },
  };
}

/* ---- CLI ---- */
// Floors for the scan itself (see the CANNOT VERIFY branch in `main`): the shipped UI has 5 files,
// 348 en keys and >100 t() literals, so a scan that finds far fewer is broken rather than clean.
const MIN_FILES = Number(process.env.I18N_MIN_FILES ?? 3);
const MIN_EN_KEYS = Number(process.env.I18N_MIN_EN_KEYS ?? 100);
const MIN_T_KEYS = Number(process.env.I18N_MIN_T_KEYS ?? 50);

function main() {
  const argDir = process.argv[2];
  const adminUiDir = argDir
    ? path.resolve(argDir)
    : path.join(__dirname, "..", "admin-ui");

  if (!fs.existsSync(path.join(adminUiDir, "i18n.js"))) {
    console.error("error: i18n.js not found in " + adminUiDir);
    process.exit(2);
  }

  const { issues, enKeys, langs, stats } = checkI18n(adminUiDir);

  for (const iss of issues) {
    if (iss.type === "locale-missing") console.log("MISSING " + iss.lang + "  " + iss.key);
    else if (iss.type === "code-missing") console.log("CODE-REF-NO-EN  " + iss.key);
    else if (iss.type === "dead-key") console.log("DEAD-EN-KEY  " + iss.key);
    else if (iss.type === "declared-no-dictionary")
      console.log(`LANGUAGE-DECLARED-NO-DICTIONARY  ${iss.lang} is in LANGUAGES but has no I18N entry`);
    else if (iss.type === "unscanned-file")
      console.log(
        `UNSCANNED-UI-FILE  ${iss.file} is not read by this scan (its language is not in ` +
          `SCANNED_EXTENSIONS) and is not recorded in UNSCANNED_UI_OK: a t() call there is unchecked`,
      );
    else if (iss.type === "stale-unscanned-record")
      console.log(
        `STALE-UNSCANNED-RECORD  ${iss.file} is recorded in UNSCANNED_UI_OK but that no longer applies ` +
          `(the file is gone, or it is scannable now)`,
      );
    else if (iss.type === "dictionary-not-selectable")
      console.log(`DICTIONARY-NOT-SELECTABLE  I18N.${iss.lang} exists but LANGUAGES never offers it`);
  }

  // Floors for the scan itself: with no lower bound, a scan that read zero files (renamed
  // directory, broken glob) printed `OK (0 en keys …)` and exited 0 — the failure mode every other
  // guard in this repository floors.
  if (stats.unterminated) {
    console.error(
      "CANNOT VERIFY: a template literal never closes, so the scan stopped early and every t() " +
        "call after it is invisible — fix the UI file, then re-run",
    );
    process.exit(2);
  }
  if (stats.files < MIN_FILES || enKeys.length < MIN_EN_KEYS || stats.tKeys < MIN_T_KEYS) {
    console.error(
      `CANNOT VERIFY: scanned ${stats.files} UI file(s) (< ${MIN_FILES}), ${enKeys.length} en key(s) ` +
        `(< ${MIN_EN_KEYS}) and ${stats.tKeys} t() literal(s) (< ${MIN_T_KEYS}) — the scan is probably ` +
        `looking at the wrong place, so reporting "consistent" would mean nothing`,
    );
    process.exit(2);
  }

  if (issues.length === 0) {
    console.log(
      `OK  (${enKeys.length} en keys, ${langs.length} locales (${langs.join(', ')}), ` +
        `code↔en consistent; ${stats.files} UI file(s) scanned, ` +
        `${stats.unscannedJudged} file(s) deliberately unscanned and recorded ` +
        `(${[...UNSCANNED_UI_OK.keys()].join(', ') || 'none'}), ` +
        `${stats.tKeys} t() literal(s) checked)`,
    );
    process.exit(0);
  }
  console.log(`${issues.length} i18n issue(s) in ${adminUiDir}`);
  process.exit(1);
}

if (require.main === module) main();

module.exports = { checkI18n, scanSource, collectKeys };
