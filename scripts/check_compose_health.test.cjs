#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/check_compose_health.cjs.
 *
 * The checker exists because the official cluster topology shipped three hydra
 * services with NO healthcheck, and because the probe PATH is role-dependent: an edge
 * answers `/healthz` (200, token-free) but returns **404** for `/api/v1/health`
 * (measured 2026-09-29), so a control-node probe copied onto an edge would report every
 * healthy edge unhealthy. These tests pin all three failure modes plus the floors.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CHECKER = path.join(__dirname, 'check_compose_health.cjs');
const REPO = path.resolve(__dirname, '..');

const CONTROL_HC = {
  test: ['CMD-SHELL', 'curl -fsS -H "Authorization: Bearer ${HYDRA_ADMIN_TOKEN}" http://127.0.0.1:8081/api/v1/health || exit 1'],
  interval: '10s',
};
const EDGE_HC = { test: ['CMD-SHELL', 'curl -fsS http://127.0.0.1:8081/healthz || exit 1'] };

function svc(image, role, healthcheck) {
  const out = { image, environment: {} };
  if (role) out.environment.HYDRA_ROLE = role;
  if (healthcheck !== undefined) out.healthcheck = healthcheck;
  return out;
}

function fixture(doc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'compose-health-'));
  const file = path.join(dir, 'rendered.json');
  fs.writeFileSync(file, JSON.stringify(doc));
  return file;
}

function run(docs, env = {}) {
  const args = docs.map((d) => `--compose-json=${d}`);
  const res = spawnSync(process.execPath, [CHECKER, ...args], {
    encoding: 'utf8',
    env: { ...process.env, COMPOSE_HEALTH_MIN_SERVICES: '1', ...env },
  });
  return { status: res.status, stdout: res.stdout || '', stderr: res.stderr || '' };
}

test('control nodes with the token and edges on /healthz pass', () => {
  const r = run([fixture({ services: {
    'hydra-control-a': svc('hydra:latest', 'leader', CONTROL_HC),
    'hydra-edge': svc('hydra:latest', 'edge', EDGE_HC),
    'hydra': svc('hydra:latest', undefined, CONTROL_HC),
    'redis': { image: 'redis:7-alpine' },
  } })]);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /3 hydra service\(s\) checked: OK/);
});

test('the defect that motivated the checker: a hydra service with NO healthcheck', () => {
  const r = run([fixture({ services: { 'hydra-control-a': svc('hydra:latest', 'leader', undefined) } })]);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no healthcheck \(nothing supervises this node\)/);
});

test('an edge probed through /api/v1/health fails (that path is 404 on an edge)', () => {
  const r = run([fixture({ services: { 'hydra-edge': svc('hydra:latest', 'edge', CONTROL_HC) } })]);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /\/api\/v1\/health is 404 on an edge/);
});

test('an edge with no /healthz or /readyz probe fails', () => {
  const odd = { test: ['CMD-SHELL', 'curl -fsS http://127.0.0.1:8081/ || exit 1'] };
  const r = run([fixture({ services: { 'hydra-edge': svc('hydra:latest', 'edge', odd) } })]);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /must be probed through a token-free path/);
});

test('a control probe without the admin token fails (it would 401 forever)', () => {
  const noToken = { test: ['CMD-SHELL', 'curl -fsS http://127.0.0.1:8081/api/v1/health'] };
  const r = run([fixture({ services: { 'hydra-control-a': svc('hydra:latest', 'leader', noToken) } })]);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /answers 401 without the admin token/);
});

test('a hydra-local image (the local stack) is recognised too', () => {
  const r = run([fixture({ services: { 'hydra-c': svc('hydra-local:latest', 'edge', EDGE_HC) } })]);
  assert.equal(r.status, 0, r.stderr);
});

