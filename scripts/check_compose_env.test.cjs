#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/check_compose_env.cjs.
 *
 * The checker exists because the tree shipped manifest/binary mismatches that nothing failed on:
 * `docker-compose.local.yml` still SET `HYDRA_LEADER_LEASE_MS` / `HYDRA_CONTROL_POLL_MS`
 * (`RETIRED_CLUSTER_ENV` names are reported as ignored at boot), and `environment/build.sh` — the
 * recipe that produces the image every manifest in `environment/` runs — omitted the
 * `cluster-redis` feature that a Redis-backed manifest requires.
 *
 * So these tests pin BOTH directions for both rules: the real tree passes, and a tree with each
 * defect fails with the manifest, the variable and the feature named. Three of them are CONTROLs
 * against a guard that fails for the wrong reason: a fixture whose nodes MERGE their environment
 * through a YAML anchor must not be reported as "wiring without Redis", a recipe that
 * omits a feature no manifest needs must pass, and the CANNOT-VERIFY paths must not be a FAIL.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CHECKER = path.join(__dirname, 'check_compose_env.cjs');
const REPO = path.resolve(__dirname, '..');

/** A tree with the REAL `CLUSTER_ONLY_ENV` / `RETIRED_CLUSTER_ENV` tables, so the rules are the shipped ones. */
function tree({ manifests = {}, recipes = {}, owner = true, ownerText = null } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'compose-env-'));
  if (owner) {
    const ownerDir = path.join(dir, 'crates/hydra-server/src/cluster');
    fs.mkdirSync(ownerDir, { recursive: true });
    fs.writeFileSync(
      path.join(ownerDir, 'mod.rs'),
      ownerText === null
        ? fs.readFileSync(path.join(REPO, 'crates/hydra-server/src/cluster/mod.rs'), 'utf8')
        : ownerText,
    );
  }
  const envDir = path.join(dir, 'environment');
  fs.mkdirSync(envDir, { recursive: true });
  for (const [name, text] of Object.entries(manifests)) fs.writeFileSync(path.join(envDir, name), text);
  for (const [name, text] of Object.entries(recipes)) fs.writeFileSync(path.join(envDir, name), text);
  return dir;
}

function run(dir, env = {}) {
  const res = spawnSync(process.execPath, [CHECKER], {
    encoding: 'utf8',
    env: { ...process.env, COMPOSE_ENV_ROOT: dir, ...env },
  });
  return { status: res.status, stdout: res.stdout || '', stderr: res.stderr || '' };
}

/** A minimal node: image + env lines, no healthcheck needed (that is another guard's rule). */
function node(name, envLines, { extraPorts = [] } = {}) {
  return [
    `  ${name}:`,
    '    image: hydra:latest',
    '    environment:',
    ...envLines.map((l) => `      ${l}`),
    '    ports:',
    ...extraPorts.map((p) => `      - "${p}"`),
  ].join('\n');
}

const REDIS = 'HYDRA_REDIS_URL: "redis://redis:6379"';
const FULL_RECIPE = 'cargo build -p hydra-server --features server,cluster-redis,db-postgres --bin hydra\n';

/** The three shipped manifest names, each given the same body unless overridden. */
function allManifests(body) {
  return {
    'docker-compose.yml': body,
    'docker-compose.cluster.yml': body,
    'docker-compose.local.yml': body,
  };
}
function allRecipes(text = FULL_RECIPE) {
  return { 'build.sh': text, 'release.sh': text };
}

test('the real repository tree passes', () => {
  const res = spawnSync(process.execPath, [CHECKER], { encoding: 'utf8', env: { ...process.env, COMPOSE_ENV_ROOT: '' } });
  assert.equal(res.status, 0, `${res.status} ${res.stdout}${res.stderr}`);
  assert.match(res.stdout, /none sets any of the 15 variable\(s\) RETIRED_CLUSTER_ENV retires/);
  // The judged counts are asserted so a scan that finds nothing cannot pass: the single-node file
  // has one hydra service, both multi-node files have three.
  assert.match(res.stdout, /docker-compose\.yml=1, environment\/docker-compose\.cluster\.yml=3, environment\/docker-compose\.local\.yml=3/);
});

