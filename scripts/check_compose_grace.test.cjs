#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/check_compose_grace.cjs.
 *
 * The checker ties the deployment grace period to the shutdown budget the code
 * actually uses, so the tests pin: the arithmetic (drain + final + slack), the
 * duration parser, the "found nothing ⇒ exit 2, never a silent pass" floor, and
 * the two shapes that made this a real defect (no `stop_grace_period` at all —
 * Docker then allows 10s — and a value below the budget).
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CHECKER = path.join(__dirname, 'check_compose_grace.cjs');
const REPO = path.resolve(__dirname, '..');

function composeDoc(services) {
  return JSON.stringify({ services }, null, 2);
}

function fixture(doc) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'grace-'));
  const file = path.join(dir, 'rendered.json');
  fs.writeFileSync(file, doc);
  return file;
}

function services(names, gracePeriod) {
  const out = {};
  for (const n of names) {
    out[n] = { image: 'hydra:latest', container_name: n };
    if (gracePeriod !== undefined) out[n].stop_grace_period = gracePeriod;
  }
  return out;
}

function run(files, env = {}) {
  const args = files.map((f) => `--compose-json=${f}`);
  const res = spawnSync(process.execPath, [CHECKER, ...args], {
    encoding: 'utf8',
    env: { ...process.env, COMPOSE_GRACE_MIN_SERVICES: '1', ...env },
  });
  return { status: res.status, stdout: res.stdout || '', stderr: res.stderr || '' };
}

test('all hydra services with an adequate grace period pass', () => {
  const r = run([fixture(composeDoc(services(['hydra-a', 'hydra-b'], '30s')))]);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /drain 20s \+ final 5s \+ slack 5s = 30s required/);
  assert.match(r.stdout, /hydra-a \(stop_grace_period=30s\)/);
});

test('a missing stop_grace_period fails and names Docker\'s 10s default', () => {
  const r = run([fixture(composeDoc(services(['hydra'], undefined)))]);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /hydra: no stop_grace_period \(Docker's default 10s < 30s drain budget\)/);
});

test('a grace period below the budget fails (the 10s-default shape)', () => {
  const r = run([fixture(composeDoc(services(['hydra'], '10s')))]);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /stop_grace_period=10s \(10s\) < 30s/);
});

test('a bigger grace period is fine, and durations are parsed not string-compared', () => {
  const r = run([
    fixture(composeDoc(services(['a'], '40s'))),
    fixture(composeDoc(services(['b'], '1m'))),
  ]);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /a \(stop_grace_period=40s\)/);
  assert.match(r.stdout, /b \(stop_grace_period=1m\)/);
});

test('non-hydra services are ignored', () => {
  const r = run([fixture(composeDoc({
    hydra: { image: 'hydra:latest', stop_grace_period: '30s' },
    clickhouse: { image: 'clickhouse/clickhouse-server:24.3' },
    redis: { image: 'redis:7-alpine' },
  }))]);
  assert.equal(r.status, 0, r.stderr);
  assert.doesNotMatch(r.stdout, /clickhouse|redis/);
});

test('the local image name (hydra-local) is recognised too', () => {
  const r = run([fixture(composeDoc({ 'hydra-a': { image: 'hydra-local:latest' } }))]);
  assert.equal(r.status, 1);
  assert.match(r.stderr, /hydra-a/);
});

test('finding no hydra services is exit 2, never a silent pass', () => {
  const r = run([fixture(composeDoc({ redis: { image: 'redis:7-alpine' } }))], { COMPOSE_GRACE_MIN_SERVICES: '5' });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /CANNOT VERIFY: only 0 hydra-image service\(s\) found/);
});

test('the threshold follows the code, not a hard-coded number', () => {
  // Simulate a maintainer raising the drain default from 20s to 60s, in a FIXTURE
  // main.rs (never the real one: a killed mutation probe could leave it patched).
  const mainRs = path.join(REPO, 'crates', 'hydra-server', 'src', 'main.rs');
  const patched = fs.readFileSync(mainRs, 'utf8').replace(/(fn parse_shutdown_drain_secs[\s\S]*?unwrap_or\()20(\))/, '$160$2');
  assert.notEqual(patched, fs.readFileSync(mainRs, 'utf8'), 'could not build the fixture');
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'grace-main-'));
  const fixtureMain = path.join(dir, 'main.rs');
  fs.writeFileSync(fixtureMain, patched);
  const r = run([fixture(composeDoc(services(['hydra'], '30s')))], { COMPOSE_GRACE_MAIN_RS: fixtureMain });
  assert.equal(r.status, 1, 'a 60s drain default must invalidate a 30s grace period');
  assert.match(r.stderr, /70s required|stop_grace_period=30s \(30s\) < 70s/);
});

