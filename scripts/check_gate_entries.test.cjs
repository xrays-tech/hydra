#!/usr/bin/env node
'use strict';
/* Tests for `scripts/check_gate_entries.cjs`.
 *
 * The guard exists because the gate went RED on 2026-09-30 for a reason unrelated to the code: two
 * entries inherited the binary another entry built, and a parallel `cargo test --features server`
 * relinked it without the cluster features. So the tests must show the rule firing on that exact
 * shape — and staying quiet on the shapes it must not judge.
 *
 * Run: node --test scripts/check_gate_entries.test.cjs
 */
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const GUARD = path.join(__dirname, 'check_gate_entries.cjs');
const DOC_LINE = '#   cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra\n';

/** A fixture tree: a WORKFLOW, a gate script, and integration drills.
 *
 * The guard's subject is the tracked workflow since 2026-10-05 (it used to read only the gitignored
 * gate script, which made it FAIL IN CI while passing locally — its subject did not exist on a fresh
 * checkout). Each case below is about ONE of the two sources, so the fixture writes a CLEAN filler
 * workflow unless a case asks for a workflow of its own: otherwise every case would also report
 * whatever the other source happened to contain.
 */
function fixture({ entry, drill = DOC_LINE, tail = 'echo "GATE COMPLETE" >> "$LOG"\nexit "$overall"\n', extraEntries = '', workflow = null }) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cge-'));
  fs.mkdirSync(path.join(root, '.acceptance'), { recursive: true });
  fs.mkdirSync(path.join(root, 'integration'), { recursive: true });
  fs.mkdirSync(path.join(root, '.github', 'workflows'), { recursive: true });
  fs.writeFileSync(
    path.join(root, '.github', 'workflows', 'ci.yml'),
    workflow === null ? cleanWorkflow() : workflow,
  );
  // The head carries the LOG variable (the real script truncates and appends to it), and the default
  // tail writes the terminal marker: the guard checks that a truncated log cannot masquerade as a
  // finished run, so a fixture without a log would fail for a reason unrelated to the case.
  const head = '#!/usr/bin/env bash\ndeclare -a NAMES=()\noverall=0\n'
    + 'LOG=".acceptance/round10-gate.log"\n: > "$LOG"\n';
  const filler = Array.from({ length: 45 }, (_, i) => `gate "filler ${i}" true\n`).join('');
  fs.writeFileSync(
    path.join(root, '.acceptance', 'round10-gate.sh'),
    head + filler + extraEntries + entry + '\n' + tail,
  );
  fs.writeFileSync(path.join(root, 'integration', 'test_doc.py'), drill + 'print("ok")\n');
  fs.writeFileSync(path.join(root, 'integration', 'test_plain.py'), 'print("ok")\n');
  return root;
}

/** A workflow with enough clean steps for the entry floor, and no drill in any of them. */
function cleanWorkflow(extra = '') {
  const filler = Array.from({ length: 45 }, (_, i) =>
    `      - name: filler ${i}\n        run: true\n`).join('');
  return `name: ci\non: [push]\njobs:\n  check:\n    steps:\n${filler}${extra}`;
}

function run(root, env = {}) {
  const r = spawnSync(process.execPath, [GUARD], {
    encoding: 'utf8',
    // A fixture judges one or two preconditions, so the JUDGED floor is neutralised here (it has its
    // own dedicated case below) — the same discipline the other guard suites use, and the reason
    // this suite first reported "expected drift, got CANNOT VERIFY" instead of the drift itself.
    // The BINARY-DEPENDENCY floor is neutralised for the same reason, also with its own case.
    env: { ...process.env, CGE_ROOT: root, CGE_MIN_JUDGED: '0', CGE_MIN_BINARY_DEPS: '0', ...env },
  });
  return { status: r.status, out: `${r.stdout || ''}${r.stderr || ''}` };
}

test('an entry that runs a documenting drill WITHOUT building the binary is drift', () => {
  const root = fixture({ entry: 'gate "cluster thing" python3 integration/test_doc.py' });
  const r = run(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /cluster thing/);
  assert.match(r.out, /does not build it/);
  assert.match(r.out, /test_doc\.py/);
});

