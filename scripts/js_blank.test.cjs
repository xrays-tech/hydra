#!/usr/bin/env node
'use strict';
/**
 * Tests for `scripts/js_blank.cjs` — the shared masker that blanks comments and literal CONTENTS for
 * the guards that must not be fooled by strings (`check_e2e_contracts`, `check_test_tails`).
 *
 * Why this suite exists (round 176): the masker mis-handled a **regex literal containing a backtick**
 * (`/\.build_pool\(` outside the owner/` in `scripts/check_redis_pool.test.cjs`). The backtick opened a
 * TEMPLATE literal that never closed, so
 *   * 74% of that file was blanked (measured 8231 → 2167 non-space characters),
 *   * `maskJsLiteralsReport` reported `unterminated: true` for a VALID file — and
 *     `check_e2e_contracts` turns that flag into "this file cannot be parsed as JS",
 *   * `check_test_tails` could not see the file's `process.exit(` at all: a guard blinded by its own
 *     masker, which is worse than a guard that is absent, because the OK line still printed.
 *
 * Run: node --test scripts/js_blank.test.cjs
 */
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const { maskJsLiterals, maskJsLiteralsReport } = require('./js_blank.cjs');

const visible = (text) => text.replace(/\s/g, '');

test('a regex literal containing a backtick does not swallow the rest of the file', () => {
  const src = 'const re = /a`b/;\nconst after = 1;\n';
  const { masked, unterminated } = maskJsLiteralsReport(src);
  assert.equal(unterminated, false, 'a regex with a backtick is not an unterminated template');
  assert.match(masked, /const after = 1;/);
  assert.equal(visible(masked).length, visible(src).length);
});

test('THE MEASURED CASE: the real guard suite keeps its code AND its exit visible', () => {
  const file = path.join(__dirname, 'check_redis_pool.test.cjs');
  const src = fs.readFileSync(file, 'utf8');
  const { masked, unterminated } = maskJsLiteralsReport(src);
  assert.equal(unterminated, false, `${path.basename(file)} is valid JS and must not be reported unparseable`);
  assert.match(
    masked,
    /process\.exit\(1\);/,
    'the file\'s conditional exit must be visible to a scanner (it was blanked before round 176)',
  );
  // Half of the file is two Rust fixtures in template literals, which are legitimately blanked — so the
  // floor sits well below the measured 2126/8231 and exists only to catch a runaway again.
  assert.ok(
    visible(masked).length > 1500,
    `only ${visible(masked).length} non-space character(s) survived masking (a runaway blanked the file)`,
  );
});

test('CONTROL: division is not treated as a regex literal', () => {
  const src = 'const half = a / b;\nconst after = 1;\nconst ratio = x / y / z;\n';
  const masked = maskJsLiterals(src);
  assert.match(masked, /const after = 1;/);
  assert.match(masked, /const ratio = x \/ y \/ z;/);
  assert.equal(visible(masked).length, visible(src).length);
});

test('CONTROL: a regex in a keyword position is skipped too (`return /x/`)', () => {
  const src = 'function f() { return /a`b/.test(s); }\nconst after = 2;\n';
  const { masked, unterminated } = maskJsLiteralsReport(src);
  assert.equal(unterminated, false);
  assert.match(masked, /const after = 2;/);
});

test('CONTROL: a `/` inside a character class does not end the regex early', () => {
  const src = 'const re = /[a/]`/;\nconst after = 3;\n';
  const { masked, unterminated } = maskJsLiteralsReport(src);
  assert.equal(unterminated, false);
  assert.match(masked, /const after = 3;/);
});

test('CONTROL: an unterminated template is STILL reported (the fix must not disable the alarm)', () => {
  const { masked, unterminated } = maskJsLiteralsReport('const t = `abc\ndef();\n');
  assert.equal(unterminated, true);
  assert.doesNotMatch(masked, /def\(\);/);
});

test('CONTROL: a string containing a slash is still masked as a string', () => {
  const src = 'const s = "a/b";\nconst after = 4;\n';
  const masked = maskJsLiterals(src);
  assert.match(masked, /const s = "   ";/, 'the string CONTENTS are blanked, the quotes survive');
  assert.match(masked, /const after = 4;/);
});

test('TREE PIN: no JS/TS file in the tree is mis-read as an unterminated literal', () => {
  // The alarm is only useful if it is EMPTY on a healthy tree: with the round-176 pre-fix masker this
  // scan reported `scripts/check_redis_pool.test.cjs` — a valid file whose second half (74% of its
  // non-space characters) was invisible to every consumer of this helper. A future mis-mask, or a file
  // that really cannot be parsed, lands here first.
  const skip = new Set(['node_modules', '.git', 'target', '.acceptance', 'dist', 'dist-test', 'pw-browsers']);
  const offenders = [];
  (function walk(dir, depth) {
    if (depth > 7) return;
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      if (skip.has(e.name)) continue;
      const full = path.join(dir, e.name);
      if (e.isDirectory()) walk(full, depth + 1);
      else if (/\.(cjs|js|mjs|ts)$/.test(e.name)) {
        const rel = path.relative(path.join(__dirname, '..'), full);
        if (maskJsLiteralsReport(fs.readFileSync(full, 'utf8')).unterminated) offenders.push(rel);
      }
    }
  })(path.join(__dirname, '..'), 0);
  assert.deepEqual(offenders, [], `mis-read as unterminated: ${offenders.join(', ')}`);
});
