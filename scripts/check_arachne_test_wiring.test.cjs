#!/usr/bin/env node
'use strict';
/* Tests for `scripts/check_arachne_test_wiring.cjs`.
 *
 * The guard exists because of the 2026-10-10 F1 finding: `tests/arachne_adoption.rs` was added
 * without being added to either explicit `--test` list, so B1–B4 never ran in any automated entry
 * while the gate stayed GREEN. The first negative case below IS that exact bug (a fixture tree
 * whose workflow omits one arachne name), and it must be caught.
 *
 * Run: node --test scripts/check_arachne_test_wiring.test.cjs
 */
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const GUARD = path.join(__dirname, 'check_arachne_test_wiring.cjs');

/** A fixture repository root with test files, a workflow, and (optionally) a gate script. */
function fixture({ files, ciTests, gateTests }) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'atw-'));
  const tests = path.join(root, 'crates', 'hydra-server', 'tests');
  fs.mkdirSync(tests, { recursive: true });
  fs.mkdirSync(path.join(root, '.github', 'workflows'), { recursive: true });
  fs.mkdirSync(path.join(root, '.acceptance'), { recursive: true });
  for (const f of files) fs.writeFileSync(path.join(tests, `${f}.rs`), '// fixture\n');
  const cargoLine = (names) => names.map((n) => `            --test ${n}`).join(' \\\n') || '';
  fs.writeFileSync(
    path.join(root, '.github', 'workflows', 'ci.yml'),
    `name: ci\njobs:\n  check:\n    steps:\n      - name: arachne\n        run: |\n`
    + `          cargo test -p hydra-server --features server,cluster-redis,arachne \\\n`
    + `            --lib \\\n${cargoLine(ciTests)}\n`,
  );
  if (gateTests !== null) {
    fs.writeFileSync(
      path.join(root, '.acceptance', 'round10-gate.sh'),
      `#!/usr/bin/env bash\n`
      + `gate "arachne rust tests (in-process)"  cargo test -p hydra-server --features server,cluster-redis,arachne --lib `
      + gateTests.map((n) => `--test ${n}`).join(' ') + `\n`,
    );
  }
  return root;
}

function run(root) {
  return spawnSync(process.execPath, [GUARD], {
    encoding: 'utf8',
    env: { ...process.env, ATW_ROOT: root },
  });
}

test('clean: every arachne_*.rs is named in workflow and gate', () => {
  const root = fixture({
    files: ['arachne_adoption', 'arachne_store'],
    ciTests: ['arachne_adoption', 'arachne_store'],
    gateTests: ['arachne_adoption', 'arachne_store'],
  });
  const r = run(root);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /2 arachne test file\(s\)/);
});

test('F1 reverse-falsification: a test file missing from the workflow IS caught', () => {
  // The exact 2026-10-10 bug: adoption exists on disk, both lists omit it.
  const root = fixture({
    files: ['arachne_adoption', 'arachne_store'],
    ciTests: ['arachne_store'],
    gateTests: ['arachne_store'],
  });
  const r = run(root);
  assert.equal(r.status, 1, r.stderr);
  assert.match(r.stderr, /arachne_adoption\.rs is NOT named in any `--test` list of \.github\/workflows\/ci\.yml/);
  assert.match(r.stderr, /arachne_adoption\.rs is NOT named in the arachne entry of \.acceptance\/round10-gate\.sh/);
});

test('stale name: a --test token with no file IS caught', () => {
  const root = fixture({
    files: ['arachne_store'],
    ciTests: ['arachne_gone', 'arachne_store'],
    gateTests: ['arachne_store'],
  });
  const r = run(root);
  assert.equal(r.status, 1, r.stderr);
  assert.match(r.stderr, /names `--test arachne_gone` but crates\/hydra-server\/tests\/arachne_gone\.rs does not exist/);
});

test('gate drift: named in CI but forgotten by the local gate IS caught', () => {
  const root = fixture({
    files: ['arachne_adoption', 'arachne_store'],
    ciTests: ['arachne_adoption', 'arachne_store'],
    gateTests: ['arachne_store'],
  });
  const r = run(root);
  assert.equal(r.status, 1, r.stderr);
  assert.match(r.stderr, /arachne_adoption\.rs is NOT named in the arachne entry of \.acceptance\/round10-gate\.sh/);
});

test('absent gate script is a note, not a finding (fresh-checkout convention)', () => {
  // `gateTests: null` makes the fixture skip the gate script entirely — the fresh-checkout shape.
  const root = fixture({
    files: ['arachne_store'],
    ciTests: ['arachne_store'],
    gateTests: null,
  });
  const r = run(root);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /absent \(normal on a fresh checkout\)/);
});

test('cannot verify: no arachne test files at all', () => {
  const root = fixture({ files: [], ciTests: [], gateTests: [] });
  const r = run(root);
  assert.equal(r.status, 2);
  assert.match(r.stderr, /CANNOT VERIFY/);
});

test('the REAL repository is wired (this is the regression this guard holds)', () => {
  const r = spawnSync(process.execPath, [GUARD], { encoding: 'utf8' });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /ATW ok:/);
});
