#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/check_cluster_env.cjs.
 *
 * The guard backs an operator-facing ERROR: when a node has cluster wiring configured but is not a
 * cluster node, `NodeRole::from_env` prints the variable names it will NOT use — and that list is
 * `CLUSTER_ONLY_ENV`. A name missing from the table is a setting dropped in SILENCE (round 193
 * measured seven of them), so the tests pin both directions of the rule and, just as importantly,
 * the two ways this checker could become vacuous: a table it cannot parse, and a `src/cluster/`
 * with no reads to judge.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CHECKER = path.join(__dirname, 'check_cluster_env.cjs');
const REPO = path.resolve(__dirname, '..');

/**
 * A fake repository root: the owner file (table + the reads under `src/cluster/`), plus a
 * `src/main.rs` holding the reads that satisfy the "must earn its place" direction.
 */
function fixture({ table = ['HYDRA_REDIS_URL', 'HYDRA_NODE_ID'], clusterReads = ['HYDRA_REDIS_URL', 'HYDRA_NODE_ID'], crateReads = null, nonLiteral = false, wholeEnv = false } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'cluster-env-'));
  const owner = path.join(dir, 'crates/hydra-server/src/cluster/mod.rs');
  fs.mkdirSync(path.dirname(owner), { recursive: true });
  const array = table.map((n) => `    "${n}",`).join('\n');
  const reads = clusterReads.map((n) => `    let _ = std::env::var("${n}");`).join('\n');
  // The indirection shape the completeness rule cannot see through (round 194: `env::var(name)`).
  // `nonLiteral: N` writes N sites, so a case can pin that ONE record covers ONE site (not a file).
  const count = nonLiteral === true ? 1 : Number(nonLiteral) || 0;
  let indirect = '';
  for (let i = 1; i <= count; i += 1) {
    indirect += `\nfn by_name_${i}() {\n    let name = "HYDRA_ANYTHING_${i}";\n    let _ = std::env::var(name);\n}\n`;
  }
  // A whole-environment sweep: the guard cannot name what it reads either (round 195).
  if (wholeEnv) {
    indirect += '\nfn sweep() {\n    for (k, _v) in std::env::vars() {\n        let _ = k;\n    }\n}\n';
  }
  fs.writeFileSync(
    owner,
    `const CLUSTER_ONLY_ENV: [&str; ${table.length}] = [\n${array}\n];\n\n` +
      `fn from_env() {\n${reads}\n}\n${indirect}`,
  );
  // By default every table name is also read somewhere in the crate (R2 satisfied).
  const elsewhere = crateReads === null ? table : crateReads;
  fs.mkdirSync(path.join(dir, 'crates/hydra-server/src'), { recursive: true });
  fs.writeFileSync(
    path.join(dir, 'crates/hydra-server/src/main.rs'),
    `${elsewhere.map((n) => `fn _${n.toLowerCase()}() { let _ = std::env::var("${n}"); }`).join('\n')}\n`,
  );
  return dir;
}

function run(dir, { env = {} } = {}) {
  const res = spawnSync(process.execPath, [CHECKER], {
    encoding: 'utf8',
    // `CLUSTER_ENV_NOT_CLUSTER_ONLY: '{}'` / `CLUSTER_ENV_NON_LITERAL_OK: '{}'` REPLACE the built-in
    // records: a fixture tree that does not contain the repository's reads would otherwise report
    // every record as stale (measured here first — test 1 failed on exactly that before this line
    // existed). Cases that exercise records pass their own value.
    env: {
      ...process.env,
      CLUSTER_ENV_ROOT: dir,
      CLUSTER_ENV_NOT_CLUSTER_ONLY: '{}',
      CLUSTER_ENV_NON_LITERAL_OK: '{}',
      ...env,
    },
  });
  return { status: res.status, stdout: res.stdout || '', stderr: res.stderr || '' };
}

