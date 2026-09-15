#!/usr/bin/env node
/* Admin-UI i18n 词条完整性检查（2026-08-27 i18n 计划 Task 2–4 复用）。
 *
 * 加载 admin-ui/i18n.js（以 new Function 注入浏览器桩，避免 eval 严格模式
 * 作用域隔离），做四向校验，任一违规即非零退出：
 *   1. en→zh/fr/de   现有：en 中每个 key 在 zh/fr/de 都必须存在；
 *   2. 代码→en       新增：代码里 t("...") 字面量（含动态 t("crud."+f.fk+".nav")）
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
const path = require("path");

/* ---------------------------------------------------------------------------
 * Robust JS string-literal scanner.
 * Tracks: double/single-quoted strings, template literals, line/block comments,
 * and regex literals (distinguished from division by the preceding token).
 * Returns: { literals: Set<string>, code: string } where `code` is the source
 * with comments removed (strings preserved) — used for t()/fk regexes.
 * ------------------------------------------------------------------------- */
function scanSource(src) {
  const literals = new Set();
  let code = "";
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
    // template literal (keys are never built from templates; skip contents)
    if (c === "`") {
      i++;
      while (i < n) {
        if (src[i] === "\\") { i += 2; continue; }
        if (src[i] === "`") { i++; break; }
        i++;
      }
      code += "``";
      continue;
    }
    // regex literal
    if (c === "/" && isRegStart()) {
      i++;
      let inClass = false;
      let depth = 0;
      while (i < n) {
        if (src[i] === "\\") { i += 2; continue; }
        if (src[i] === "[") inClass = true;
        else if (src[i] === "]") inClass = false;
        if (src[i] === "{") depth++;
        else if (src[i] === "}") depth = Math.max(0, depth - 1);
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
  return { literals, code };
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

function loadI18n(dir) {
  const code = fs.readFileSync(path.join(dir, "i18n.js"), "utf8");
  return new Function(
    "localStorage", "navigator", "document", "window",
    code + "\nreturn { I18N, lookup };",
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
  const en = i18n.I18N.en;
  const enKeys = collectKeys(en, "", []);
  const enSet = new Set(enKeys);

  // Code files: every *.js except i18n.js (the definition) + every *.html.
  const files = fs.readdirSync(adminUiDir)
    .filter((f) => (f.endsWith(".js") && f !== "i18n.js") || f.endsWith(".html"))
    .map((f) => fs.readFileSync(path.join(adminUiDir, f), "utf8"));
  const { literals, code } = scanSource(files.join("\n"));

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

  // (1) en→zh/fr/de (pre-existing).
  for (const lang of ["zh", "fr", "de"]) {
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

  return { issues, enKeys };
}

/* ---- CLI ---- */
function main() {
  const argDir = process.argv[2];
  const adminUiDir = argDir
    ? path.resolve(argDir)
    : path.join(__dirname, "..", "admin-ui");

  if (!fs.existsSync(path.join(adminUiDir, "i18n.js"))) {
    console.error("error: i18n.js not found in " + adminUiDir);
    process.exit(2);
  }

  const { issues, enKeys } = checkI18n(adminUiDir);

  for (const iss of issues) {
    if (iss.type === "locale-missing") console.log("MISSING " + iss.lang + "  " + iss.key);
    else if (iss.type === "code-missing") console.log("CODE-REF-NO-EN  " + iss.key);
    else if (iss.type === "dead-key") console.log("DEAD-EN-KEY  " + iss.key);
  }

  if (issues.length === 0) {
    console.log(`OK  (${enKeys.length} en keys, 4 locales, code↔en consistent)`);
    process.exit(0);
  }
  console.log(`${issues.length} i18n issue(s) in ${adminUiDir}`);
  process.exit(1);
}

if (require.main === module) main();

module.exports = { checkI18n, scanSource, collectKeys };