test('an unreadable code budget is exit 2 rather than a guess', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'grace-nomain-'));
  const fixtureMain = path.join(dir, 'main.rs');
  fs.writeFileSync(fixtureMain, 'fn main() {}\n');
  const r = run([fixture(composeDoc(services(['hydra'], '30s')))], { COMPOSE_GRACE_MAIN_RS: fixtureMain });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /CANNOT VERIFY: could not read the shutdown budget/);
});

test('unknown arguments are rejected and --help works', () => {
  const bad = spawnSync(process.execPath, [CHECKER, '--wat'], { encoding: 'utf8' });
  assert.equal(bad.status, 2);
  assert.match(bad.stderr, /unknown argument: --wat/);
  const help = spawnSync(process.execPath, [CHECKER, '--help'], { encoding: 'utf8' });
  assert.equal(help.status, 0);
  assert.match(help.stdout, /check_compose_grace/);
});

/**
 * Round 121: a SKIPPED shipped file must still be checked, statically.
 *
 * `docker compose config` cannot render the LOCAL stack in CI (its env_file is gitignored), so that
 * file was skipped there and its three services were never checked — while the floor
 * (`MIN_SERVICES = 4`) happened to equal the 4 services of the two files that do render. Measured
 * 2026-09-30: deleting `stop_grace_period` from hydra-a/b/c still printed OK in CI conditions.
 * `--static-file` drives exactly that path (no docker involved).
 */
function fixtureWithoutGrace() {
  const src = fs.readFileSync(path.join(REPO, 'environment', 'docker-compose.local.yml'), 'utf8');
  const stripped = src.replace(/^ {4}stop_grace_period: 30s\n/gm, '');
  assert.ok(stripped.includes('hydra-a:'), 'fixture lost the local stack');
  assert.ok(!/stop_grace_period/.test(stripped), 'fixture still carries a stop_grace_period');
  const p = path.join(os.tmpdir(), `grace-static-${process.pid}.yml`);
  fs.writeFileSync(p, stripped);
  return p;
}

test('a skipped file is checked STATICALLY: a missing stop_grace_period still fails', () => {
  const bad = fixtureWithoutGrace();
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${bad}`], { encoding: 'utf8' });
  assert.notEqual(res.status, 0, `a static check found no problem:\n${res.stdout}`);
  assert.match(res.stderr, /no `stop_grace_period`/);
  assert.match(res.stderr, /hydra-a/);
  fs.rmSync(bad, { force: true });
});

test('CONTROL: the untouched local stack passes the static check', () => {
  const res = spawnSync(process.execPath, [
    CHECKER,
    `--static-file=${path.join(REPO, 'environment', 'docker-compose.local.yml')}`,
  ], { encoding: 'utf8' });
  assert.equal(res.status, 0, res.stderr);
  assert.match(res.stdout, /hydra-a \(stop_grace_period=30s\)/);
});

test('the static path never passes by finding nothing', () => {
  const empty = path.join(os.tmpdir(), `grace-static-empty-${process.pid}.yml`);
  fs.writeFileSync(empty, 'services:\n  redis:\n    image: redis:7\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${empty}`], { encoding: 'utf8' });
  assert.equal(res.status, 2, `status=${res.status} ${res.stdout}`);
  assert.match(res.stderr, /found no hydra-image service/);
  fs.rmSync(empty, { force: true });
});

test('the shipped compose files pass (real docker compose render)', () => {
  const res = spawnSync(process.execPath, [CHECKER], { encoding: 'utf8' });
  assert.equal(res.status, 0, `a shipped hydra service can be SIGKILLed mid-drain:\n${res.stderr}`);
  // 7 locally (the local stack renders here); 4 in CI, where the local file's
  // gitignored env_file is absent and the checker prints a SKIP reason instead.
  assert.match(res.stdout, /\d+ hydra service\(s\) checked: OK/);
});

/* Round 129: shapes the text reader cannot judge must STOP the check, not vanish.
 * Measured before this: a file whose hydra-a used `extends:` and whose hydra-c used
 * `image: ${HYDRA_IMAGE_TAG}` printed OK (exit 0) with only base-hydra checked — two services that
 * really get deployed were never looked at. */