test('CONTROL: a complete table with reads everywhere passes and names the table', () => {
  const r = run(fixture());
  assert.equal(r.status, 0, `${r.status} ${r.stderr}`);
  assert.match(r.stdout, /2 cluster-only name\(s\); 2 literal read\(s\) under crates\/hydra-server\/src\/cluster\//);
  assert.match(r.stdout, /CLUSTER_ONLY_ENV is HYDRA_REDIS_URL, HYDRA_NODE_ID/);
  // ...and the OK line must NOT claim the ERROR prints it (a static scan cannot see that chain).
  assert.match(r.stdout, /NOT visible to a static scan/);
});

test('a cluster-only read that is NOT in the table is a finding (the round-193 defect)', () => {
  // Exactly what shipped: the code reads it, the list does not name it, so the operator is told
  // three variables are dropped while this one disappears without a word.
  const r = run(fixture({ table: ['HYDRA_REDIS_URL'], clusterReads: ['HYDRA_REDIS_URL', 'HYDRA_LEADER_LEASE_MS'] }));
  assert.equal(r.status, 1, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /HYDRA_LEADER_LEASE_MS is read in .*cluster\/mod\.rs:\d+ \(cluster-only code\) but is NOT in CLUSTER_ONLY_ENV/);
  assert.match(r.stderr, /drop it in silence/);
});

test('CONTROL: the same read is accepted once it is recorded as read in every mode', () => {
  const r = run(fixture({ table: ['HYDRA_REDIS_URL'], clusterReads: ['HYDRA_REDIS_URL', 'HYDRA_ROLE'] }), {
    env: { CLUSTER_ENV_NOT_CLUSTER_ONLY: JSON.stringify({ HYDRA_ROLE: 'the selector itself' }) },
  });
  assert.equal(r.status, 0, `${r.status} ${r.stderr}`);
  assert.match(r.stdout, /1 recorded non-cluster name\(s\): OK/);
});

test('a recorded exception that no longer applies is a finding (a claim that cannot expire)', () => {
  // The record's subject is not read under src/cluster/ any more: keeping it would hide the next
  // real one behind a decision nobody re-checked.
  const r = run(fixture({ table: ['HYDRA_REDIS_URL'], clusterReads: ['HYDRA_REDIS_URL'] }), {
    env: { CLUSTER_ENV_NOT_CLUSTER_ONLY: JSON.stringify({ HYDRA_ROLE: 'the selector itself' }) },
  });
  assert.equal(r.status, 1, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /NOT_CLUSTER_ONLY records HYDRA_ROLE, but that no longer applies/);
});

test('a table entry that nothing reads is a finding (a name must earn its place)', () => {
  const r = run(fixture({ table: ['HYDRA_REDIS_URL', 'HYDRA_NODE_ID'], clusterReads: ['HYDRA_REDIS_URL'], crateReads: ['HYDRA_REDIS_URL'] }));
  assert.equal(r.status, 1, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /HYDRA_NODE_ID is listed in CLUSTER_ONLY_ENV but nothing in crates\/hydra-server\/src reads it/);
});

test('a duplicate entry is a finding', () => {
  const r = run(fixture({ table: ['HYDRA_REDIS_URL', 'HYDRA_REDIS_URL'], clusterReads: ['HYDRA_REDIS_URL'], crateReads: ['HYDRA_REDIS_URL'] }));
  assert.equal(r.status, 1, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /HYDRA_REDIS_URL is listed twice in CLUSTER_ONLY_ENV/);
});

/* Round 194. The completeness rule used to match only `"HYDRA_*"` literals, so a read under
 * `src/cluster/` of a NON-HYDRA name was invisible to it — `HOSTNAME` (the node-id fallback tier)
 * was exactly that, in the shipped tree. A filter hardcoded to one prefix, inside the guard written
 * to catch filters that eat objects. */
test('a non-HYDRA literal read under src/cluster/ is judged like any other (round 194)', () => {
  const r = run(fixture({ table: ['HYDRA_REDIS_URL'], clusterReads: ['HYDRA_REDIS_URL', 'HOSTNAME'], crateReads: ['HYDRA_REDIS_URL'] }));
  assert.equal(r.status, 1, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /HOSTNAME is read in .*cluster\/mod\.rs:\d+ \(cluster-only code\) but is NOT in CLUSTER_ONLY_ENV/);

  // ...and the shipped answer for `HOSTNAME` is the recorded one: the OS always sets it, so it is
  // not something an operator configures for the cluster and cannot be "dropped wiring".
  const ok = run(fixture({ table: ['HYDRA_REDIS_URL'], clusterReads: ['HYDRA_REDIS_URL', 'HOSTNAME'], crateReads: ['HYDRA_REDIS_URL'] }), {
    env: { CLUSTER_ENV_NOT_CLUSTER_ONLY: JSON.stringify({ HOSTNAME: 'the OS hostname' }) },
  });
  assert.equal(ok.status, 0, `${ok.status} ${ok.stderr}`);
  assert.match(ok.stdout, /2 recorded non-cluster name\(s\)|1 recorded non-cluster name\(s\)/);
});

test('a NON-literal env read must be recorded (the completeness rule cannot see the name)', () => {
  const r = run(fixture({ nonLiteral: true }));
  assert.equal(r.status, 1, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /cluster\/mod\.rs:\d+ reads the environment through a NON-literal argument/);
  assert.match(r.stderr, /NON_LITERAL_READ_OK/);

  const ok = run(fixture({ nonLiteral: true }), {
    env: { CLUSTER_ENV_NON_LITERAL_OK: JSON.stringify({ 'crates/hydra-server/src/cluster/mod.rs#1': 'a loop over the table itself' }) },
  });
  assert.equal(ok.status, 0, `${ok.status} ${ok.stderr}`);
  assert.match(ok.stdout, /1 non-literal read\(s\) \(all recorded\)/);

  // The record expires when the indirection goes away: keeping it would be an unexpirable decision.
  const stale = run(fixture(), {
    env: { CLUSTER_ENV_NON_LITERAL_OK: JSON.stringify({ 'crates/hydra-server/src/cluster/mod.rs#1': 'a loop over the table itself' }) },
  });
  assert.equal(stale.status, 1, `${stale.status} ${stale.stdout}`);
  assert.match(stale.stderr, /NON_LITERAL_READ_OK records .*cluster\/mod\.rs#1, but that site is gone/);
});

/* Round 194, adversarial review of this guard: the non-literal records were keyed by FILE, so the
 * reason written for the `CLUSTER_ONLY_ENV` loop vouched for every future indirection in the same
 * file — a `env::var(SECRET_KNOB)` reading a name that is not in the table passed with exit 0 and an
 * OK line claiming every read was accounted for. Keyed by SITE, one record covers one site. */
test('one non-literal record covers ONE site, not the whole file (round 194)', () => {
  const recorded = { 'crates/hydra-server/src/cluster/mod.rs#1': 'the table loop' };

  // Two sites, one record: the second is unrecorded and must be reported BY SITE.
  const two = run(fixture({ nonLiteral: 2 }), { env: { CLUSTER_ENV_NON_LITERAL_OK: JSON.stringify(recorded) } });
  assert.equal(two.status, 1, `${two.status} ${two.stdout}`);
  assert.match(two.stderr, /cluster\/mod\.rs:\d+ reads the environment through a NON-literal argument/);
  assert.match(two.stderr, /2 non-literal read\(s\) \(1 UNRECORDED\)/);

  // CONTROL: both recorded, and the OK line stops claiming more than it checked.
  const both = run(fixture({ nonLiteral: 2 }), {
    env: {
      CLUSTER_ENV_NON_LITERAL_OK: JSON.stringify({
        'crates/hydra-server/src/cluster/mod.rs#1': 'the table loop',
        'crates/hydra-server/src/cluster/mod.rs#2': 'a second indirection, recorded on purpose',
      }),
    },
  });
  assert.equal(both.status, 0, `${both.status} ${both.stderr}`);
  assert.match(both.stdout, /2 non-literal read\(s\) \(all recorded\)/);
});

/* The OK line used to assert "the fallback ERROR names: <table>" — a chain this file never reads.
 * A fixture whose table is declared and never used printed the same sentence for all ten names. */
test('the OK line does not claim the ERROR prints the table (a static scan cannot see that chain)', () => {
  const r = run(fixture());
  assert.equal(r.status, 0, `${r.status} ${r.stderr}`);
  assert.doesNotMatch(r.stdout, /the fallback ERROR names:/);
  assert.match(r.stdout, /NOT visible to a static scan/);
  assert.match(r.stdout, /test_startup_knobs\.py .*K10/s);
});

/* Round 195: `env::vars()` walks the whole environment, and it matched NOTHING in this guard — an
 * unrecorded sweep was invisible to every rule in the file. It is now the same kind of site as an
 * `env::var(<expression>)`: it must be recorded, by site. */
test('a whole-environment sweep (env::vars) is a non-literal site that must be recorded', () => {
  const r = run(fixture({ wholeEnv: true }));
  assert.equal(r.status, 1, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /cluster\/mod\.rs:\d+ reads the environment through a NON-literal argument/);

  const ok = run(fixture({ wholeEnv: true }), {
    env: { CLUSTER_ENV_NON_LITERAL_OK: JSON.stringify({ 'crates/hydra-server/src/cluster/mod.rs#1': 'a sweep, recorded on purpose' }) },
  });
  assert.equal(ok.status, 0, `${ok.status} ${ok.stderr}`);
  assert.match(ok.stdout, /1 non-literal read\(s\) \(all recorded\)/);
});

/* Round 197: the scan read RAW lines, so a name mentioned only inside a doc comment counted as a
 * reader. Measured with a fixture: `CLUSTER_ONLY_ENV` listed `HYDRA_NOTE_ONLY`, its single occurrence
 * in the whole tree was `/// … std::env::var("HYDRA_NOTE_ONLY") …`, and the guard said OK. */
test('a name that only appears in a COMMENT has no reader (a comment is never evidence)', () => {
  const dir = fixture({
    table: ['HYDRA_REDIS_URL', 'HYDRA_NOTE_ONLY'],
    clusterReads: ['HYDRA_REDIS_URL'],
    crateReads: ['HYDRA_REDIS_URL'],
  });
  // The only mention of HYDRA_NOTE_ONLY is a doc comment.
  const main = path.join(dir, 'crates/hydra-server/src/main.rs');
  fs.appendFileSync(main, '\n/// mentions std::env::var("HYDRA_NOTE_ONLY") but never runs\nfn nothing() {}\n');
  const r = run(dir);
  assert.equal(r.status, 1, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /HYDRA_NOTE_ONLY is listed in CLUSTER_ONLY_ENV but nothing in crates\/hydra-server\/src reads it/);

  // CONTROL: the same mention in CODE is a reader.
  fs.writeFileSync(main, fs.readFileSync(main, 'utf8') + 'fn real() { let _ = std::env::var("HYDRA_NOTE_ONLY"); }\n');
  const ok = run(dir);
  assert.equal(ok.status, 0, `${ok.status} ${ok.stderr}`);
});

/* Round 198: the guard now shares `rust_blank.cjs`'s `stripCommentsAndTestItems`, which is strictly
 * stronger than the line-start heuristic it replaced: a TRAILING comment is stripped too, and a
 * `#[cfg(test)]` module is not the product reading the knob. Both directions are asserted here — the
 * second one used to be a documented KNOWN LIMIT ("would therefore need a record") rather than a rule. */
test('a TRAILING comment is not a reader either, and a cfg(test) read is not one at all', () => {
  const dir = fixture({ table: ['HYDRA_REDIS_URL', 'HYDRA_NOTE_ONLY'], clusterReads: ['HYDRA_REDIS_URL'], crateReads: ['HYDRA_REDIS_URL'] });
  const main = path.join(dir, 'crates/hydra-server/src/main.rs');
  // Trailing comment on a CODE line: the line is real, the mention is not.
  fs.appendFileSync(main, 'fn code() {} // mentions std::env::var("HYDRA_NOTE_ONLY") in passing\n');
  const trailing = run(dir);
  assert.equal(trailing.status, 1, `${trailing.status} ${trailing.stdout}`);
  assert.match(trailing.stderr, /HYDRA_NOTE_ONLY is listed in CLUSTER_ONLY_ENV but nothing in crates\/hydra-server\/src reads it/);

  // A #[cfg(test)] module that READS a name the table does not list: not a cluster-only read.
  const withTest = fixture({ table: ['HYDRA_REDIS_URL'], clusterReads: ['HYDRA_REDIS_URL'], crateReads: ['HYDRA_REDIS_URL'] });
  fs.appendFileSync(path.join(withTest, 'crates/hydra-server/src/cluster/mod.rs'),
    '\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { let _ = std::env::var("HYDRA_FROM_A_TEST"); }\n}\n');
  const tested = run(withTest);
  assert.equal(tested.status, 0, `${tested.status} ${tested.stdout}${tested.stderr}`);
});

test('CANNOT VERIFY: a tree without the table declaration (2, not 1)', () => {
  const dir = fixture();
  const owner = path.join(dir, 'crates/hydra-server/src/cluster/mod.rs');
  fs.writeFileSync(owner, 'fn from_env() { let _ = std::env::var("HYDRA_REDIS_URL"); }\n');
  const r = run(dir);
  assert.equal(r.status, 2, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /no `const CLUSTER_ONLY_ENV: \[&str; N\] = \[` declaration/);
});

test('CANNOT VERIFY: an empty table (a clean report from nothing is not a pass)', () => {
  const r = run(fixture({ table: [], clusterReads: ['HYDRA_REDIS_URL'], crateReads: [] }));
  assert.equal(r.status, 2, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /CLUSTER_ONLY_ENV parsed as empty/);
});

test('CANNOT VERIFY: src/cluster/ with no literal env read (a rule with no subject)', () => {
  const dir = fixture({ table: ['HYDRA_REDIS_URL'], clusterReads: [], crateReads: ['HYDRA_REDIS_URL'] });
  const r = run(dir);
  assert.equal(r.status, 2, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /has no literal `env::var\("NAME"\)` read — a rule with no subject is not a pass/);
});

test('CANNOT VERIFY: the owner file is missing (the table has exactly one home)', () => {
  const dir = fixture();
  fs.rmSync(path.join(dir, 'crates/hydra-server/src/cluster/mod.rs'));
  const r = run(dir);
  assert.equal(r.status, 2, `${r.status} ${r.stdout}`);
  assert.match(r.stderr, /cluster\/mod\.rs is missing — this guard owns the table in it/);
});

test('an unknown argument is refused', () => {
  const res = spawnSync(process.execPath, [CHECKER, '--wat'], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  assert.match(res.stderr, /unknown argument: --wat/);
});

test('the real repository tree passes (the table names exactly the cluster topology)', () => {
  const res = spawnSync(process.execPath, [CHECKER], { encoding: 'utf8', env: { ...process.env, CLUSTER_ENV_ROOT: '' } });
  assert.equal(res.status, 0, `${res.status} ${res.stdout}${res.stderr}`);
  // The count is DERIVED from the declaration, not hard-coded: a literal here rots the moment the
  // table legitimately changes (it went 10 -> 7 when ADR-0001 retired the role selector's names),
  // and a rotted literal fails for the wrong reason. ADR-0001 replaced the role selector with the
  // member list, so asserting on the NEW names below — rather than on a name that is merely still
  // present — is what makes this a check on the decision, not on the count.
  const decl = fs.readFileSync(path.join(REPO, 'crates/hydra-server/src/cluster/mod.rs'), 'utf8')
    .match(/const\s+CLUSTER_ONLY_ENV\s*:\s*\[&str;\s*(\d+)\]\s*=\s*\[([\s\S]*?)\];/);
  assert.ok(decl, 'CLUSTER_ONLY_ENV declaration not found');
  const declared = [...decl[2].matchAll(/"([A-Z0-9_]+)"/g)].map((m) => m[1]);
  assert.match(res.stdout, new RegExp(`${declared.length} cluster-only name\\(s\\)`));
  assert.match(res.stdout, /HYDRA_CLUSTER_PEERS/);
  assert.match(res.stdout, /HYDRA_CLUSTER_ID/);
  assert.match(res.stdout, /HYDRA_ARACHNE_LISTEN/);
});

test('the shipped table still matches the Rust test that pins it', () => {
  // Two owners of the same list would be worse than one: the unit test in `cluster/mod.rs` asserts
  // the exact names, and this reads the same declaration, so a change to only one of them shows up.
  const src = fs.readFileSync(path.join(REPO, 'crates/hydra-server/src/cluster/mod.rs'), 'utf8');
  const decl = src.match(/const\s+CLUSTER_ONLY_ENV\s*:\s*\[&str;\s*(\d+)\]\s*=\s*\[([\s\S]*?)\];/);
  assert.ok(decl, 'CLUSTER_ONLY_ENV declaration not found');
  const names = [...decl[2].matchAll(/"([A-Z0-9_]+)"/g)].map((m) => m[1]);
  assert.equal(names.length, Number(decl[1]), 'the declared length must match the number of entries');
  assert.equal(new Set(names).size, names.length, 'no duplicates');
});
