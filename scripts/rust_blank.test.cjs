#!/usr/bin/env node
'use strict';
/**
 * Tests for `scripts/rust_blank.cjs` — the shared Rust blanker SEVEN guards depend on
 * (`check_alert_expressions`, `check_ci_wiring`, `check_documented_defaults`, `check_documented_env`,
 * `check_documented_metrics`, `check_source_purity`, `check_tenant_error_codes`).
 *
 * Why this suite exists (round 177): the helper had NO dedicated test file — one incidental case in
 * `check_source_purity.test.cjs` (about char literals) was all the direct coverage it had, while its
 * own header documents three separately-measured bugs (`'{'` in a char literal derailing brace
 * matching, a `register_int_counter!` inside `#[cfg(test)]` being read as live, and a NESTED block
 * comment, which Rust allows and this scanner must survive). A helper that blanks the text guards reason about is exactly where a silent
 * over-blank turns a guard into a rubber stamp, so every documented lesson is pinned here, plus the
 * tree-wide invariants that make "the scan can still see the code" checkable.
 *
 * Run: node --test scripts/rust_blank.test.cjs
 */
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');

const {
  stripComments,
  stripCommentsOnly,
  stripTestItems,
  stripCommentsAndTestItems,
  isTestGate,
} = require('./rust_blank.cjs');

const visible = (t) => t.replace(/\s/g, '');

test('stripComments preserves length and line numbers (the padding contract)', () => {
  const src = '// 注释 with 中文\nlet s = "a string";\nlet c = \'x\';\n/* block */\ncode();\n';
  const out = stripComments(src);
  assert.equal(out.length, src.length, 'offsets must not move');
  assert.equal(out.split('\n').length, src.split('\n').length);
  assert.match(out, /code\(\);/, 'real code survives');
  assert.doesNotMatch(out, /a string/);
  assert.doesNotMatch(out, /注释/);
});

test("a char literal holding a brace does not derail brace matching (the measured bug)", () => {
  // Header of rust_blank.cjs: `const L: char = '{';` inside a test item made the depth never return to
  // zero, so everything after that item was blanked and a real `.unwrap()` in production was never
  // scanned — the guard printed OK.
  const src = [
    'pub fn prod() { let _ = risky().unwrap(); }',
    '#[cfg(test)]',
    'mod tests {',
    "    const L: char = '{';",
    "    const R: char = '}';",
    '    fn t() {}',
    '}',
    'pub fn after() { other().unwrap(); }',
    '',
  ].join('\n');
  const out = stripTestItems(stripComments(src));
  assert.doesNotMatch(out, /mod tests/, 'the test item is blanked');
  assert.match(out, /other\(\)\.unwrap\(\)/, 'production code AFTER the test item is still visible');
  assert.equal((out.match(/\{/g) || []).length, (out.match(/\}/g) || []).length);
});

test('escaped quotes and backslashes in char literals are handled', () => {
  const src = "const Q: char = '\\'';\nconst B: char = '\\\\';\nfn after() {}\n";
  const out = stripComments(src);
  assert.equal(out.length, src.length);
  assert.match(out, /fn after\(\) \{\}/);
});