test('a service using `extends:` makes the static path refuse (CANNOT VERIFY)', () => {
  const p = path.join(os.tmpdir(), `grace-extends-${process.pid}.yml`);
  fs.writeFileSync(p, 'services:\n  base:\n    image: hydra-local:latest\n    stop_grace_period: 30s\n  hydra-a:\n    extends: base\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.equal(res.status, 2, `status=${res.status} ${res.stdout}`);
  assert.match(res.stderr, /cannot judge 1 service\(s\)/);
  assert.match(res.stderr, /hydra-a uses `extends:`/);
  fs.rmSync(p, { force: true });
});

test('a service whose image is a variable makes the static path refuse too', () => {
  const p = path.join(os.tmpdir(), `grace-varimg-${process.pid}.yml`);
  fs.writeFileSync(p, 'services:\n  hydra-c:\n    image: ${HYDRA_IMAGE_TAG}\n    stop_grace_period: 30s\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.equal(res.status, 2, `status=${res.status} ${res.stdout}`);
  assert.match(res.stderr, /not decided until render time/);
  fs.rmSync(p, { force: true });
});

/* ...and the mirror risk of that fix: a legal YAML line comment must NOT fail. */
test('an inline comment after stop_grace_period is tolerated (legal YAML)', () => {
  const p = path.join(os.tmpdir(), `grace-comment-${process.pid}.yml`);
  fs.writeFileSync(p, 'services:\n  hydra-a:\n    image: hydra-local:latest\n    stop_grace_period: 30s # measured 25s drain\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.equal(res.status, 0, `status=${res.status} ${res.stderr}`);
  assert.match(res.stdout, /stop_grace_period=30s/);
  fs.rmSync(p, { force: true });
});

/* Round 131: the key lookup is DEPTH-AWARE. `scalar`/`hasKey` searched the whole block with
 * `^\s+key:`, so a key inside a NESTED mapping answered a question about the service itself:
 * `x-static-decoy: { stop_grace_period: 30s }` made a service with no grace period look compliant. */
test('a nested decoy key does not satisfy the grace rule', () => {
  const p = path.join(os.tmpdir(), `grace-decoy-${process.pid}.yml`);
  fs.writeFileSync(p, 'services:\n  hydra-a:\n    image: hydra-local:latest\n    x-static-decoy:\n      stop_grace_period: 30s\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.notEqual(res.status, 0, `a nested key satisfied the rule:\n${res.stdout}`);
  assert.match(res.stderr, /no `stop_grace_period`/);
  fs.rmSync(p, { force: true });
});

test('CONTROL: the same key at the service level still passes', () => {
  const p = path.join(os.tmpdir(), `grace-ownkey-${process.pid}.yml`);
  fs.writeFileSync(p, 'services:\n  hydra-a:\n    image: hydra-local:latest\n    stop_grace_period: 30s\n    x-static-decoy:\n      other: 1\n');
  const res = spawnSync(process.execPath, [CHECKER, `--static-file=${p}`], { encoding: 'utf8' });
  assert.equal(res.status, 0, `status=${res.status} ${res.stderr}`);
  fs.rmSync(p, { force: true });
});

/* Round 185: the image FILTER must not eat a service by NAME. A service called `hydra-*` whose image
 * does not match `IMAGE_RE` was skipped silently — nothing about its stop_grace_period was checked
 * while the OK line counted "N hydra service(s) checked" as if it had been. */
test('a hydra-NAMED service the image filter skips is a finding, not a silent skip', () => {
  const svc = services(['hydra-a', 'hydra-ghost'], '30s');
  svc['hydra-ghost'].image = 'someone-elses/hydra-fork:latest';
  const r = run([fixture(composeDoc(svc))]);
  assert.equal(r.status, 1, r.stdout);
  assert.match(r.stdout + r.stderr, /hydra-ghost: the service NAME looks like a hydra node/);
  assert.match(r.stdout + r.stderr, /image filter skipped it, so nothing about its stop_grace_period/);
});

test('CONTROL: a NON-hydra service with a foreign image is still skipped without complaint', () => {
  const svc = services(['hydra-a'], '30s');
  svc.clickhouse = { image: 'clickhouse/clickhouse-server:24.3' };
  const r = run([fixture(composeDoc(svc))]);
  assert.equal(r.status, 0, r.stdout);
  assert.doesNotMatch(r.stdout + r.stderr, /clickhouse: the service NAME/);
});
