#!/usr/bin/env node
'use strict';
/**
 * Tests for `check_redis_pool.cjs` — BOTH directions, plus the honest "cannot verify" exit.
 *
 * Every fixture is a real temporary tree fed to the real script through its `CRP_*`
 * overrides, so the assertions are about the script's behaviour, not about a re-implementation
 * of it. The negative cases are the point: a guard that only ever sees a clean tree proves
 * nothing (this session has shipped two guards that were silently vacuous).
 */
const { execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const SCRIPT = path.join(__dirname, 'check_redis_pool.cjs');
// The single owner, relative to the fixture root (the fixtures write this file).
const OWNER = 'crates/hydra-server/src/redis/mod.rs';
const failures = [];

function check(label, ok, detail = '') {
  console.log(`   ${ok ? 'PASS' : 'FAIL'}  ${label}${detail ? '  — ' + detail : ''}`);
  if (!ok) failures.push(label);
}

function tree(files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'crp-'));
  for (const [rel, body] of Object.entries(files)) {
    const full = path.join(root, rel);
    fs.mkdirSync(path.dirname(full), { recursive: true });
    fs.writeFileSync(full, body);
  }
  return root;
}

function run(files) {
  const root = tree(files);
  try {
    const out = execFileSync('node', [SCRIPT], {
      encoding: 'utf8',
      env: {
        ...process.env,
        CRP_CRATES: path.join(root, 'crates'),
        // ABSOLUTE, and inside the same temporary tree as CRP_CRATES: a relative owner would
        // point at the real repository while the scan walked the fixture.
        CRP_OWNER: path.join(root, 'crates/hydra-server/src/redis/mod.rs'),
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    return { code: 0, out };
  } catch (e) {
    return { code: e.status, out: `${e.stdout || ''}${e.stderr || ''}` };
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

const GOOD_OWNER = `
fn pool_from_config(config: Config, perf: PerformanceConfig, size: usize) -> Result<Pool, RedisError> {
    Pool::new(
        config,
        Some(perf),
        Some(connection_config()),
        Some(reconnect_policy()),
        size,
    )
    .map_err(RedisError::from)
}
`;

// 1) the real shape: one owner, one call, Some(policy)
let r = run({ 'crates/hydra-server/src/redis/mod.rs': GOOD_OWNER });
check('R1/R2: a single owner passing Some(policy) passes', r.code === 0, `exit=${r.code} ${r.out.trim().slice(0, 90)}`);

// 2) R2 — `None` policy in the owner (the P1 itself)
r = run({
  'crates/hydra-server/src/redis/mod.rs': GOOD_OWNER.replace(
    'Some(reconnect_policy()),',
    'None,',
  ),
});
check(
  'R2: a `None` policy in the owner is a violation naming the line',
  r.code === 1 && /4th `Pool::new` argument is `None`/.test(r.out),
  `exit=${r.code} ${r.out.trim().split('\n')[0].slice(0, 110)}`,
);

// 3) R1 — a second call site elsewhere in the tree (the four that really existed)
r = run({
  'crates/hydra-server/src/redis/mod.rs': GOOD_OWNER,
  'crates/hydra-server/tests/common/mod.rs':
    'fn p(url: &str) -> Pool { let cfg = Config::from_url(url).unwrap(); Pool::new(cfg, None, None, None, 1).unwrap() }\n',
});
check(
  'R1: a `Pool::new` outside the owner is a violation that names the file',
  r.code === 1 && /tests\/common\/mod\.rs:1/.test(r.out) && /outside the single owner/.test(r.out),
  `exit=${r.code} ${r.out.trim().split('\n')[0].slice(0, 120)}`,
);

// 4) a comment mentioning the constructor is NOT a call site (the owner documents it)
r = run({
  'crates/hydra-server/src/redis/mod.rs':
    '// `Pool::new(config, perf, connection, policy, size)` takes the policy separately\n' + GOOD_OWNER,
});
check('a comment that mentions `Pool::new` is not counted as a call site', r.code === 0, `exit=${r.code}`);

// 5) too few arguments = the same defect by omission
r = run({ 'crates/hydra-server/src/redis/mod.rs': 'fn f() { Pool::new(config, Some(perf), Some(conn)) }\n' });
check(
  'R2: a `Pool::new` with no 4th argument is a violation',
  r.code === 1 && /has 3 argument/.test(r.out),
  `exit=${r.code} ${r.out.trim().slice(0, 110)}`,
);

// 6) a tree with no call at all must be CANNOT VERIFY, never a pass
r = run({ 'crates/hydra-server/src/redis/mod.rs': 'fn nothing_here() {}\n' });
check('a scan that matches no call exits 2 (never 0)', r.code === 2, `exit=${r.code} ${r.out.trim().slice(0, 80)}`);

// 7) a missing owner file must be CANNOT VERIFY
r = run({ 'crates/hydra-server/src/other.rs': GOOD_OWNER });
check('a missing owner file exits 2 (never 0)', r.code === 2, `exit=${r.code} ${r.out.trim().slice(0, 80)}`);

// 8) the argument splitter must not be fooled by nested parens/commas in earlier arguments
r = run({
  'crates/hydra-server/src/redis/mod.rs':
    'fn f() { Pool::new(cfg.with(x(y, z)), Some(perf), Some(conn), Some(policy), 2) }\n',
});
check(
  'the 4th argument is found past nested parens and commas',
  r.code === 0,
  `exit=${r.code} ${r.out.trim().slice(0, 90)}`,
);

// 9) ...and the same shape with `None` in the 4th slot is still caught
r = run({
  'crates/hydra-server/src/redis/mod.rs':
    'fn f() { Pool::new(cfg.with(x(y, z)), Some(perf), Some(conn), None, 2) }\n',
});
check('...and `None` in the 4th slot after nested parens is still a violation', r.code === 1, `exit=${r.code}`);

/* --- round 118: the two bypasses the textual rule used to miss ---------------------- */

// R1's "is this a call or a comment?" test was `line.slice(0, idx).includes('//')`, so a call
// site sharing its line with a URL was skipped entirely. Measured 2026-09-30: control (no `//`
// on the line) exited 1, the same violation with `redis://…` on the line exited 0.
r = run({
  [OWNER]: GOOD_OWNER,
  'crates/hydra-server/src/other.rs':
    'fn f() { let c = Config::from_url("redis://127.0.0.1:6379"); let p = Pool::new(c, None, None, None, 4); }\n',
});
check(
  'a `Pool::new` on a line that also contains `//` (a URL) is still a violation',
  r.code === 1 && /other\.rs:1: `Pool::new` outside the single owner/.test(r.out),
  `exit=${r.code} out=${r.out.trim().slice(0, 120)}`,
);
// CONTROL: the identical file without the URL must behave the same way — otherwise the case
// above could be passing for an unrelated reason.
r = run({
  [OWNER]: GOOD_OWNER,
  'crates/hydra-server/src/other.rs':
    'fn f() { let c = Config::from_url("redis-127.0.0.1:6379"); let p = Pool::new(c, None, None, None, 4); }\n',
});
check(
  'CONTROL: the same file without the `//` is a violation too',
  r.code === 1,
  `exit=${r.code}`,
);
// A comment that merely mentions the constructor must STILL not be a violation (the original
// reason the heuristic existed).
r = run({
  [OWNER]: GOOD_OWNER,
  'crates/hydra-server/src/other.rs':
    '// never call Pool::new outside the owner\n/// `Pool::new` is the owner\u2019s job\nfn f() {}\n',
});
check(
  'CONTROL: comments mentioning `Pool::new` are still not violations',
  r.code === 0,
  `exit=${r.code} out=${r.out.trim().slice(0, 120)}`,
);

// R1b: fred's Builder::build_pool(size) passes `self.policy` (None by default) internally, so
// it builds a pool that never re-dials without ever writing the text `Pool::new`.
r = run({
  [OWNER]: GOOD_OWNER,
  'crates/hydra-server/src/other.rs':
    'fn f() -> Result<Pool, Error> { Builder::from_config(cfg).build_pool(2) }\n',
});
check(
  '`.build_pool(` outside the owner is a violation (fred\'s Builder bypasses the policy)',
  r.code === 1 && /\.build_pool\(` outside the single owner/.test(r.out),
  `exit=${r.code} out=${r.out.trim().slice(0, 120)}`,
);
// ...and it is fine INSIDE the owner, where the policy is supplied.
r = run({
  [OWNER]: GOOD_OWNER + '\nfn g() -> Result<Pool, Error> { Builder::from_config(cfg).build_pool(2) }\n',
});
check(
  'CONTROL: `.build_pool(` inside the owner is accepted',
  r.code === 0,
  `exit=${r.code} out=${r.out.trim().slice(0, 120)}`,
);

// R1c: renaming the type hides every `Pool::new` from a textual search.
r = run({
  [OWNER]: GOOD_OWNER,
  'crates/hydra-server/src/other.rs': 'use fred::clients::Pool as P;\nfn f() { let _ = P::new(c, None, None, None, 1); }\n',
});
check(
  '`Pool as <alias>` is a violation (it hides `Pool::new` from the rule)',
  r.code === 1 && /renames the pool type/.test(r.out),
  `exit=${r.code} out=${r.out.trim().slice(0, 120)}`,
);

console.log();
if (failures.length) {
  console.log(`redis-pool guard tests: FAILED (${failures.length}): ${failures.join('; ')}`);
  process.exit(1);
}
console.log('redis-pool guard tests: PASSED (17 assertions, both directions + cannot-verify)');

/* Round 130: R2's "the 4th argument must be `Some(...)`" was satisfied by an argument that is not
 * the policy: `splitArgs` walked the raw text, so a comma inside a TOP-LEVEL string argument shifted
 * every later slot. Measured: `Pool::new(cfg, "a,b", Some(conn), None, 2)` produced
 * ["cfg","\"a","b\"","Some(conn)","None","2"] — `parts[3] === "Some(conn)"` while the real 4th
 * argument is `None`, and the guard printed OK.
 *
 * NOTE (a negative result worth keeping): the shape the review reported — a comma inside a NESTED
 * call such as `label("a,b")` — was never dangerous, because the depth counter already covers
 * parentheses. Only a comma at depth 0 can shift the slots, which is what the blanked copy fixes. */
r = run({
  [OWNER]: 'fn f() { Pool::new(cfg, "a,b", Some(conn), None, 2) }\n',
});
check(
  'a comma inside a top-level string argument does not hide a `None` policy',
  r.code === 1 && /4th `Pool::new` argument is `None`/.test(r.out),
  `exit=${r.code} out=${r.out.trim().slice(0, 140)}`,
);

r = run({
  [OWNER]: 'fn f() { Pool::new(cfg, "a,b", Some(conn), Some(policy), 2) }\n',
});
check(
  'CONTROL: the same call with a real policy in the 4th slot passes',
  r.code === 0,
  `exit=${r.code} out=${r.out.trim().slice(0, 140)}`,
);