test('CONTROL: the same entry WITH the build passes', () => {
  const root = fixture({
    entry: 'gate "cluster thing" bash -c \'cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra >/dev/null && python3 integration/test_doc.py\'',
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /1 drill\(s\) DOCUMENT a build/);
});

test('CONTROL: a drill that documents no build is not judged (the rule does not overreach)', () => {
  const root = fixture({ entry: 'gate "plain thing" python3 integration/test_plain.py' });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /0 drill\(s\) DOCUMENT a build/);
});

test('a gate script that prints RED but does not RETURN its verdict is drift', () => {
  const root = fixture({
    entry: 'gate "cluster thing" python3 integration/test_doc.py',
    tail: 'echo "OVERALL=RED"\n',
  });
  const r = run(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /does not end by RETURNING its verdict/);
});

test('the judged floor refuses to pass when the rule reaches almost nothing', () => {
  const root = fixture({
    entry: 'gate "cluster thing" bash -c \'cargo build -p hydra-server --features server --bin hydra >/dev/null && python3 integration/test_doc.py\'',
  });
  const r = run(root, { CGE_MIN_JUDGED: '9' });
  assert.equal(r.status, 2, `status=${r.status} ${r.out}`);
  assert.match(r.out, /CANNOT VERIFY/);
  assert.match(r.out, /documented-build precondition/);
});

test('a workflow too short to be the real one is CANNOT VERIFY, never a pass', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cge-small-'));
  fs.mkdirSync(path.join(root, 'integration'), { recursive: true });
  fs.mkdirSync(path.join(root, '.github', 'workflows'), { recursive: true });
  fs.writeFileSync(path.join(root, 'integration', 'test_doc.py'), DOC_LINE);
  fs.writeFileSync(path.join(root, '.github', 'workflows', 'ci.yml'),
    'name: ci\non: [push]\njobs:\n  check:\n    steps:\n      - name: one\n        run: true\n');
  const r = run(root);
  assert.equal(r.status, 2, `status=${r.status} ${r.out}`);
  assert.match(r.out, /only 1 workflow step\(s\) with a `run:` block parsed/);
});

test('a missing WORKFLOW is CANNOT VERIFY (it is the subject, and it is tracked)', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cge-none-'));
  fs.mkdirSync(path.join(root, 'integration'), { recursive: true });
  fs.writeFileSync(path.join(root, 'integration', 'test_plain.py'), 'print("ok")\n');
  const r = run(root);
  assert.equal(r.status, 2, `status=${r.status} ${r.out}`);
  assert.match(r.out, /CANNOT VERIFY: cannot read \.github\/workflows\/ci\.yml/);
});

test('a missing LOCAL gate script is a NOTE, not a failure (it is gitignored — the CI condition)', () => {
  // This is the case that made the guard red in CI for as long as it existed: `.acceptance/` is
  // gitignored, so on a fresh checkout the old subject was simply absent. A guard that refuses to
  // judge is fine; one whose subject CANNOT exist where it runs is not.
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cge-nogate-'));
  fs.mkdirSync(path.join(root, 'integration'), { recursive: true });
  fs.mkdirSync(path.join(root, '.github', 'workflows'), { recursive: true });
  fs.writeFileSync(path.join(root, 'integration', 'test_plain.py'), 'print("ok")\n');
  fs.writeFileSync(path.join(root, '.github', 'workflows', 'ci.yml'), cleanWorkflow());
  const r = run(root);
  assert.equal(r.status, 0, `status=${r.status} ${r.out}`);
  assert.match(r.out, /the local gate script is not present \(gitignored\)/);
});

/* Round 154: a drill that STARTS the prebuilt binary must run against a binary this gate run built.
 * Measured 2026-09-30: 38 of the 87 entries ran such a drill, only 9 built the binary themselves, and
 * 9 more ran BEFORE the gate's first build — inheriting whatever the last cargo command had left
 * (feature-poor in the round-143 incident, absent on a fresh checkout). CI builds the binary up front
 * for exactly this drill group, and the gate now does too. */
const BIN_DRILL = 'import os\nBIN = os.path.join(ROOT, "target", "debug", "hydra")\n';

test('an entry running a binary-starting drill with no earlier build is drift', () => {
  const root = fixture({ entry: 'gate "tenant thing" python3 integration/test_doc.py', drill: BIN_DRILL });
  const r = run(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /starts `target\/debug\/hydra`/);
  assert.match(r.out, /no entry before it in its gate run builds that binary/);
});

