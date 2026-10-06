#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/check_documented_env.cjs.
 *
 * The checker exists because `dev-docs/ops.md` advertised a log-filter alias
 * (`HYDRA_LOG`) that no code has ever read — a documented knob that does nothing,
 * which an operator would reach for exactly when logging matters most.
 *
 * Two REGRESSION cases matter as much as the happy path, because the first two
 * versions of the checker both passed their own falsification:
 *   * evidence must not come from prose (`.md`) — the document satisfied itself;
 *   * evidence must not come from comments — the checker's own header, which
 *     quotes the row under test, counted as proof.
 * Both are pinned below.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CHECKER = path.join(__dirname, 'check_documented_env.cjs');
const REPO = path.resolve(__dirname, '..');

function fixture({ docs, code = {}, minRows = '2' } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'docenv-'));
  const docsPath = path.join(dir, 'ops.md');
  fs.writeFileSync(docsPath, docs ?? defaultDocs());
  const codeRoot = path.join(dir, 'tree');
  for (const [rel, content] of Object.entries(code)) {
    const full = path.join(codeRoot, rel);
    fs.mkdirSync(path.dirname(full), { recursive: true });
    fs.writeFileSync(full, content);
  }
  fs.mkdirSync(codeRoot, { recursive: true });
  return { dir, docs: docsPath, codeRoot };
}

function defaultDocs({ rows } = {}) {
  const body = rows ?? [
    '| `HYDRA_ADMIN_TOKEN` | — | admin bearer token. |',
    '| `HYDRA_DB_URL` | `sqlite://…` | database URL. |',
    '| `RUST_LOG` | `info` | tracing filter. |',
  ];
  return `# ops\n\n| Variable | Default | Notes |\n|---|---|---|\n${body.join('\n')}\n`;
}

function run(fx, extraArgs = [], env = {}) {
  const args = [`--docs=${fx.docs}`, `--code-root=${fx.codeRoot}`, ...extraArgs];
  const res = spawnSync(process.execPath, [CHECKER, ...args], {
    encoding: 'utf8',
    env: {
      ...process.env,
      DOC_ENV_MIN_ROWS: '2',
      // The PROSE-only record list is REPLACED (not extended) by this override, so a fixture document
      // never inherits the repository's ten records — and never trips the staleness check for names it
      // does not contain. Cases that need a record pass their own JSON.
      DOC_ENV_PROSE_OK: '{}',
      ...env,
    },
  });
  return { status: res.status, stdout: res.stdout || '', stderr: res.stderr || '' };
}

const WIRED = {
  'src/main.rs': 'fn main() { let t = std::env::var("HYDRA_ADMIN_TOKEN"); let d = std::env::var("HYDRA_DB_URL"); }\n',
  'environment/docker-compose.yml': 'services:\n  hydra:\n    environment:\n      RUST_LOG: info\n',
};

test('a table whose names are all wired passes', () => {
  const r = run(fixture({ code: WIRED }));
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /3 distinct documented env name\(s\)/);
});

