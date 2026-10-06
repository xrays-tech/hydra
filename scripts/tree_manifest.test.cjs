#!/usr/bin/env node
'use strict';
/**
 * Tests for `scripts/tree_manifest.cjs`.
 *
 * The tool exists so "this gate verdict covers the revision it claims to" stops depending on a human
 * re-reading the transcript (round 178 had to be re-run by hand for exactly that reason). Its two
 * classifications are therefore both load-bearing:
 *   * a CODE change during a run ⇒ exit 1 (the earlier entries judged a revision that is gone);
 *   * a DOCS-only change         ⇒ exit 0 with a NOTE (no code moved; the doc-dependent entries may
 *     still not match the final text).
 * A tool that cannot tell those apart, or that flips on generated files (`target/`, `.acceptance/`),
 * would make every run look unstable — so each direction has its own case.
 *
 * Run: node --test scripts/tree_manifest.test.cjs
 */
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

const SCRIPT = path.join(__dirname, 'tree_manifest.cjs');
let seq = 0;

/** A skeleton tree with one file in each classification, plus generated areas that must be ignored. */
function tree(extra = {}) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), `tm_${process.pid}_${seq++}_`));
  const files = {
    'scripts/guard.cjs': '// guard\n',
    'crates/x/src/lib.rs': 'pub fn f() {}\n',
    'docs/index.html': '<html></html>\n',
    'dev-docs/plan.md': '# plan\n',
    'target/junk.bin': 'binary\n',
    '.acceptance/scratch.log': 'log\n',
    ...extra,
  };
  for (const [rel, body] of Object.entries(files)) {
    const full = path.join(root, rel);
    fs.mkdirSync(path.dirname(full), { recursive: true });
    fs.writeFileSync(full, body);
  }
  return root;
}

function run(root, args) {
  try {
    const out = execFileSync('node', [SCRIPT, ...args], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      env: { ...process.env, TREE_MANIFEST_ROOT: root },
    });
    return { status: 0, out: out.toString() };
  } catch (e) {
    return { status: e.status === undefined ? 1 : e.status, out: (e.stdout || '') + (e.stderr || '') };
  }
}

const record = (root) => run(root, ['--write', path.join(root, '.acceptance', 'manifest.txt')]);
const verify = (root) => run(root, ['--check', path.join(root, '.acceptance', 'manifest.txt')]);
const edit = (root, rel, body) => {
  const full = path.join(root, rel);
  fs.mkdirSync(path.dirname(full), { recursive: true });
  fs.writeFileSync(full, body);
};

test('an unchanged tree verifies clean', () => {
  const root = tree();
  assert.equal(record(root).status, 0);
  const r = verify(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /the source tree is unchanged/);
});

test('a CODE change during the run is a finding that names the file', () => {
  const root = tree();
  record(root);
  edit(root, 'scripts/guard.cjs', '// changed\n');
  const r = verify(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /CHANGED modified scripts\/guard\.cjs/);
  assert.match(r.out, /does not cover the tree as it is now/);
});

test('CONTROL: a DOCS-only change is a note, not a finding', () => {
  const root = tree();
  record(root);
  edit(root, 'docs/index.html', '<html>changed</html>\n');
  edit(root, 'dev-docs/plan.md', '# plan changed\n');
  const r = verify(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /NOTE: 2 doc file\(s\) changed during the run/);
});

test('an ADDED code file is a finding, and so is a REMOVED one', () => {
  const root = tree();
  record(root);
  edit(root, 'integration/test_new.py', 'print("x")\n');
  const added = verify(root);
  assert.equal(added.status, 1, added.out);
  assert.match(added.out, /CHANGED added integration\/test_new\.py/);

  const root2 = tree();
  record(root2);
  fs.rmSync(path.join(root2, 'crates/x/src/lib.rs'));
  const removed = verify(root2);
  assert.equal(removed.status, 1, removed.out);
  assert.match(removed.out, /CHANGED removed crates\/x\/src\/lib\.rs/);
});

test('generated areas are IGNORED (a stable run stays stable)', () => {
  const root = tree();
  record(root);
  // Written during a normal run: the gate's own log, a drill's scratch, cargo output.
  fs.writeFileSync(path.join(root, 'target', 'junk.bin'), 'different binary\n');
  fs.mkdirSync(path.join(root, '.acceptance', 'some-drill'), { recursive: true });
  fs.writeFileSync(path.join(root, '.acceptance', 'some-drill', 'state.db'), 'db\n');
  fs.writeFileSync(path.join(root, 'scripts', 'run.log'), 'log\n');
  const r = verify(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /unchanged/);
});

test('a missing manifest is CANNOT VERIFY (never a pass)', () => {
  const root = tree();
  const r = verify(root);
  assert.equal(r.status, 2, r.out);
  assert.match(r.out, /CANNOT VERIFY/);
  assert.match(r.out, /the run did not record a manifest/);
});

test('a malformed manifest is CANNOT VERIFY too (it never guesses)', () => {
  const root = tree();
  record(root);
  const file = path.join(root, '.acceptance', 'manifest.txt');
  fs.writeFileSync(file, 'not a hash line\n');
  const r = verify(root);
  assert.equal(r.status, 2, r.out);
  assert.match(r.out, /malformed/);
});
