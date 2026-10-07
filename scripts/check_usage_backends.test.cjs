#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/check_usage_backends.cjs.
 *
 * The guard's whole value is that a backend's declaration cannot fall out of step with the four
 * places it touches (cargo feature, runbook env rows, its recognised knobs, the retired list), so
 * every one of those rules is pinned here with a fixture that violates it — and with the CONTROL
 * case, because a guard that fails everything proves nothing.
 *
 * The two failure modes this suite exists for, both measured in this repository before:
 *   * a guard that "passes" by parsing nothing (hence the floor, and the CANNOT VERIFY exit 2);
 *   * a record that keeps excusing something that has since been fixed (the first run of this
 *     guard found `HYDRA_CLICKHOUSE_IO_TIMEOUT_MS` documented nowhere, and the exception list was
 *     emptied rather than left in place).
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CHECKER = path.join(__dirname, 'check_usage_backends.cjs');
const REPO = path.resolve(__dirname, '..');

function fixture({ registry, backends = {}, features = ['db', 'usage-clickhouse'], docs = defaultDocs() } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'usagebe-'));
  const reg = path.join(dir, 'usage', 'mod.rs');
  fs.mkdirSync(path.dirname(reg), { recursive: true });
  fs.writeFileSync(reg, registry ?? defaultRegistry());
  const bdir = path.join(dir, 'usage', 'backends');
  for (const [name, source] of Object.entries(backends)) {
    fs.mkdirSync(path.join(bdir, name), { recursive: true });
    fs.writeFileSync(path.join(bdir, name, 'mod.rs'), source);
  }
  const cargo = path.join(dir, 'Cargo.toml');
  fs.writeFileSync(cargo, `[features]\n${features.map((f) => `${f} = []`).join('\n')}\n`);
  const docPath = path.join(dir, 'ops.md');
  fs.writeFileSync(docPath, docs);
  return { dir, registry: reg, backendsDir: bdir, cargo, docs: docPath };
}

function defaultDocs() {
  return [
    '# Runbook',
    '',
    '| Variable | Default | Notes |',
    '|---|---|---|',
    '| `HYDRA_CLICKHOUSE_URL` | *(unset)* | endpoint. |',
    '| `HYDRA_TDENGINE_URL` | *(unset)* | endpoint. |',
    '| `HYDRA_TDENGINE_TOKEN` | *(unset)* | token. |',
    '',
  ].join('\n');
}

function defaultRegistry(modules = ['clickhouse']) {
  return [
    'pub static REGISTRY: &[&UsageBackend] = &[',
    ...modules.map((m) => `    &backends::${m}::DESCRIPTOR,`),
    '];',
    '',
  ].join('\n');
}

function backend({ kind = 'clickhouse', feature = 'usage-clickhouse', requires = '', recognises = '' } = {}) {
  return [
    'pub static DESCRIPTOR: UsageBackend = UsageBackend {',
    `    kind: "${kind}",`,
    `    feature: "${feature}",`,
    `    requires: &[${requires}],`,
    `    recognises: &[${recognises}],`,
    '    reads: ReaderContract::SameBackend,',
    '    open,',
    '    notes: "a backend for the test",',
    '};',
    '',
  ].join('\n');
}

function run(fx, extraEnv = {}) {
  return spawnSync(
    process.execPath,
    [
      CHECKER,
      `--registry=${fx.registry}`,
      `--backends-dir=${fx.backendsDir}`,
      `--cargo=${fx.cargo}`,
      `--docs=${fx.docs}`,
    ],
    { encoding: 'utf8', env: { ...process.env, ...extraEnv } },
  );
}

test('a declared backend that agrees with its feature and its env rows passes', () => {
  const fx = fixture({
    backends: {
      clickhouse: backend({
        requires: 'Requirement { name: CLICKHOUSE_URL_ENV, purpose: "the endpoint" }',
        recognises: '"HYDRA_CLICKHOUSE_IO_TIMEOUT_MS"',
      }),
    },
    docs: `${defaultDocs()}| \`HYDRA_CLICKHOUSE_IO_TIMEOUT_MS\` | \`15000\` | write deadline. |\n`,
  });
  fs.writeFileSync(
    path.join(fx.backendsDir, 'clickhouse', 'mod.rs'),
    `const CLICKHOUSE_URL_ENV: &str = "HYDRA_CLICKHOUSE_URL";\n${backend({
      requires: 'Requirement { name: CLICKHOUSE_URL_ENV, purpose: "the endpoint" }',
      recognises: '"HYDRA_CLICKHOUSE_IO_TIMEOUT_MS"',
    })}`,
  );
  const r = run(fx);
  assert.equal(r.status, 0, r.stderr + r.stdout);
  assert.match(r.stdout, /OK: 1 usage backend/);
});

test('a feature that does not exist in the crate manifest is a failure', () => {
  const fx = fixture({ backends: { clickhouse: backend({ feature: 'usage-clickhose' }) } });
  const r = run(fx);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /usage-clickhose/);
  assert.match(r.stderr, /not a feature of crates\/hydra-server\/Cargo\.toml/);
});

