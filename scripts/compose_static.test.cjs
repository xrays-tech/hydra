#!/usr/bin/env node
'use strict';
/**
 * Tests for `scripts/compose_static.cjs` — the text reader the two compose guards fall back to when
 * `docker compose config` cannot render a file (in CI that is always the LOCAL stack, whose
 * gitignored env_file is absent).
 *
 * The parser is deliberately shallow, so these cases pin BOTH what it does and what it cannot do:
 * a fixture using `extends:`/anchors must be seen as "maybe inherited" rather than silently reading
 * a service as compliant. The `m` flag matters: a service block is many lines, and without it the
 * `image:` regex only matched when that line happened to be last (measured while writing it —
 * every shipped file reported zero hydra services).
 */
const test = require('node:test');
const assert = require('node:assert/strict');

const {
  serviceBlocks,
  hydraServices,
  scalar,
  scalarOwn,
  hasOwnKey,
  ownKeys,
  collectAnchors,
  healthcheckDisabled,
  healthcheckTest,
  envValue,
  unjudgeableServices,
} = require('./compose_static.cjs');

const IMAGE = /(^|\/)hydra(-local)?:/;

const SAMPLE = [
  'name: fixture',
  'services:',
  '  hydra:',
  '    image: hydra:latest',
  '    stop_grace_period: 30s',
  '    environment:',
  '      HYDRA_ROLE: leader',
  '    healthcheck:',
  '      test: ["CMD-SHELL", "curl -fsS http://127.0.0.1:8081/api/v1/health"]',
  '  redis:',
  '    image: redis:7-alpine',
  '    healthcheck:',
  '      test: ["CMD", "redis-cli", "ping"]',
  'volumes:',
  '  data:',
  '',
].join('\n');

test('service blocks stop at the next service and at the next top-level key', () => {
  const blocks = serviceBlocks(SAMPLE);
  assert.deepEqual([...blocks.keys()], ['hydra', 'redis']);
  assert.ok(!blocks.get('redis').includes('volumes:'), 'the trailing top-level key leaked into a block');
});

test('only services whose image matches are returned, with scalars readable', () => {
  const svcs = hydraServices(SAMPLE, IMAGE);
  assert.equal(svcs.length, 1);
  assert.equal(svcs[0].name, 'hydra');
  assert.equal(scalar(svcs[0].block, 'stop_grace_period'), '30s');
  assert.equal(scalar(svcs[0].block, 'HYDRA_ROLE'), 'leader');
  assert.equal(scalar(svcs[0].block, 'not_there'), null);
  assert.ok(hasOwnKey(svcs[0].block, 'healthcheck'));
  assert.ok(!hasOwnKey(svcs[0].block, 'deploy'));
});

test('a file with no hydra-image service yields an empty list (callers must refuse to pass)', () => {
  const svcs = hydraServices('services:\n  redis:\n    image: redis:7\n', IMAGE);
  assert.equal(svcs.length, 0);
});

test('multiple hydra services (the local-stack shape) are all returned', () => {
  const text = [
    'services:',
    '  hydra-a:',
    '    image: hydra-local:latest',
    '    stop_grace_period: 30s',
    '  hydra-b:',
    '    image: hydra-local:latest',
    '  mock:',
    '    image: other:1',
  ].join('\n');
  const svcs = hydraServices(text, IMAGE);
  assert.deepEqual(svcs.map((s) => s.name), ['hydra-a', 'hydra-b']);
  // hydra-b has no grace period: the guards must see `null`, not a default.
  assert.equal(scalar(svcs[1].block, 'stop_grace_period'), null);
});

test('the shipped compose files parse, and every hydra service is found', () => {
  const fs = require('node:fs');
  const path = require('node:path');
  const ROOT = path.resolve(__dirname, '..');
  const files = ['docker-compose.yml', 'docker-compose.cluster.yml', 'docker-compose.local.yml'];
  let total = 0;
  for (const f of files) {
    const text = fs.readFileSync(path.join(ROOT, 'environment', f), 'utf8');
    const svcs = hydraServices(text, IMAGE);
    assert.ok(svcs.length > 0, `${f}: the static reader found no hydra service`);
    for (const s of svcs) {
      // These two assertions must use the SAME readers the guards use: `check_compose_grace` reads
      // `scalarOwn` for the grace period and `check_compose_health` reads `hasOwnKey` for the
      // healthcheck. Measured round 144: the nested-tolerant `scalar` satisfied this assertion for a
      // fixture whose `stop_grace_period` lived only inside a decoy sub-mapping, while the guard
      // reported `MISSING` — the test was asserting about a reader the guard does not use.
      assert.ok(scalarOwn(s.block, 'stop_grace_period'), `${f} · ${s.name}: no stop_grace_period visible to the static path`);
      assert.ok(hasOwnKey(s.block, 'healthcheck'), `${f} · ${s.name}: no healthcheck visible to the static path`);
    }
    total += svcs.length;
  }
  // 1 (compose) + 3 (cluster) + 3 (local) — the count the floor of each guard is sized against.
  assert.equal(total, 7);
});