test('finding no hydra services is exit 2, never a silent pass', () => {
  const r = run([fixture({ services: { redis: { image: 'redis:7-alpine' } } })], { COMPOSE_HEALTH_MIN_SERVICES: '4' });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /CANNOT VERIFY: only 0 hydra-image service\(s\) found/);
});

test('unknown arguments are rejected and --help works', () => {
  const bad = spawnSync(process.execPath, [CHECKER, '--wat'], { encoding: 'utf8' });
  assert.equal(bad.status, 2);
  assert.match(bad.stderr, /unknown argument: --wat/);
  const help = spawnSync(process.execPath, [CHECKER, '--help'], { encoding: 'utf8' });
  assert.equal(help.status, 0);
  assert.match(help.stdout, /check_compose_health/);
});

/**
 * Round 121: the static path for a SKIPPED file (see `compose_static.cjs`), driven by
 * `--static-file` so no docker is involved.
 */
function fixtureWithoutHealthcheck() {
  const file = path.join(REPO, 'environment', 'docker-compose.local.yml');
  const lines = fs.readFileSync(file, 'utf8').split('\n');
  const start = lines.indexOf('  hydra-c:');
  assert.ok(start > 0, 'fixture lost hydra-c');
  let end = start + 1;
  while (end < lines.length && !(lines[end].startsWith('  ') && !lines[end].startsWith('    ') && lines[end].trim() !== '')) end += 1;
  const hc = lines.findIndex((l, i) => i > start && i < end && l.trim() === 'healthcheck:');
  assert.ok(hc > start, 'hydra-c has no healthcheck to remove');
  let j = hc + 1;
  while (j < end && (lines[j].startsWith('      ') || lines[j].trim() === '')) j += 1;
  const p = path.join(os.tmpdir(), `health-static-${process.pid}.yml`);
  fs.writeFileSync(p, [...lines.slice(0, hc), ...lines.slice(j)].join('\n'));
  return p;
}

test('a skipped file is checked STATICALLY: an unsupervised hydra service still fails', () => {
  const bad = fixtureWithoutHealthcheck();
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${bad}`], { encoding: 'utf8' });
  assert.notEqual(res.status, 0, `a static check found no problem:\n${res.stdout}`);
  assert.match(res.stderr, /hydra-c/);
  assert.match(res.stderr, /no healthcheck/);
  fs.rmSync(bad, { force: true });
});

test('CONTROL: the untouched local stack passes the static check', () => {
  const res = spawnSync(process.execPath, [
    CHECKER,
    `--static-file=${path.join(REPO, 'environment', 'docker-compose.local.yml')}`,
  ], { encoding: 'utf8' });
  assert.equal(res.status, 0, res.stderr);
  assert.match(res.stdout, /hydra-c \(role=edge\)/);
});

test('the static path never passes by finding nothing', () => {
  const empty = path.join(os.tmpdir(), `health-static-empty-${process.pid}.yml`);
  fs.writeFileSync(empty, 'services:\n  redis:\n    image: redis:7\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${empty}`], { encoding: 'utf8' });
  assert.equal(res.status, 2, `status=${res.status} ${res.stdout}`);
  assert.match(res.stderr, /found no hydra-image service/);
  fs.rmSync(empty, { force: true });
});

test('all shipped compose files pass (real docker compose render)', () => {
  const res = spawnSync(process.execPath, [CHECKER], { encoding: 'utf8' });
  assert.equal(res.status, 0, `a shipped hydra service is unsupervised or probed wrongly:\n${res.stderr}`);
  assert.match(res.stdout, /hydra service\(s\) checked: OK/);
  assert.match(res.stdout, /hydra-edge \(role=edge\)/);
});