test('a documented-but-unread name fails and prints the row', () => {
  const r = run(fixture({
    code: WIRED,
    docs: defaultDocs({ rows: [...defaultDocs().split('\n').slice(5, 8), '| `HYDRA_GHOST_VAR` | `1` | documented, never read. |'] }),
  }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /1 documented env name\(s\) have no READ site/);
  assert.match(r.stderr, /HYDRA_GHOST_VAR/);
  assert.match(r.stderr, /documented, never read/);
});

test('REGRESSION: a name mentioned only in a markdown file is still not wired', () => {
  // The first version scanned .md, so the document satisfied its own claim.
  const r = run(fixture({
    code: { ...WIRED, 'dev-docs/notes.md': 'We set HYDRA_GHOST_VAR in production.\n' },
    docs: defaultDocs({ rows: [...defaultDocs().split('\n').slice(5, 8), '| `HYDRA_GHOST_VAR` | `1` | documented. |'] }),
  }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /HYDRA_GHOST_VAR/);
});

test('REGRESSION: a name mentioned only in a code comment is still not wired', () => {
  // The second version scanned comments, so the checker's own header quoting the
  // row under test counted as proof that the variable was read.
  const r = run(fixture({
    code: {
      ...WIRED,
      'src/config.rs': '// historically we also accepted HYDRA_GHOST_VAR here\n/* see HYDRA_GHOST_VAR in the changelog */\n',
      'scripts/legacy.sh': '# used to export HYDRA_GHOST_VAR\n',
      'environment/docker-compose.yml': 'services:\n  hydra:\n    environment:\n      RUST_LOG: info  # HYDRA_GHOST_VAR was removed\n',
    },
    docs: defaultDocs({ rows: [...defaultDocs().split('\n').slice(5, 8), '| `HYDRA_GHOST_VAR` | `1` | documented. |'] }),
  }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /HYDRA_GHOST_VAR/);
});

test('a constant that CARRIES the name and is READ through it is evidence', () => {
  // The name travels through a constant (`pub const TLS_ENV: &str = "HYDRA_TLS_LISTEN"`), the shape
  // the real tree uses in `listeners.rs`. Requiring a literal `env::var("HYDRA_…")` would call those
  // unread; requiring only the declaration would be a WEAK predicate (see the next case); requiring
  // merely "used as an argument" was ALSO too weak (round 134 — `validate(TLS_ENV)` and a `format!`
  // argument are not reads, and both exist in the real tree). So the hop must END in a read:
  // `env::var(CONST)` or `helper(CONST, …)` where the helper reads the environment.
  const r = run(fixture({
    code: {
      'src/listeners.rs':
        'pub const TLS_ENV: &str = "HYDRA_TLS_LISTEN";\nfn read() -> Option<String> { std::env::var(TLS_ENV).ok() }\n',
    },
    docs: defaultDocs({ rows: ['| `HYDRA_TLS_LISTEN` | unset | tls listener. |', '| `HYDRA_OTHER` | x | other. |'] }),
  }));
  assert.equal(r.status, 1); // HYDRA_OTHER has no read site
  assert.doesNotMatch(r.stderr, /HYDRA_TLS_LISTEN/, `a carrier READ through env::var must count:\n${r.stderr}`);
});

test('REGRESSION: a session-118 hole — a compose `environment:` assignment is NOT a read', () => {
  // Measured 2026-09-30 before the fix: this exact fixture (a table row plus a compose
  // assignment, with NOTHING reading the variable) printed OK / exit 0. A compose file WRITES the
  // variable; only code reading it proves the operator's setting does something.
  const r = run(fixture({
    code: { 'environment/docker-compose.yml': 'services:\n  hydra:\n    environment:\n      HYDRA_FOO: "1"\n' },
    docs: defaultDocs({ rows: ['| `HYDRA_FOO` | `1` | nothing reads this. |', '| `HYDRA_ADMIN_TOKEN` | — | token. |'] }),
  }));
  assert.equal(r.status, 1, `a compose assignment was accepted as a read:\n${r.stdout}`);
  assert.match(r.stderr, /HYDRA_FOO/);
});

test('a name passed to a helper as a literal argument IS a read', () => {
  // Most knobs in this tree are read through helpers that take the variable NAME as `&str`
  // (`env_positive_u32("HYDRA_TENANT_API_LOCKOUT_SECS", 900)`), so the inline `env::var("X")`
  // shape alone would report those as unread.
  const r = run(fixture({
    code: { 'src/tenant_api.rs': 'fn f() { let n = env_positive_u32("HYDRA_LOCKOUT", 900); let _ = n; }\n' },
    docs: defaultDocs({ rows: ['| `HYDRA_LOCKOUT` | `900` | lockout. |', '| `HYDRA_ADMIN_TOKEN` | — | token. |'] }),
  }));
  assert.equal(r.status, 1);
  assert.doesNotMatch(r.stderr, /HYDRA_LOCKOUT/, `a helper argument must count as a read:\n${r.stderr}`);
});

test('a log message that merely begins with the name is NOT a read', () => {
  const r = run(fixture({
    code: { 'src/main.rs': 'fn f() { warn!("HYDRA_FOO is unset"); }\n' },
    docs: defaultDocs({ rows: ['| `HYDRA_FOO` | `1` | never read. |', '| `HYDRA_ADMIN_TOKEN` | — | token. |'] }),
  }));
  assert.equal(r.status, 1, `a warning about the name was accepted as a read:\n${r.stdout}`);
});

test('an UNUSED constant holding the name is NOT a read', () => {
  const r = run(fixture({
    code: { 'src/listeners.rs': 'pub const TLS_ENV: &str = "HYDRA_TLS_LISTEN";\n' },
    docs: defaultDocs({ rows: ['| `HYDRA_TLS_LISTEN` | unset | tls listener. |', '| `HYDRA_ADMIN_TOKEN` | — | token. |'] }),
  }));
  assert.equal(r.status, 1, `a declared-but-unused constant was accepted as a read:\n${r.stdout}`);
});

test('a Python/JS read is evidence too', () => {
  const r = run(fixture({
    code: {
      'tools/x.py': 'import os\nv = os.environ["HYDRA_PY_VAR"]\n',
      'admin-ui/app.js': 'const a = process.env.HYDRA_JS_VAR;\n',
    },
    docs: defaultDocs({ rows: ['| `HYDRA_PY_VAR` | x | py. |', '| `HYDRA_JS_VAR` | x | js. |'] }),
  }));
  assert.equal(r.status, 0, r.stderr);
});

test('both names in one cell are checked (the RUST_LOG / HYDRA_LOG shape)', () => {
  const r = run(fixture({
    code: WIRED,
    docs: defaultDocs({ rows: ['| `RUST_LOG` / `HYDRA_GHOST_VAR` | `info` | filter. |', '| `HYDRA_ADMIN_TOKEN` | — | token. |'] }),
  }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /HYDRA_GHOST_VAR/);
  assert.doesNotMatch(r.stderr, /^\s*RUST_LOG$/m);
});

test('a row that says outright the variable is not read is skipped (honest prose)', () => {
  const r = run(fixture({
    code: WIRED,
    docs: defaultDocs({ rows: ['| `HYDRA_EDGE_TLS` | — | never implemented, code does not read it (see §5). |', '| `HYDRA_ADMIN_TOKEN` | — | token. |', '| `HYDRA_DB_URL` | — | db. |'] }),
  }));
  assert.equal(r.status, 0, r.stderr);
});

test('non-environment constants in a first column are not treated as promises', () => {
  const r = run(fixture({
    code: WIRED,
    docs: defaultDocs({ rows: ['| `SOME_INTERNAL_CONST` | — | not an env var. |', '| `HYDRA_ADMIN_TOKEN` | — | token. |', '| `HYDRA_DB_URL` | — | db. |'] }),
  }));
  assert.equal(r.status, 0, r.stderr);
});

test('a table smaller than the floor is exit 2, never a silent pass', () => {
  const r = run(fixture({ code: WIRED, docs: defaultDocs({ rows: ['| `HYDRA_ADMIN_TOKEN` | — | token. |'] }) }));
  assert.equal(r.status, 2);
  assert.match(r.stderr, /CANNOT SCAN: only 1 table row\(s\) with a backticked name found/);
});

test('a missing docs file is exit 2', () => {
  const fx = fixture({ code: WIRED });
  const res = spawnSync(process.execPath, [CHECKER, `--docs=${path.join(fx.dir, 'nope.md')}`, `--code-root=${fx.codeRoot}`], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  assert.match(res.stderr, /CANNOT SCAN: docs file not found/);
});

test('a missing code root is exit 2', () => {
  const fx = fixture({ code: WIRED });
  const res = spawnSync(process.execPath, [CHECKER, `--docs=${fx.docs}`, `--code-root=${path.join(fx.dir, 'nope')}`], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  assert.match(res.stderr, /CANNOT SCAN: code root not found/);
});

/**
 * Round 124: WHERE the evidence lives matters as much as its shape.
 *
 * Measured before this fix: a tree whose ONLY mention of a documented name was a string inside the
 * checker's own fixture file (`scripts/check_documented_env.test.cjs`) was reported as wired — so
 * deleting the real read from the product would not have been noticed for that name. The rule is
 * about code an operator RUNS.
 */
test('REGRESSION: a mention inside a TEST artifact is not a read', () => {
  const rows = ['| `HYDRA_GHOST_VAR` | `1` | documented. |'];
  const r = run(fixture({
    code: { 'scripts/check_documented_env.test.cjs': 'const f = require("./x");\nconst y = f("HYDRA_GHOST_VAR");\n' },
    docs: defaultDocs({ rows }),
  }), [], { DOC_ENV_MIN_ROWS: '1' });
  assert.equal(r.status, 1, `a fixture file proved that the variable is read:\n${r.stdout}`);
  assert.match(r.stderr, /HYDRA_GHOST_VAR/);
});

test('CONTROL: the same mention in non-test code IS a read', () => {
  const r = run(fixture({
    code: { 'src/config.cjs': 'const y = f("HYDRA_GHOST_VAR");\n' },
    docs: defaultDocs({ rows: ['| `HYDRA_GHOST_VAR` | `1` | documented. |'] }),
  }), [], { DOC_ENV_MIN_ROWS: '1' });
  assert.equal(r.status, 0, r.stderr);
});

test('a `#[cfg(test)]` unit test calling the helper is NOT a read', () => {
  const body = 'fn knob() -> u32 { env_positive_u32("HYDRA_GHOST_VAR", 1) }\n';
  const inTests = '#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { env_positive_u32("HYDRA_GHOST_VAR", 1); }\n}\n';
  const ghost = { rows: ['| `HYDRA_GHOST_VAR` | `1` | documented. |'] };
  const bad = run(fixture({ code: { 'src/knobs.rs': inTests }, docs: defaultDocs(ghost) }), [], { DOC_ENV_MIN_ROWS: '1' });
  assert.equal(bad.status, 1, `a #[cfg(test)] item proved that the variable is read:\n${bad.stdout}`);
  // CONTROL: the same call OUTSIDE the test module is a genuine read.
  const good = run(fixture({ code: { 'src/knobs.rs': body }, docs: defaultDocs(ghost) }), [], { DOC_ENV_MIN_ROWS: '1' });
  assert.equal(good.status, 0, good.stderr);
});

test('unknown arguments are rejected and --help works', () => {
  const bad = spawnSync(process.execPath, [CHECKER, '--wat'], { encoding: 'utf8' });
  assert.equal(bad.status, 2);
  assert.match(bad.stderr, /unknown argument: --wat/);
  const help = spawnSync(process.execPath, [CHECKER, '--help'], { encoding: 'utf8' });
  assert.equal(help.status, 0);
  assert.match(help.stdout, /check_documented_env/);
});

/**
 * Round 134: a "carrier constant" only counts when something READS the environment through it.
 *
 * The first version accepted any use as a call argument. Measured on the real tree: of the three
 * uses of the listeners' constants, one is `validate(TLS_LISTEN_ENV, v)` (a validation call) and one
 * is a `format!("{}={}", LISTEN_ENV, …)` argument — neither reads anything, yet either would have
 * been accepted as proof that the variable is read.
 */
test('a carrier constant that is only VALIDATED or PRINTED is not a read', () => {
  const rows = ['| `HYDRA_GHOST_VAR` | `1` | documented. |'];
  const r = run(fixture({
    code: {
      'src/listeners.rs':
        'pub const GHOST_ENV: &str = "HYDRA_GHOST_VAR";\n' +
        'fn validate(name: &str, v: &str) { let _ = (name, v); }\n' +
        'fn plan(v: &str) { validate(GHOST_ENV, v); let _ = format!("{}={}", GHOST_ENV, v); }\n',
    },
    docs: defaultDocs({ rows }),
  }), [], { DOC_ENV_MIN_ROWS: '1' });
  assert.equal(r.status, 1, `a validation/format use was accepted as a read:\n${r.stdout}`);
  assert.match(r.stderr, /HYDRA_GHOST_VAR/);
});

test('CONTROL: `env::var(CONST)` through the carrier IS a read', () => {
  const rows = ['| `HYDRA_GHOST_VAR` | `1` | documented. |'];
  const r = run(fixture({
    code: {
      'src/listeners.rs':
        'pub const GHOST_ENV: &str = "HYDRA_GHOST_VAR";\n' +
        'fn read() -> Option<String> { std::env::var(GHOST_ENV).ok() }\n',
    },
    docs: defaultDocs({ rows }),
  }), [], { DOC_ENV_MIN_ROWS: '1' });
  assert.equal(r.status, 0, r.stderr);
});

test('CONTROL: a carrier passed to a helper that reads the environment IS a read', () => {
  const rows = ['| `HYDRA_GHOST_VAR` | `1` | documented. |'];
  const r = run(fixture({
    code: {
      'src/knobs.rs':
        'pub const GHOST_ENV: &str = "HYDRA_GHOST_VAR";\n' +
        'fn env_positive_u32(key: &str, default: u32) -> u32 { std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default) }\n' +
        'fn f() -> u32 { env_positive_u32(GHOST_ENV, 5) }\n',
    },
    docs: defaultDocs({ rows }),
  }), [], { DOC_ENV_MIN_ROWS: '1' });
  assert.equal(r.status, 0, r.stderr);
});

test('the shipped ops.md table is fully wired', () => {
  // NO override here on purpose: this case runs against the real document, whose ten prose-only names
  // are recorded IN THE GUARD (round 180 audited each one) — that is exactly the state being asserted.
  const res = spawnSync(process.execPath, [CHECKER], { encoding: 'utf8' });
  assert.equal(res.status, 0, `the operator runbook promises an env var nothing reads:\n${res.stderr}`);
  assert.match(res.stdout, /distinct documented env name\(s\) \(\d+ more appear in prose and are recorded as needing no read site\) \(\d+ occurrence\(s\)\) in dev-docs\/ops\.md all have a READ site in code/);
});

test('the shipped docs no longer advertise the dead HYDRA_LOG alias', () => {
  const ops = fs.readFileSync(path.join(REPO, 'dev-docs', 'ops.md'), 'utf8');
  const rows = ops.split('\n').filter((l) => l.trimStart().startsWith('|'));
  const claims = rows.filter((l) => /`HYDRA_LOG`/.test(l) && !/not read|未读取/i.test(l));
  assert.equal(claims.length, 0, `HYDRA_LOG is asserted as a knob again: ${claims[0]}`);
});

/* Round 160: the floor counts DISTINCT names, not occurrences. A name repeated across rows used to
 * satisfy `MIN_ROWS` on its own, while the OK line called the occurrence count "N documented env
 * name(s)". Measured on the real table: 44 occurrences over 40 distinct names. */
test('one env name repeated 25× does NOT satisfy the distinct-name floor', () => {
  const rows = Array.from({ length: 25 }, (_, i) => `| \`HYDRA_ADMIN_TOKEN\` | \`v${i}\` | repeated |`);
  const r = run(fixture({ code: WIRED, docs: defaultDocs({ rows }) }), [], { DOC_ENV_MIN_ROWS: '20' });
  assert.notEqual(r.status, 0, `expected a refusal, got status=${r.status}`);
  assert.match(r.stderr, /only 1 DISTINCT env name\(s\) checked/);
});

/* Round 180: the table rule only covers FIRST-COLUMN names, so a knob documented in prose (a note, a
 * shell example, another row's description) was invisible to it — measured on the real ops.md: 10 such
 * names, each audited by hand this round and recorded in the guard with the reason it needs no read
 * site. A NEW prose-only name is now a finding, and a record that no longer applies is one too. */
test('a prose-only env name that is not recorded is a finding', () => {
  const docs = defaultDocs({ rows: [
    '| `HYDRA_ADMIN_TOKEN` | — | admin bearer token. |',
    '| `HYDRA_DB_URL` | `sqlite://…` | database URL. |',
  ] }) + '\nThe rotation procedure also passes `HYDRA_ROTATION_PROBE_VERSION=7` on the command line.\n';
  const r = run(fixture({ docs, code: WIRED }));
  assert.equal(r.status, 1, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stderr, /HYDRA_ROTATION_PROBE_VERSION/);
  assert.match(r.stderr, /OUTSIDE the table's first column/);
});

test('CONTROL: the same name RECORDED passes, and the record count is reported', () => {
  const docs = defaultDocs({ rows: [
    '| `HYDRA_ADMIN_TOKEN` | — | admin bearer token. |',
    '| `HYDRA_DB_URL` | `sqlite://…` | database URL. |',
  ] }) + '\nThe rotation procedure also passes `HYDRA_ROTATION_PROBE_VERSION=7` on the command line.\n';
  const r = run(fixture({ docs, code: WIRED }), [], {
    DOC_ENV_PROSE_OK: JSON.stringify({ HYDRA_ROTATION_PROBE_VERSION: 'named in the procedure prose only' }),
  });
  assert.equal(r.status, 0, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stdout, /1 more appear in prose and are recorded as needing no read site/);
});

test('a PROSE record that no longer applies is a finding (a stale claim)', () => {
  const r = run(fixture({ code: WIRED }), [], {
    DOC_ENV_PROSE_OK: JSON.stringify({ HYDRA_GHOST: 'was prose-only once' }),
  });
  assert.equal(r.status, 1, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stderr, /PROSE_ONLY_OK records 1 name\(s\) that no longer apply/);
  assert.match(r.stderr, /HYDRA_GHOST/);
});

test('a recorded name that GAINS a table row makes the record stale', () => {
  const docs = defaultDocs({ rows: [
    '| `HYDRA_ADMIN_TOKEN` | — | admin bearer token. |',
    '| `HYDRA_DB_URL` | `sqlite://…` | database URL. |',
    '| `HYDRA_GHOST` | — | now a real row. |',
  ] });
  const r = run(fixture({ docs, code: WIRED }), [], {
    DOC_ENV_PROSE_OK: JSON.stringify({ HYDRA_GHOST: 'was prose-only once' }),
  });
  assert.equal(r.status, 1, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stderr, /no longer apply/);
});

/* Round 182: READ_BY_DEPENDENCY says "no literal read is needed, a DEPENDENCY reads it". The dangerous
 * direction is a DEAD exemption: the tree grew a literal read site, so the entry exempts nothing and
 * silently hides that read. That direction is a finding (in fixtures too); the other direction — the
 * name left the documented table — is a note, because a focused fixture document has three rows. */
test('a dependency exemption whose name GAINED a literal read site is a finding', () => {
  const docs = defaultDocs({ rows: [
    '| `HYDRA_ADMIN_TOKEN` | — | admin bearer token. |',
    '| `HYDRA_DB_URL` | `sqlite://…` | database URL. |',
  ] });
  const r = run(fixture({
    docs,
    code: {
      'src/main.rs': 'fn main() { let t = std::env::var("HYDRA_ADMIN_TOKEN"); let d = std::env::var("HYDRA_DB_URL");'
        + ' let l = std::env::var("HYDRA_DEP_GHOST"); }\n',
    },
  }), [], { DOC_ENV_READ_BY_DEPENDENCY: JSON.stringify({ HYDRA_DEP_GHOST: 'used to be read by a dependency' }) });
  assert.equal(r.status, 1, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stderr, /HYDRA_DEP_GHOST is exempted as dependency-read, but this tree HAS a literal READ site/);
});

test('CONTROL: a dependency exemption with no literal read site passes', () => {
  const docs = defaultDocs({ rows: [
    '| `HYDRA_ADMIN_TOKEN` | — | admin bearer token. |',
    '| `HYDRA_DB_URL` | `sqlite://…` | database URL. |',
  ] });
  const r = run(fixture({ docs, code: WIRED }), [], {
    DOC_ENV_READ_BY_DEPENDENCY: JSON.stringify({ HYDRA_DEP_GHOST: 'read by a dependency, never as a literal' }),
  });
  assert.equal(r.status, 0, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stdout, /note: READ_BY_DEPENDENCY records HYDRA_DEP_GHOST, which is not a documented name/);
});

test('CONTROL: the shipped table keeps the RUST_LOG exemption and reports it', () => {
  const res = spawnSync(process.execPath, [CHECKER], { encoding: 'utf8' });
  assert.equal(res.status, 0, res.stderr);
  assert.match(res.stdout, /RUST_LOG is read by a DEPENDENCY/);
});