/* Round 131: own-level vs nested keys. `scalar` searches the whole block (some keys legitimately
 * live in a sub-mapping: `test:` under `healthcheck:`, `HYDRA_ROLE` under `environment:`), while
 * `scalarOwn`/`hasOwnKey` answer questions about the SERVICE — a nested decoy must not satisfy them.
 * Measured before this: `x-static-decoy: { stop_grace_period: 30s }` made a service with no grace
 * period look compliant, and a decoy `healthcheck` produced a bogus probe diagnosis. */
test('own-level lookups ignore nested decoys, nested lookups still work', () => {
  const text = [
    'services:',
    '  hydra-a:',
    '    image: hydra-local:latest',
    '    environment:',
    '      HYDRA_ROLE: leader',
    '    x-static-decoy:',
    '      stop_grace_period: 30s',
    '      healthcheck:',
    '        test: ["CMD-SHELL", "true"]',
    '    healthcheck:',
    '      test: ["CMD-SHELL", "curl -fsS http://127.0.0.1:8081/api/v1/health"]',
  ].join('\n');
  const [svc] = hydraServices(text, IMAGE);
  assert.equal(scalarOwn(svc.block, 'stop_grace_period'), null, 'a nested decoy satisfied an own-level question');
  assert.ok(hasOwnKey(svc.block, 'healthcheck'));
  assert.ok(hasOwnKey(svc.block, 'environment'));
  assert.ok(!hasOwnKey(svc.block, 'x-static-decoy-nope'));
  // The service's own sub-blocks are read from the RIGHT mapping: the probe command comes from its
  // own `healthcheck:` and the role from its own `environment:`.
  assert.equal(envValue(svc.block, 'HYDRA_ROLE'), 'leader');
  assert.match(healthcheckTest(svc.block), /api\/v1\/health/);
  // ...and this is WHY the guard uses those readers: the nested-tolerant `scalar` returns the FIRST
  // matching line, which here is the decoy's (`["CMD-SHELL", "true"]`). Asserted on purpose, so the
  // hazard stays visible instead of being rediscovered later.
  assert.match(scalar(svc.block, 'test'), /true/);
  assert.ok(ownKeys(svc.block).some((l) => l.includes('image:')));
});

test('`healthcheck: { disable: true }` is recognised (docker removes the key)', () => {
  const text = 'services:\n  hydra-a:\n    image: hydra-local:latest\n    healthcheck:\n      disable: true\n';
  const [svc] = hydraServices(text, IMAGE);
  assert.ok(hasOwnKey(svc.block, 'healthcheck'), 'the key is present in the text');
  assert.ok(healthcheckDisabled(svc.block), 'but it disables the healthcheck');
  const other = hydraServices('services:\n  hydra-b:\n    image: hydra-local:latest\n    healthcheck:\n      test: ["CMD", "true"]\n', IMAGE)[0];
  assert.ok(!healthcheckDisabled(other.block));
});

/* Round 140: comments are comments. Two real losses were measured on `docker-compose.local.yml`:
 *   * a COLUMN-ZERO comment inside `services:` ended the mapping, so every service after it vanished
 *     from `serviceBlocks` entirely;
 *   * a TRAILING comment on a service line (`redis:   # local cache`) made the line unrecognisable,
 *     so that service's keys folded into the previous block and the service disappeared from
 *     `hydraServices` — the local `hydra-c` was no longer checked by either compose guard, with no
 *     refusal and the floors still satisfied. */