test('CONTROL: an earlier entry that builds the binary is enough', () => {
  const root = fixture({
    entry: 'gate "tenant thing" python3 integration/test_doc.py',
    drill: BIN_DRILL,
    extraEntries: 'gate "build the binary under test" cargo build -p hydra-server --features server --bin hydra\n',
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /1 step\(s\) run a drill that starts the prebuilt binary/);
});

test('CONTROL: an entry that builds the binary itself is enough', () => {
  const root = fixture({
    entry: 'gate "tenant thing" bash -c \'cargo build -p hydra-server --features server --bin hydra >/dev/null && python3 integration/test_doc.py\'',
    drill: BIN_DRILL,
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
});

test('CONTROL: a drill that does NOT start the prebuilt binary is not judged', () => {
  // A plain program: it neither documents a build nor starts the binary, so NEITHER rule may judge it.
  const root = fixture({ entry: 'gate "plain" python3 integration/test_doc.py', drill: 'print("ok")\n' });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /0 step\(s\) run a drill that starts the prebuilt binary/);
});

test('the binary-dependency floor refuses to pass when the rule reaches nothing', () => {
  const root = fixture({
    entry: 'gate "tenant thing" bash -c \'cargo build -p hydra-server --features server --bin hydra >/dev/null && python3 integration/test_doc.py\'',
    drill: BIN_DRILL,
  });
  const r = run(root, { CGE_MIN_BINARY_DEPS: '9' });
  assert.equal(r.status, 2, `status=${r.status} ${r.out}`);
  assert.match(r.out, /starts the prebuilt binary \(< 9\)/);
});

/* Round 168: the OK line must PRINT the self-build/inherit split. Measured 2026-10-01 on the real
 * gate: 38 entries run a drill that starts the prebuilt binary, 9 build it themselves and 29 trust an
 * earlier entry. The old wording ("each either building it in its own entry or preceded by one that
 * does") was true but hid the 29 — the number a reader needs in order to judge how much of the gate
 * rests on entry ORDER. These tests pin the printed split, not a floor (see the guard header). */
test('the OK line reports the self-build/inherit split (inheriting entry)', () => {
  const root = fixture({
    entry: 'gate "tenant thing" python3 integration/test_doc.py',
    drill: BIN_DRILL,
    extraEntries: 'gate "build the binary under test" cargo build -p hydra-server --features server --bin hydra\n',
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /SELF-BUILD 0 \/ INHERITED-IN-JOB 1/);
});

test('the OK line reports the self-build/inherit split (self-building entry)', () => {
  const root = fixture({
    entry: 'gate "tenant thing" bash -c \'cargo build -p hydra-server --features server --bin hydra >/dev/null && python3 integration/test_doc.py\'',
    drill: BIN_DRILL,
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /SELF-BUILD 1 \/ INHERITED-IN-JOB 0/);
});

test('CONTROL: the split counts both shapes in one run', () => {
  // `extraEntries` are written BEFORE `entry`, so the self-building reader must be the extra one and
  // the plain reader the main one — the other order is rule-3 drift (its own case above) and would
  // make this control fail for a reason that has nothing to do with the split it pins.
  const root = fixture({
    entry: 'gate "tenant second" python3 integration/test_doc.py',
    drill: BIN_DRILL,
    extraEntries: 'gate "tenant thing" bash -c \'cargo build -p hydra-server --features server --bin hydra >/dev/null && python3 integration/test_doc.py\'\n',
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /SELF-BUILD 1 \/ INHERITED-IN-JOB 1/);
});

/* 2026-10-05 — WHY THE SUBJECT CHANGED. The guard used to read only `.acceptance/round10-gate.sh`,
 * which is gitignored and was never tracked, so in CI its subject did not exist: it returned CANNOT
 * VERIFY and the `scripts` job ran RED for a reason unrelated to the code (measured: `CGE_ROOT=<empty>
 * node scripts/check_gate_entries.cjs` → exit 2). These two cases pin the redesign from both sides. */
test('the WORKFLOW is judged (a step that runs a documenting drill without building it is drift)', () => {
  const root = fixture({
    entry: 'gate "unrelated" true',
    workflow: cleanWorkflow(
      '      - name: cluster limits (no build)\n' +
      '        run: python3 integration/test_doc.py\n'),
  });
  const r = run(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /"cluster limits \(no build\)" .*does not build it/s);
});

test('the CI condition passes: workflow present, gitignored gate script ABSENT', () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cge-ci-'));
  fs.mkdirSync(path.join(root, 'integration'), { recursive: true });
  fs.mkdirSync(path.join(root, '.github', 'workflows'), { recursive: true });
  fs.writeFileSync(path.join(root, 'integration', 'test_doc.py'), DOC_LINE + 'print("ok")\n');
  fs.writeFileSync(path.join(root, '.github', 'workflows', 'ci.yml'), cleanWorkflow(
    '      - name: builds and runs\n' +
    '        run: cargo build -p hydra-server --features server --bin hydra && python3 integration/test_doc.py\n'));
  const r = run(root);
  assert.equal(r.status, 0, `status=${r.status} ${r.out}`);
  assert.match(r.out, /1 drill\(s\) DOCUMENT a build/);
  assert.match(r.out, /the local gate script is not present/);
});

/* Round 162: a BUILD terminated by `;` instead of `&&` swallows its failure, and the rest of the entry
 * then runs against a STALE artefact. Measured on the four SDK/CLI entries: with a failing `npm` shim
 * the `;` chain continued (`exit=2`, drill still reached) while the `&&` chain stopped. */
test('a build terminated by `;` is drift (its failure would be ignored)', () => {
  const root = fixture({
    entry: "gate \"ts sdk\" bash -c 'cd tools/hydra-ts && npm run build >/dev/null 2>&1; cd ../.. && python3 integration/test_doc.py'",
    drill: 'print("ok")\n',
  });
  const r = run(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /terminated by `;` instead of `&&`/);
});

test('CONTROL: the same entry with `&&` passes', () => {
  const root = fixture({
    entry: "gate \"ts sdk\" bash -c 'cd tools/hydra-ts && npm run build >/dev/null && cd ../.. && python3 integration/test_doc.py'",
    drill: 'print("ok")\n',
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
});

/* Round 163: the LOG must carry a terminal marker. It is truncated at the start of every run, so a log
 * that merely STOPS (killed, terminal gone, out of disk) used to be indistinguishable from one that
 * finished GREEN — the verdict could not be audited after the fact. */
test('a gate script whose log has no terminal marker is drift', () => {
  const root = fixture({
    entry: 'gate "plain" python3 integration/test_doc.py',
    drill: 'print("ok")\n',
    tail: 'exit "$overall"\n',
  });
  const r = run(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /terminal marker into its LOG/);
});

test('CONTROL: the same script WITH the marker passes', () => {
  const root = fixture({
    entry: 'gate "plain" python3 integration/test_doc.py',
    drill: 'print("ok")\n',
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
});

/* Round 178: the parser must not silently EAT an entry. A line that calls `gate` in a shape the regex
 * does not accept (a single-quoted name, a leading space) still RUNS in bash while every rule in this
 * guard ignores it — including the "build the binary you run" rule that exists because of the round-143
 * RED gate. Measured on the real script: 92 entries and 0 such lines, i.e. a LATENT hole worth pinning. */
test('a single-quoted entry is reported, not silently skipped', () => {
  // A PLAIN drill: the only rule that may fire here is the completeness one (a `DOC_LINE` drill would
  // also trip rule 1, and a case that fails for two reasons proves neither).
  const root = fixture({
    entry: "gate 'tenant thing' python3 integration/test_doc.py",
    drill: 'print("ok")\n',
  });
  const r = run(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /looks like an entry/);
  assert.match(r.out, /parser did not accept it/);
});

test('a leading-space entry is reported too', () => {
  const root = fixture({
    entry: ' gate "tenant thing" python3 integration/test_doc.py',
    drill: 'print("ok")\n',
  });
  const r = run(root);
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /looks like an entry/);
});

test('CONTROL: a column-zero double-quoted entry is parsed (no completeness finding)', () => {
  const root = fixture({
    entry: 'gate "tenant thing" python3 integration/test_doc.py',
    drill: 'print("ok")\n',
  });
  const r = run(root);
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /looks like a gate entry/);
});
