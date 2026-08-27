#!/usr/bin/env node
/* Admin-UI i18n 词条完整性检查（2026-08-27 i18n 计划 Task 2–4 复用）。
 *
 * 加载 admin-ui/i18n.js（以 new Function 注入浏览器桩，避免 eval 严格模式
 * 作用域隔离），断言：en 中出现的每一个词条 key 在 zh/fr/de 中都必须存在；
 * 任一缺失即非零退出。用法：node scripts/check_i18n.js
 */
"use strict";
const fs = require("fs");
const path = require("path");

const file = path.join(__dirname, "..", "admin-ui", "i18n.js");
const code = fs.readFileSync(file, "utf8");

const sandbox = new Function(
  "localStorage", "navigator", "document",
  code + "\nreturn { I18N, lookup, LANGUAGES, currentLang, setLang, t };",
)(
  { getItem: () => null, setItem: () => {} },
  { language: "en-US" },
  { documentElement: { lang: "" }, querySelectorAll: () => [] },
);

const keys = [];
(function walk(node, prefix) {
  for (const k of Object.keys(node)) {
    const q = prefix ? prefix + "." + k : k;
    if (typeof node[k] === "string") keys.push(q);
    else walk(node[k], q);
  }
})(sandbox.I18N.en, "");

let missing = 0;
for (const lang of ["zh", "fr", "de"]) {
  for (const k of keys) {
    if (typeof sandbox.lookup(sandbox.I18N[lang], k) !== "string") {
      console.log("MISSING", lang, k);
      missing++;
    }
  }
}
console.log(missing === 0 ? `ALL KEYS PRESENT (${keys.length} keys × 4 locales)` : `${missing} missing`);
process.exit(missing ? 1 : 0);