test('raw strings are blanked, including ones whose content holds `"#`', () => {
  const src = 'let a = r#"x "y" z"#;\nlet b = r##"p "# q"##;\nfn after() {}\n';
  const out = stripComments(src);
  assert.equal(out.length, src.length);
  assert.doesNotMatch(out, /x "y" z/);
  assert.doesNotMatch(out, /p "# q/);
  assert.match(out, /fn after\(\) \{\}/);
});

test('a `//` inside a string is not a comment, and a `"` inside a comment is not a string', () => {
  const src = 'let url = "http://x/y";\n// a comment with a " quote and a \' tick\nfn after() {}\n';
  const out = stripComments(src);
  assert.equal(out.length, src.length);
  assert.match(out, /fn after\(\) \{\}/, 'the string does not swallow the next line');
  assert.doesNotMatch(out, /http/);
});

test('nested block comments are blanked as a unit', () => {
  const src = '/* outer /* inner */ still comment */\nfn after() {}\n';
  const out = stripComments(src);
  assert.equal(out.length, src.length);
  assert.match(out, /fn after\(\) \{\}/);
  assert.doesNotMatch(out, /still comment/);
});

test('stripTestItems: test gates are blanked, and anything ambiguous is KEPT (scanned)', () => {
  const blanked = (code) => !/fn gated/.test(stripTestItems(stripComments(code)));
  assert.ok(blanked('#[cfg(test)]\nmod m { fn gated() {} }\n'), 'cfg(test) is test code');
  assert.ok(
    blanked('#[cfg(all(test, feature = "x"))]\nmod m { fn gated() {} }\n'),
    'all(test, …) implies test',
  );
  assert.ok(
    !blanked('#[cfg(any(test, feature = "x"))]\nmod m { fn gated() {} }\n'),
    'any(test, …) can SHIP: it must be scanned, not hidden',
  );
  assert.ok(!blanked('#[cfg(not(test))]\nmod m { fn gated() {} }\n'), 'not(test) is production-only');
  assert.ok(
    !blanked('#[cfg(feature = "test")]\nmod m { fn gated() {} }\n'),
    'a string spelling "test" is not the test predicate',
  );
  // The `mod tests;` FORM takes no braces: the scan must stop at the `;` rather than run to the next
  // brace-matched block (which would blank unrelated code below it).
  const semicolonForm = stripTestItems(stripComments('#[cfg(test)]\nmod tests;\nfn after() {}\n'));
  assert.doesNotMatch(semicolonForm, /mod tests;/, 'the `mod tests;` form is blanked');
  assert.match(semicolonForm, /fn after\(\) \{\}/, 'and it stops at the `;`, not at the next block');
  assert.equal(isTestGate('#[cfg(test)]'), true);
  assert.equal(isTestGate('#[cfg(any(test, feature = "x"))]'), false);
});

test('stripCommentsOnly keeps literal CONTENTS (guards read names out of them)', () => {
  const src = 'register_int_counter!("hydra_named_total", "help");\n';
  assert.match(stripCommentsOnly(src), /hydra_named_total/);
  assert.doesNotMatch(stripComments(src), /hydra_named_total/, 'stripComments blanks the name');
});

test('stripCommentsAndTestItems blanks test items but keeps production literals', () => {
  const src = [
    'register_int_counter!("hydra_prod_total", "help");',
    '#[cfg(test)]',
    'mod tests {',
    '    register_int_counter!("hydra_test_only_total", "help");',
    '}',
    '',
  ].join('\n');
  const out = stripCommentsAndTestItems(src);
  assert.match(out, /hydra_prod_total/);
  assert.doesNotMatch(out, /hydra_test_only_total/);
});

test('TREE PIN: 119 repo sources survive the blanker (measured invariants)', () => {
  const root = path.join(__dirname, '..');
  const files = [];
  (function walk(dir, depth) {
    if (depth > 8) return;
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      if (['node_modules', '.git', 'target', '.acceptance', 'dist', '.cargo-cache'].includes(e.name)) continue;
      const full = path.join(dir, e.name);
      if (e.isDirectory()) walk(full, depth + 1);
      else if (e.name.endsWith('.rs')) files.push(full);
    }
  })(path.join(root, 'crates'), 0);
  assert.ok(files.length > 100, `expected the crate sources, found ${files.length}`);
  const lengthBad = [];
  const braceBad = [];
  const emptied = [];
  for (const f of files) {
    const raw = fs.readFileSync(f, 'utf8');
    const commented = stripComments(raw);
    if (commented.length !== raw.length) lengthBad.push(path.relative(root, f));
    const stripped = stripTestItems(commented);
    const open = (stripped.match(/\{/g) || []).length;
    const close = (stripped.match(/\}/g) || []).length;
    if (open !== close) braceBad.push(`${path.relative(root, f)} (${open} vs ${close})`);
    if (visible(stripped).length === 0 && visible(commented).length > 0) {
      emptied.push(path.relative(root, f));
    }
  }
  assert.deepEqual(lengthBad, [], 'offsets moved in: ' + lengthBad.join(', '));
  assert.deepEqual(braceBad, [], 'brace matching desynced in: ' + braceBad.join(', '));
  assert.deepEqual(emptied, [], 'blanked to nothing (a scan would see no code): ' + emptied.join(', '));
});

test('REAL-FILE PIN: metrics.rs keeps its production registrations and loses its test one', () => {
  const file = path.join(__dirname, '..', 'crates', 'hydra-server', 'src', 'admin', 'metrics.rs');
  const raw = fs.readFileSync(file, 'utf8');
  const stripped = stripTestItems(stripComments(raw));
  const names = [...raw.matchAll(/register_int_counter!\(\s*"([^"]+)"/g)].map((m) => m[1]);
  assert.ok(names.length >= 4, `expected the metric registrations, found ${names.length}`);
  // Measured: 5 in the raw file, 4 after stripping (the fifth lives inside `#[cfg(test)]`).
  assert.equal((stripped.match(/register_int_counter!\(/g) || []).length, names.length - 1);
  // The three PUBLIC names above are production registrations, so they survive the combination whose
  // whole point is "literals intact, test items gone" (`stripCommentsAndTestItems`) — and the one name
  // that does NOT survive it is the test-only counter.
  const combined = stripCommentsAndTestItems(raw);
  const keptNames = names.filter((n) => combined.includes(n));
  assert.equal(keptNames.length, names.length - 1, `kept ${keptNames.length} of ${names.length}`);
  assert.ok(names.slice(0, 3).every((n) => keptNames.includes(n)));
  assert.doesNotMatch(stripped, /#\[cfg\(test\)\]/, 'the test gate is gone from the stripped text');
});