test('FALSIFY: a manifest that still sets a RETIRED variable is caught, and the variable is named', () => {
  const dir = tree({
    manifests: allManifests([
      'services:',
      node('hydra-a', [REDIS, 'HYDRA_REDIS_URL: "redis://redis:6379"', 'HYDRA_LEADER_LEASE_MS: "15000"']),
    ].join('\n')),
    recipes: allRecipes(),
  });
  const r = run(dir);
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /docker-compose\.yml: sets HYDRA_LEADER_LEASE_MS, which ADR-0001 RETIRED/);
  // ...and for the OTHER name in the shipped defect, so the rule is the table and not one string.
  const dir2 = tree({
    manifests: allManifests([
      'services:',
      node('hydra-a', [REDIS, 'HYDRA_REDIS_URL: "redis://redis:6379"', 'HYDRA_CONTROL_POLL_MS: "500"']),
    ].join('\n')),
    recipes: allRecipes(),
  });
  const r2 = run(dir2);
  assert.equal(r2.status, 1, r2.stdout + r2.stderr);
  assert.match(r2.stderr, /sets HYDRA_CONTROL_POLL_MS, which ADR-0001 RETIRED/);
});

test('FALSIFY: a recipe that omits the feature the manifest needs is caught, and the feature is named', () => {
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS])].join('\n')),
    recipes: allRecipes('cargo build -p hydra-server --features server,db-postgres --bin hydra\n'),
  });
  const r = run(dir);
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /environment\/build\.sh: .*missing feature\(s\) cluster-redis required by environment\/docker-compose\.yml/);
  // Every recipe is judged, not just the first: the same defect in the other one is reported too.
  assert.match(r.stderr, /environment\/release\.sh: .*missing feature\(s\) cluster-redis/);
});

test('CONTROL: a recipe missing a feature NO manifest needs stays green (the rule is one-directional)', () => {
  const dir = tree({
    // A single-node manifest: no Redis — so `cluster-redis` is not required of the recipe,
    // and a recipe that omits it must not be nagged.
    manifests: allManifests(['services:', node('hydra', ['HYDRA_LISTEN: "0.0.0.0:8080"'])].join('\n')),
    recipes: allRecipes('cargo build -p hydra-server --features server,db-postgres --bin hydra\n'),
  });
  const r = run(dir);
  assert.equal(r.status, 0, r.stdout + r.stderr);
});

test('FALSIFY: cluster wiring with no Redis URL is caught — that node boots standalone', () => {
  const dir = tree({
    manifests: allManifests([
      'services:',
      node('hydra-a', ['HYDRA_REDIS_MODE: single', 'HYDRA_NODE_ID: "a"']),
    ].join('\n')),
    recipes: allRecipes(),
  });
  const r = run(dir);
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /sets cluster wiring \(HYDRA_REDIS_MODE, HYDRA_NODE_ID\) but no HYDRA_REDIS_URL/);
});

test('CONTROL: a node whose environment is one YAML merge is read through the anchor', () => {
  // The shipped cluster file is written this way: `hydra-a` defines `environment: &node-env` and the
  // other two merge it. A reader that looked only at the literal keys of a merging service would
  // see no Redis URL on `hydra-b`/`hydra-c` and report a false "wiring without Redis".
  const dir = tree({
    manifests: allManifests([
      'services:',
      [
        '  hydra-a:',
        '    image: hydra:latest',
        '    environment: &node-env',
        `      ${REDIS}`,
        '      HYDRA_REDIS_MODE: single',
        '      HYDRA_NODE_ID: a',
        '  hydra-b:',
        '    image: hydra:latest',
        '    environment:',
        '      <<: *node-env',
        '      HYDRA_NODE_ID: b',
        '  hydra-c:',
        '    image: hydra:latest',
        '    environment:',
        '      <<: *node-env',
        '      HYDRA_NODE_ID: c',
      ].join('\n'),
    ].join('\n')),
    recipes: allRecipes(),
  });
  const r = run(dir);
  assert.equal(r.status, 0, r.stdout + r.stderr);
  assert.match(r.stdout, /docker-compose\.yml=3/);
});