test('a column-zero comment inside services does not end the mapping', () => {
  const text = [
    'services:',
    '  hydra-a:',
    '    image: hydra-local:latest',
    '# NOTE: the edge has no admin API',
    '  hydra-edge:',
    '    image: hydra-local:latest',
  ].join('\n');
  assert.deepEqual(hydraServices(text, IMAGE).map((s) => s.name), ['hydra-a', 'hydra-edge']);
});

test('a trailing comment on a service line does not hide that service', () => {
  const text = ['services:', '  hydra-a:', '    image: hydra-local:latest', '  redis:   # local cache', '    image: redis:7'].join('\n');
  assert.deepEqual(hydraServices(text, IMAGE).map((s) => s.name), ['hydra-a']);
  // ...and the commented service's own keys are not folded into hydra-a.
  const [a] = hydraServices(text, IMAGE);
  assert.equal(scalarOwn(a.block, 'image'), 'hydra-local:latest');
});

test('REGRESSION on the real file: a trailing comment must not drop hydra-c', () => {
  const fsmod = require('node:fs');
  const pathmod = require('node:path');
  const file = pathmod.resolve(__dirname, '..', 'environment', 'docker-compose.local.yml');
  const text = fsmod.readFileSync(file, 'utf8');
  const patched = text.replace(/^  redis:$/m, '  redis:   # local cache');
  assert.notEqual(patched, text, 'fixture: the redis service line was not found');
  assert.deepEqual(
    hydraServices(patched, IMAGE).map((s) => s.name),
    hydraServices(text, IMAGE).map((s) => s.name),
    'the trailing comment changed which services are visible',
  );
});

/* Round 142: YAML anchors and merge keys. Measured on the shipped cluster file before the fix:
 * `hydra-control-a` writes `HYDRA_ROLE: leader` verbatim under `environment: &control-env`, and
 * `subBlock` returned [] because the key line carries an anchor — `envValue` said null, and
 * `check_compose_health` silently substituted `'all'`. `hydra-control-b` merges the same anchor
 * (`<<: *control-env`), so its role was unreadable for a second reason. */
const ANCHORED = [
  'services:',
  '  hydra-a:',
  '    image: hydra-local:latest',
  '    environment: &control-env',
  '      HYDRA_ROLE: leader',
  '      HYDRA_REDIS_URL: "redis://redis:6379"',
  '  hydra-b:',
  '    image: hydra-local:latest',
  '    environment:',
  '      <<: *control-env',
  '      HYDRA_NODE_ID: b',
  '  hydra-c:',
  '    image: hydra-local:latest',
  '    environment:',
  '      <<: *control-env',
  '      HYDRA_ROLE: edge',
  '',
].join('\n');

function blockOf(text, name) {
  const b = serviceBlocks(text).get(name);
  assert.ok(b !== undefined, `fixture: service ${name} not found`);
  return b;
}

test('an ANCHOR on the key line does not hide the sub-mapping it introduces', () => {
  const anchors = collectAnchors(ANCHORED);
  assert.deepEqual([...anchors.keys()], ['control-env'], 'the anchor was not collected');
  assert.equal(
    envValue(blockOf(ANCHORED, 'hydra-a'), 'HYDRA_ROLE', anchors),
    'leader',
    'the value written verbatim under `environment: &control-env` was not read',
  );
});

test('a merged `environment:` is read through its anchor', () => {
  const anchors = collectAnchors(ANCHORED);
  assert.equal(envValue(blockOf(ANCHORED, 'hydra-b'), 'HYDRA_ROLE', anchors), 'leader');
  // ...and the service's own keys still win over the merged ones (YAML merge semantics).
  assert.equal(envValue(blockOf(ANCHORED, 'hydra-b'), 'HYDRA_NODE_ID', anchors), 'b');
  assert.equal(envValue(blockOf(ANCHORED, 'hydra-c'), 'HYDRA_ROLE', anchors), 'edge');
});

test('CONTROL: without the anchor table a merged variable stays unreadable (no guessing)', () => {
  assert.equal(envValue(blockOf(ANCHORED, 'hydra-b'), 'HYDRA_ROLE'), null);
});