test('a REQUIRED variable the runbook does not document is a failure', () => {
  const fx = fixture({
    backends: { clickhouse: backend({ requires: 'Requirement { name: "HYDRA_NOT_IN_RUNBOOK", purpose: "x" }' }) },
  });
  const r = run(fx);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /HYDRA_NOT_IN_RUNBOOK/);
  assert.match(r.stderr, /does not document/);
});

test('a RECOGNISED variable documented nowhere is a failure, unless it is recorded with a reason', () => {
  const fx = fixture({ backends: { clickhouse: backend({ recognises: '"HYDRA_UNDOCUMENTED_KNOB"' }) } });
  const r = run(fx);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /HYDRA_UNDOCUMENTED_KNOB/);

  // The recorded-exception path, which is how a knob with a default can legitimately stay out of
  // the runbook — and it is PRINTED, so the record cannot be forgotten.
  const fx2 = fixture({ backends: { clickhouse: backend({ recognises: '"HYDRA_UNDOCUMENTED_KNOB"' }) } });
  const r2 = run(fx2, { CUB_UNDOCUMENTED_OK: JSON.stringify({ HYDRA_UNDOCUMENTED_KNOB: 'a test reason' }) });
  assert.equal(r2.status, 0, r2.stderr);
  assert.match(r2.stdout, /NOTE clickhouse: `HYDRA_UNDOCUMENTED_KNOB` is undocumented on purpose — a test reason/);
});

test('a retired value that is selectable again is a failure', () => {
  const registry = [
    'pub static RETIRED_USAGE_SINKS: &[&str] = &["sqlite"];',
    defaultRegistry(['clickhouse', 'sqlite']),
  ].join('\n');
  const fx = fixture({
    registry,
    backends: {
      clickhouse: backend(),
      sqlite: backend({ kind: 'sqlite', feature: 'db' }),
    },
  });
  const r = run(fx);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /`sqlite` is in RETIRED_USAGE_SINKS and in REGISTRY/);
});

test('two backends sharing a kind is a failure (the lookup order would decide)', () => {
  const fx = fixture({
    registry: defaultRegistry(['a', 'b']),
    backends: { a: backend({ kind: 'same' }), b: backend({ kind: 'same' }) },
  });
  const r = run(fx);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /two backends share a kind/);
});

test('an unreadable registry is CANNOT VERIFY (exit 2), never a pass', () => {
  const fx = fixture();
  fs.writeFileSync(fx.registry, '// REGISTRY was reworded away\n');
  const r = run(fx);
  assert.equal(r.status, 2);
  assert.match(r.stderr, /CANNOT VERIFY/);
});

test('finding no backend is CANNOT VERIFY (exit 2) — a guard must not pass by finding nothing', () => {
  const fx = fixture({ registry: 'pub static REGISTRY: &[&UsageBackend] = &[];\n', backends: {} });
  const r = run(fx);
  assert.equal(r.status, 2);
  assert.match(r.stderr, /refusing to pass by finding nothing/);
});

test('a backend module whose descriptor the guard cannot read is a failure, not a skip', () => {
  const fx = fixture({ backends: { clickhouse: 'pub static DESCRIPTOR: UsageBackend = todo!();\n' } });
  const r = run(fx);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no `kind`\/`feature`/);
});

test('a backend written as a flat <kind>.rs is a structural failure, not a silent accept', () => {
  const fx = fixture({ backends: {} });
  fs.mkdirSync(fx.backendsDir, { recursive: true });
  fs.writeFileSync(path.join(fx.backendsDir, 'clickhouse.rs'), backend());
  const r = run(fx);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /flat `<kind>\.rs` is a second layout/);
});

test('a literal backend kind outside src/usage is a failure (the T4.3 anti-cheat rule)', () => {
  const fx = fixture({
    backends: { clickhouse: backend() },
    registry: defaultRegistry(['clickhouse']),
  });
  // A `src/` tree with one file that knows the backend by name, exactly like the overstep that
  // passed every guard before this rule existed.
  const src = path.join(fx.dir, 'crates', 'hydra-server', 'src');
  fs.mkdirSync(path.join(src, 'usage'), { recursive: true });
  fs.writeFileSync(path.join(src, 'main.rs'), 'if sink_kind == "clickhouse" { }\n');
  fs.mkdirSync(path.join(src, 'usage', 'backends'), { recursive: true });
  const r = spawnSync(
    process.execPath,
    [
      CHECKER,
      `--registry=${fx.registry}`,
      `--backends-dir=${fx.backendsDir}`,
      `--cargo=${fx.cargo}`,
      `--docs=${fx.docs}`,
      `--root=${fx.dir}`,
    ],
    { encoding: 'utf8', env: { ...process.env } },
  );
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /names the backend `clickhouse` as a literal/);
  assert.match(r.stderr, /T4\.3 anti-cheat rule/);
});

test('the real tree passes, and names every backend', () => {
  const r = spawnSync(process.execPath, [CHECKER], { encoding: 'utf8', cwd: REPO });
  assert.equal(r.status, 0, r.stderr + r.stdout);
  assert.match(r.stdout, /clickhouse, none, tdengine/);
});