/* Round 129: a service the text reader cannot judge must stop the check (see the grace test). */
test('a service using `extends:` makes the static health path refuse (CANNOT VERIFY)', () => {
  const p = path.join(os.tmpdir(), `health-extends-${process.pid}.yml`);
  fs.writeFileSync(p, 'services:\n  base:\n    image: hydra-local:latest\n  hydra-a:\n    extends: base\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.equal(res.status, 2, `status=${res.status} ${res.stdout}`);
  assert.match(res.stderr, /cannot judge 1 service\(s\)/);
  fs.rmSync(p, { force: true });
});

/* Round 131: same depth-awareness, plus docker's `healthcheck: { disable: true }` semantics.
 * Before this, a nested decoy produced a BOGUS diagnosis ("a all-role node … probe /api/v1/health")
 * for a service that declares no healthcheck, and `disable: true` was read as "a healthcheck whose
 * test is empty" while the rendered path sees no healthcheck at all. */
test('a nested decoy healthcheck is not the service\'s own (and the diagnosis says so)', () => {
  const p = path.join(os.tmpdir(), `health-decoy-${process.pid}.yml`);
  fs.writeFileSync(p, 'services:\n  hydra-a:\n    image: hydra-local:latest\n    x-static-decoy:\n      healthcheck:\n        test: ["CMD-SHELL", "curl -fsS http://127.0.0.1:8081/healthz"]\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.notEqual(res.status, 0, res.stdout);
  assert.match(res.stderr, /no healthcheck/);
  assert.doesNotMatch(res.stderr, /probe \/api\/v1\/health/);
  fs.rmSync(p, { force: true });
});

test('`healthcheck: { disable: true }` means NO healthcheck (the rendered path agrees)', () => {
  const p = path.join(os.tmpdir(), `health-disable-${process.pid}.yml`);
  fs.writeFileSync(p, 'services:\n  hydra-a:\n    image: hydra-local:latest\n    healthcheck:\n      disable: true\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.notEqual(res.status, 0, res.stdout);
  assert.match(res.stderr, /no healthcheck/);
  fs.rmSync(p, { force: true });
});

/* Round 142: the static path must read a role that arrives through a YAML ANCHOR or MERGE KEY.
 * Measured before the fix on the shipped cluster file: `hydra-control-a` writes `HYDRA_ROLE: leader`
 * under `environment: &control-env` and `hydra-control-b` merges it (`<<: *control-env`), yet the
 * static path printed `role=all` for both while the RENDERED path printed `role=leader` — the two
 * extractors disagreed, and an `edge` that merges its environment would have been told to probe the
 * admin API it does not have. */
function anchoredFixture(role, probe) {
  const p = path.join(os.tmpdir(), `health-anchor-${process.pid}.yml`);
  fs.writeFileSync(p, [
    'services:',
    '  hydra-a:',
    '    image: hydra-local:latest',
    '    environment: &control-env',
    `      HYDRA_ROLE: ${role}`,
    '    healthcheck:',
    `      test: ["CMD-SHELL", "${probe}"]`,
    '  hydra-b:',
    '    image: hydra-local:latest',
    '    environment:',
    '      <<: *control-env',
    '    healthcheck:',
    `      test: ["CMD-SHELL", "${probe}"]`,
    '',
  ].join('\n'));
  return p;
}

test('a role arriving through an anchor/merge key is read (an edge is not told to probe the admin API)', () => {
  const p = anchoredFixture('edge', 'curl -fsS http://127.0.0.1:8081/healthz');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.equal(res.status, 0, `the merged edge role was not read:\n${res.stdout}${res.stderr}`);
  assert.match(res.stdout, /hydra-b \(role=edge\)/, 'the merging service has no role in the output');
  fs.rmSync(p, { force: true });
});

test('a merge key this file cannot resolve is REFUSED (exit 2), never defaulted to `all`', () => {
  const p = path.join(os.tmpdir(), `health-unresolved-${process.pid}.yml`);
  fs.writeFileSync(p, [
    'services:',
    '  hydra-a:',
    '    image: hydra-local:latest',
    '    environment:',
    '      <<: *not-defined-here',
    '    healthcheck:',
    '      test: ["CMD-SHELL", "curl -fsS http://127.0.0.1:8081/healthz"]',
    '',
  ].join('\n'));
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.equal(res.status, 2, `status=${res.status} ${res.stdout}`);
  assert.match(res.stderr, /cannot judge/);
  assert.match(res.stderr, /\*not-defined-here/);
  fs.rmSync(p, { force: true });
});

/* Round 155: a role this reader CANNOT read must be refused, never defaulted to `all`.
 * Measured on one service written with `environment:` as a LIST (`- HYDRA_ROLE=edge`): the RENDERED
 * path said `role=edge` and passed, the STATIC path said `role=all` and FAILED it — telling the
 * operator to give an edge an admin probe, which is the false positive this guard exists to prevent.
 * The `${VAR}` form behaves the same way. */
test('an unreadable HYDRA_ROLE is REFUSED (exit 2), never defaulted to `all`', () => {
  const listForm = path.join(os.tmpdir(), `health-envlist-${process.pid}.yml`);
  fs.writeFileSync(listForm, [
    'services:',
    '  hydra-edge:',
    '    image: hydra-local:latest',
    '    environment:',
    '      - HYDRA_ROLE=edge',
    '    healthcheck:',
    '      test: ["CMD-SHELL", "curl -fsS http://127.0.0.1:8081/healthz"]',
    '',
  ].join('\n'));
  const r1 = spawnSync(process.execPath, [CHECKER, `--static-file=${listForm}`], { encoding: 'utf8' });
  assert.equal(r1.status, 2, `LIST form: status=${r1.status} ${r1.stdout}`);
  assert.match(r1.stderr, /environment:. is a LIST/);
  assert.doesNotMatch(r1.stderr, /role=all/, 'the unreadable role was turned into `all`');
  fs.rmSync(listForm, { force: true });

  const interpolated = path.join(os.tmpdir(), `health-envvar-${process.pid}.yml`);
  fs.writeFileSync(interpolated, [
    'services:',
    '  hydra-edge:',
    '    image: hydra-local:latest',
    '    environment:',
    '      HYDRA_ROLE: ${NODE_ROLE:-edge}',
    '    healthcheck:',
    '      test: ["CMD-SHELL", "curl -fsS http://127.0.0.1:8081/healthz"]',
    '',
  ].join('\n'));
  const r2 = spawnSync(process.execPath, [CHECKER, `--static-file=${interpolated}`], { encoding: 'utf8' });
  assert.equal(r2.status, 2, `interpolated: status=${r2.status} ${r2.stdout}`);
  assert.match(r2.stderr, /not decided until render time/);
  fs.rmSync(interpolated, { force: true });
});

/* Round 185: the image FILTER must not eat a service by NAME — a service called `hydra-*` whose image
 * does not match `IMAGE_RE` was skipped silently while the OK line counted "N hydra service(s) checked"
 * as if it had been. */
test('a hydra-NAMED service the image filter skips is a finding (rendered path)', () => {
  const r = run([fixture({ services: {
    'hydra-a': svc('hydra:latest', 'leader', CONTROL_HC),
    'hydra-ghost': svc('someone-elses/hydra-fork:latest', 'edge', EDGE_HC),
  } })]);
  assert.equal(r.status, 1, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stdout + r.stderr, /hydra-ghost/);
  assert.match(r.stdout + r.stderr, /the service NAME looks like a hydra node/);
  assert.match(r.stdout + r.stderr, /image filter skipped it, so its role and healthcheck are unchecked/);
});

test('CONTROL: a NON-hydra service with a foreign image is skipped without complaint', () => {
  const r = run([fixture({ services: {
    'hydra-a': svc('hydra:latest', 'leader', CONTROL_HC),
    clickhouse: svc('clickhouse/clickhouse-server:24.3', null, null),
  } })]);
  assert.equal(r.status, 0, `${r.status} ${r.stdout}${r.stderr}`);
  assert.doesNotMatch(r.stdout + r.stderr, /clickhouse: the service NAME/);
});