test('a merge this file cannot resolve is REFUSED, not silently defaulted', () => {
  const unknown = ANCHORED.replace('environment: &control-env', 'environment:');
  const named = unjudgeableServices(unknown);
  assert.deepEqual(
    named.map((n) => n.name),
    ['hydra-b', 'hydra-c'],
    'services merging an undefined anchor must be named as unjudgeable',
  );
  assert.match(named[0].reason, /\*control-env/);
  // The list form is not modelled either: refusing beats guessing which anchor wins.
  const listForm = ANCHORED.replace('<<: *control-env\n      HYDRA_NODE_ID: b', '<<: [*control-env, *other]\n      HYDRA_NODE_ID: b');
  const named2 = unjudgeableServices(listForm);
  assert.deepEqual(named2.map((n) => n.name), ['hydra-b']);
  assert.match(named2[0].reason, /LIST of anchors/);
});

test('a SERVICE-level merge is refused (its grace/healthcheck may come from the anchor)', () => {
  const svcMerge = [
    'services:',
    '  hydra-a:',
    '    image: hydra-local:latest',
    '    stop_grace_period: 30s',
    '  hydra-b:',
    '    <<: *base',
    '    image: hydra-local:latest',
    '',
  ].join('\n');
  assert.deepEqual(
    unjudgeableServices(svcMerge).map((n) => n.name),
    ['hydra-b'],
    'a whole-mapping merge at the service level must be refused',
  );
});

test('REGRESSION on the real cluster file: the three members read the SAME member list', () => {
  const fsmod = require('node:fs');
  const pathmod = require('node:path');
  const file = pathmod.resolve(__dirname, '..', 'environment', 'docker-compose.cluster.yml');
  const text = fsmod.readFileSync(file, 'utf8');
  const anchors = collectAnchors(text);
  // ADR-0001: there is no role to read any more. What the real file MUST get right is the thing the
  // cluster's identity depends on — every member carries the SAME `HYDRA_CLUSTER_PEERS` string (the
  // order is positional raft identity, so a member with a different list is a different cluster),
  // and each carries its OWN node id / raft address.
  const peers = {};
  const nodes = {};
  const listen = {};
  for (const [name, block] of serviceBlocks(text)) {
    if (!/^hydra-/.test(name)) continue;
    peers[name] = envValue(block, 'HYDRA_CLUSTER_PEERS', anchors);
    nodes[name] = envValue(block, 'HYDRA_NODE_ID', anchors);
    listen[name] = envValue(block, 'HYDRA_ARACHNE_LISTEN', anchors);
  }
  const names = Object.keys(peers).sort();
  assert.equal(names.length, 3, `expected three members, got ${names.join(', ')}`);
  const distinctPeers = new Set(Object.values(peers));
  assert.deepEqual(
    [...distinctPeers].filter((v) => v !== null).length,
    1,
    `every member must carry the SAME member list, got ${JSON.stringify(peers)}`,
  );
  assert.equal(new Set(Object.values(nodes)).size, 3, `each member needs its own node id, got ${JSON.stringify(nodes)}`);
  assert.equal(new Set(Object.values(listen)).size, 3, `each member needs its own raft address, got ${JSON.stringify(listen)}`);
  // ...and the real file must NOT be refused: every anchor it merges is defined inline.
  assert.deepEqual(unjudgeableServices(text), []);
});

/* Round 156: a value at the service's OWN level that is interpolated (`stop_grace_period: ${VAR:-30s}`)
 * is unreadable here. The grace guard used to call it `unparseable` and return 1 — a DRIFT verdict
 * against a legal compose file — instead of refusing it (the module's rule: unreadable ⇒ refuse). */
test('an interpolated own-level value is REFUSED, not judged', () => {
  const text = [
    'services:',
    '  hydra-a:',
    '    image: hydra-local:latest',
    '    stop_grace_period: ${HYDRA_STOP_GRACE:-30s}',
    '    healthcheck:',
    '      test: ["CMD", "true"]',
    '',
  ].join('\n');
  const named = unjudgeableServices(text);
  assert.deepEqual(named.map((n) => n.name), ['hydra-a']);
  assert.match(named[0].reason, /not decided until render time/);
  // ...and the shipped files are NOT refused by this rule (they interpolate only INSIDE nested
  // mappings: `environment:` values and the escaped `$${…}` token inside a healthcheck command).
  const fsmod = require('node:fs');
  const pathmod = require('node:path');
  for (const f of ['docker-compose.yml', 'docker-compose.cluster.yml', 'docker-compose.local.yml']) {
    const file = pathmod.resolve(__dirname, '..', 'environment', f);
    assert.deepEqual(unjudgeableServices(fsmod.readFileSync(file, 'utf8')), [], `${f} was refused`);
  }
});