test('FALSIFY: a Redis setting and no such feature is caught on the REDIS name alone', () => {
  // `HYDRA_REDIS_MODE` alone (no URL) is enough: without `cluster-redis` that setting is read by
  // nothing at all.
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS, 'HYDRA_REDIS_MODE: single'])].join('\n')),
    recipes: allRecipes('cargo build -p hydra-server --features server,db-postgres --bin hydra\n'),
  });
  const r = run(dir);
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /missing feature\(s\) cluster-redis required by environment\/docker-compose\.yml/);
});

test('CANNOT VERIFY: a manifest with no hydra service is refused, not passed', () => {
  const dir = tree({
    manifests: allManifests('services:\n  redis:\n    image: redis:7-alpine\n'),
    recipes: allRecipes(),
  });
  const r = run(dir);
  assert.equal(r.status, 2, r.stdout + r.stderr);
  assert.match(r.stderr, /CANNOT VERIFY: found no hydra-image service/);
});

test('CANNOT VERIFY: a service this text reader cannot judge is refused', () => {
  const dir = tree({
    manifests: allManifests([
      'services:',
      node('hydra-a', [REDIS]),
      '  hydra-b:',
      '    extends: hydra-a',
      '    environment:',
      `      ${REDIS}`,
    ].join('\n')),
    recipes: allRecipes(),
  });
  const r = run(dir);
  assert.equal(r.status, 2, r.stdout + r.stderr);
  assert.match(r.stderr, /CANNOT VERIFY: cannot judge .*hydra-b uses `extends:`/);
});

test('CANNOT VERIFY: a recipe whose feature set cannot be read is refused', () => {
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS])].join('\n')),
    recipes: { 'build.sh': '# cargo build --features server\n', 'release.sh': FULL_RECIPE },
  });
  const r = run(dir);
  assert.equal(r.status, 2, r.stdout + r.stderr);
  assert.match(r.stderr, /CANNOT VERIFY: found no `--features …` or `FEATURES="…"` in environment\/build\.sh/);
});

test('CANNOT VERIFY: the source tables moving is refused, not treated as "no retired names"', () => {
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS])].join('\n')),
    recipes: allRecipes(),
    owner: false,
  });
  const r = run(dir);
  assert.equal(r.status, 2, r.stdout + r.stderr);
  assert.match(r.stderr, /CANNOT VERIFY: cannot read crates\/hydra-server\/src\/cluster\/mod\.rs/);
  // The file PRESENT but the tables gone is the other half: a guard that read the tables as empty
  // would report "none sets any of the 0 variable(s) RETIRED_CLUSTER_ENV retires" and pass.
  const moved = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS])].join('\n')),
    recipes: allRecipes(),
    ownerText: '// the cluster module, after a refactor that moved both tables\npub fn nothing() {}\n',
  });
  const r2 = run(moved);
  assert.equal(r2.status, 2, r2.stdout + r2.stderr);
  assert.match(r2.stderr, /could not read CLUSTER_ONLY_ENV \/ RETIRED_CLUSTER_ENV/);
});

test('env_file names ARE judged when the file exists, and NOTED when it does not', () => {
  const manifest = [
    'services:',
    '  hydra-a:',
    '    image: hydra:latest',
    '    env_file: [local-test.env]',
    '    environment:',
    `      ${REDIS}`,
  ].join('\n');
  const dir = tree({ manifests: allManifests(manifest), recipes: allRecipes() });
  // Absent: a NOTE, never silence, and never a failure — the file is gitignored and cannot be
  // required to exist in a checkout.
  const absent = run(dir);
  assert.equal(absent.status, 0, absent.stdout + absent.stderr);
  assert.match(absent.stdout, /note: .*env_file local-test\.env is not present, so its variables are not judged/);
  // Present AND carrying a retired name: caught, because the file is what the container really gets.
  fs.writeFileSync(path.join(dir, 'environment/local-test.env'), 'HYDRA_LEADER_LEASE_MS=15000\n');
  const present = run(dir);
  assert.equal(present.status, 1, present.stdout + present.stderr);
  assert.match(present.stderr, /sets HYDRA_LEADER_LEASE_MS, which ADR-0001 RETIRED/);
});

test('the table reader refuses a declared length that disagrees with its entries', () => {
  const { readTable } = require(CHECKER);
  assert.deepEqual(readTable('const X: [&str; 2] = [\n "A",\n "B",\n];', 'X').names, ['A', 'B']);
  assert.equal(readTable('const X: [&str; 3] = [\n "A",\n "B",\n];', 'X'), null);
  assert.equal(readTable('const Y: [&str; 1] = ["A"];', 'X'), null);
});

/* Phase 2d: SQLite is gone and the relational backend is derived from HYDRA_DB_URL. These cases
 * pin BOTH directions: a postgres URL requires `db-postgres`, a mysql URL requires `db-mysql`, a
 * sqlite URL is refused, and a build with neither/both backend features is refused. */
const POSTGRES_URL = 'HYDRA_DB_URL: "postgres://hydra:hydra@postgres:5432/hydra"';
const MYSQL_URL = 'HYDRA_DB_URL: "mysql://hydra:hydra@mysql:3306/hydra"';
const SQLITE_URL = 'HYDRA_DB_URL: "sqlite:/app/data/hydra.db?mode=rwc"';

test('FALSIFY: a SQLite HYDRA_DB_URL is refused (the backend was removed)', () => {
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS, SQLITE_URL])].join('\n')),
    recipes: allRecipes(),
  });
  const r = run(dir);
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /uses SQLite .*sqlite:\/app\/data\/hydra\.db/);
  assert.match(r.stderr, /postgres:\/\/.*or mysql:\/\//);
});

test('FALSIFY: a postgres manifest needs a recipe variant carrying db-postgres', () => {
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS, POSTGRES_URL])].join('\n')),
    recipes: {
      'build.sh': 'cargo build -p hydra-server --features server,cluster-redis,db-mysql --bin hydra\n',
      'release.sh': 'cargo build -p hydra-server --features server,cluster-redis,db-mysql --bin hydra\n',
    },
  });
  const r = run(dir);
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /no build variant carries `db-postgres`/);
});

test('CONTROL: a mysql manifest is satisfied by a db-mysql recipe variant', () => {
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS, MYSQL_URL])].join('\n')),
    recipes: allRecipes('cargo build -p hydra-server --features server,cluster-redis,db-mysql --bin hydra\n'),
  });
  const r = run(dir);
  assert.equal(r.status, 0, r.stdout + r.stderr);
  assert.match(r.stdout, /environment\/build\.sh=1 build\(s\)/);
});

test('FALSIFY: a build with NO database backend is refused', () => {
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS])].join('\n')),
    recipes: allRecipes('cargo build -p hydra-server --features server,cluster-redis --bin hydra\n'),
  });
  const r = run(dir);
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /selects NO database backend/);
  assert.match(r.stderr, /db-postgres \/ db-mysql/);
});

test('FALSIFY: a build with BOTH backends is refused (they are mutually exclusive)', () => {
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS])].join('\n')),
    recipes: allRecipes('cargo build -p hydra-server --features server,cluster-redis,db-postgres,db-mysql --bin hydra\n'),
  });
  const r = run(dir);
  assert.equal(r.status, 1, r.stdout + r.stderr);
  assert.match(r.stderr, /selects db-postgres AND db-mysql/);
});

test('CONTROL: the two-variant `FEATURES="…"` recipe shape is read (build.sh/release.sh)', () => {
  const recipe = [
    'if [[ "$DB_BACKEND" == "postgres" ]]; then',
    '    FEATURES="server,cluster-redis,usage-clickhouse,db-postgres"',
    'else',
    '    FEATURES="server,cluster-redis,usage-clickhouse,db-mysql"',
    'fi',
    'cargo build -p hydra-server --features "$FEATURES" --bin hydra',
    '',
  ].join('\n');
  const dir = tree({
    manifests: allManifests(['services:', node('hydra-a', [REDIS, POSTGRES_URL])].join('\n')),
    recipes: { 'build.sh': recipe, 'release.sh': recipe },
  });
  const r = run(dir);
  assert.equal(r.status, 0, r.stdout + r.stderr);
  assert.match(r.stdout, /2 build\(s\)/);
});
