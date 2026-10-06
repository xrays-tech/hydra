#!/usr/bin/env node
'use strict';
/**
 * Every environment variable the operator runbook tells you to set must actually
 * be read somewhere in this tree.
 *
 * `dev-docs/ops.md`'s configuration table is the canonical operator reference:
 * during an incident it is what someone greps for. It listed
 *
 *     | `RUST_LOG` / `HYDRA_LOG` | `info` | `tracing` env filter. |
 *
 * and NOTHING has ever read `HYDRA_LOG` (verified 2026-09-29 at runtime: an
 * instance started with only `HYDRA_LOG=debug` logs 0 lines while remaining
 * healthy). That is the same failure class as a UI switch that does nothing: the
 * operator believes they turned something on. The rest of the table (30 other
 * names) *is* wired, so this was a single miss — exactly the kind of thing a
 * mechanical check catches and a human reading the table does not.
 *
 * EVIDENCE is a READ, in CODE AN OPERATOR RUNS (rounds 119 + 124).
 *
 * Round 124: a mention inside a TEST ARTIFACT used to count. Measured 2026-09-30 with a tree whose
 * only occurrence of a documented name was a string in this guard's OWN fixture file
 * (`scripts/check_documented_env.test.cjs`): the guard printed OK, so deleting the real read from the
 * product would not have been noticed for that name. Test files (`*.test.*`, `*.spec.*`,
 * `test_*.py`, `*_test.go`, anything under `tests/`/`test/`) are therefore skipped, and `#[cfg(test)]`
 * items inside Rust files are blanked (`rust_blank.cjs`) — a unit test calling
 * `env_positive_u32("HYDRA_X", 1)` is not the product reading the knob.
 * KNOWN RESIDUAL: string CONTENTS are still readable in JS/TS and Python text (they must be, because
 * `process.env["HYDRA_X"]` and `os.environ["HYDRA_X"]` ARE string literals), so a doc string that
 * spells such a shape counts. Masking them was tried and removed: it reddened the control case by
 * destroying the real evidence.
 *
 * EVIDENCE is a READ, not a mention (round 119). The first version collected every uppercase
 * identifier anywhere in the tree, so a compose `environment:` ASSIGNMENT — a WRITE — proved that
 * the variable was "wired", and so did a `const X_ENV: &str = "HYDRA_X"` nothing uses. Measured
 * 2026-09-30: a table row plus `HYDRA_FOO: "1"` in a compose file passed the guard with no code
 * reading `HYDRA_FOO` at all. Accepted shapes are now: a direct `env::var("X")`/`env!("X")`/
 * `os.environ`/`process.env`/`os.Getenv`/shell `${X}` read; the name passed as a whole string
 * literal ARGUMENT to a helper (most knobs here are read through `env_positive_u32("HYDRA_X", …)`);
 * or a constant that carries the name AND is used as a call argument (one hop). Anything else is
 * a name, not a consumer. Names honoured by a dependency are listed with the reason and printed.
 *
 * Scope: the names in the FIRST column of table rows in `dev-docs/ops.md`, and
 * only those. Prose elsewhere in the file deliberately discusses knobs that were
 * never implemented (`HYDRA_EDGE_TLS`, `HYDRA_FAILOVER_GRACE_MS`,
 * `HYDRA_RATE_LIMIT_FAIL_MODE`) and those must NOT be reported — a config table
 * is a promise, prose about a known limitation is not. Rows whose own text marks
 * the variable as unimplemented are therefore skipped only if they say so
 * explicitly (see NOT_READ_MARKERS) — the recommended fix, and what ops.md does
 * today, is to keep such knobs out of the table entirely.
 *
 * Exit codes: 0 all wired, 1 a documented-but-unread name, 2 the scan could not
 * be performed (missing docs, table smaller than the floor, no code root).
 */

const fs = require('fs');
const { records, audit } = require('./recorded_exceptions.cjs');
const { stripCommentsAndTestItems } = require('./rust_blank.cjs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const DEFAULT_DOCS = path.join(ROOT, 'dev-docs', 'ops.md');
const MIN_ROWS = Number(process.env.DOC_ENV_MIN_ROWS || 20);

// A row may say outright that the variable is not read; that is honest
// documentation, not a promise, so it is skipped.
const NOT_READ_MARKERS = [/not read/i, /未读取/, /未接线/, /不存在该开关/, /从未实现/];

// Names that appear in the document OUTSIDE a table's first column: prose notes, shell examples, or
// another row's description. The rule below only covers FIRST-COLUMN names, so these were invisible to
// it — measured 2026-10-01: 10 such names, every one of them accounted for by hand (audited this
// round). A NEW prose-only name fails until it is given a table row or recorded here with a reason, and
// a record that no longer applies (the name now has a row, or has left the document) fails too: a
// recorded decision that cannot expire is a stale claim.
// The list is overridable so the guard's fixtures can build small documents without the repository's
// records (the same discipline as the floors): an override REPLACES the built-in list, and the
// staleness check applies to whichever list is in force.
const PROSE_ONLY_OK = records(process.env.DOC_ENV_PROSE_OK, [
  ['HYDRA_LOG', 'ops.md says outright that it is **NOT read** (the `RUST_LOG` row documents the removal of the alias)'],
  ['HYDRA_FAIL_MODE', 'a note recording the names that do NOT select the auth fail mode (measured 2026-09-30: each still answers 503)'],
  ['HYDRA_AUTH_FAIL_MODE', 'same note — a rejected spelling, not a knob'],
  ['HYDRA_AUTH_FAILMODE', 'same note — a rejected spelling, not a knob'],
  ['HYDRA_FAILOVER_GRACE_MS', 'ops.md lists it among the knobs that are documented but NOT wired (round 166 measurements)'],
  ['HYDRA_LEADER_LEASE_MS', 'a knob RETIRED by ADR-0001 T4.1: ops.md names it inside the `HYDRA_CONTROL_POLL_MS` row as retired (the lease has no reader, and `RETIRED_CLUSTER_ENV` says so in code)'],
  ['HYDRA_RATE_LIMIT_FAIL_MODE', 'ops.md says it does not exist at all (`grep -rn RATE_LIMIT_FAIL_MODE crates/` is empty)'],
  ['HYDRA_ENCRYPTION_KEY_FILE', 'a real knob, mentioned inside the `HYDRA_ENCRYPTION_KEY` row; read at `crypto.rs:142` (verified this round)'],
  ['HYDRA_CLICKHOUSE_IO_TIMEOUT_MS', 'a real knob, mentioned inside the `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` row; read at `clickhouse.rs:135`'],
  ['HYDRA_ENCRYPTION_KEY_PREVIOUS_VERSION', 'read at `crypto.rs:186`; named in the rotation procedure example'],
  ['HYDRA_BREAKER_QUORUM', 'read at `main.rs:73`; named in the breaker prose and in the not-wired list'],
]);

class ScanError extends Error {
  constructor(message) {
    super(message);
    this.code = 2;
  }
}

function parseArgs(argv) {
  const opts = {
    docs: process.env.DOC_ENV_DOCS || DEFAULT_DOCS,
    codeRoot: process.env.DOC_ENV_ROOT || ROOT,
  };
  for (const arg of argv) {
    if (arg.startsWith('--docs=')) opts.docs = arg.slice('--docs='.length);
    else if (arg.startsWith('--code-root=')) opts.codeRoot = arg.slice('--code-root='.length);
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else throw new ScanError(`unknown argument: ${arg}`);
  }
  return opts;
}

/** Names promised by the FIRST column of any table row in the document. */
function tableNames(markdown) {
  const rows = [];
  for (const line of markdown.split('\n')) {
    if (!line.trimStart().startsWith('|')) continue;
    const cells = line.split('|').slice(1, -1);
    if (cells.length < 2) continue;
    const first = cells[0];
    const names = [...first.matchAll(/`([A-Z][A-Z0-9_]{2,})`/g)].map((m) => m[1]);
    if (names.length === 0) continue;
    const rest = cells.slice(1).join(' | ');
    rows.push({ names, rest, line: line.trim() });
  }
  return rows;
}

const SKIP_DIRS = new Set(['target', 'node_modules', '.git', '.acceptance', 'dist', 'dist-test']);
// Executable / config files only. Prose is DELIBERATELY excluded: a mention in a
// markdown file is not evidence that anything reads the variable — the first
// version of this checker scanned `.md` too, so `dev-docs/ops.md` satisfied its
// own claim (and a falsification run of the pre-fix row passed!). `.md`/`.html`
// are therefore not evidence; `Dockerfile`-style files without an extension are.
const CODE_EXT = new Set(['.rs', '.ts', '.js', '.cjs', '.mjs', '.py', '.go', '.sh', '.yml', '.yaml']);
const CODE_NAMES = new Set(['Dockerfile', 'Makefile', 'Containerfile']);

/**
 * Removes comments (and, for shell/YAML, `#` tails) before evidence collection.
 * A comment is not a consumer: with comments left in, this checker's own header
 * — which quotes the very row it is checking — counted as proof that
 * `HYDRA_LOG` was wired, so a falsification run of the pre-fix row PASSED. That
 * is the second self-satisfaction bug in this file (the first was scanning
 * `.md`); both are recorded in the plan.
 */
function stripComments(text, ext) {
  if (ext === '.py' || ext === '.sh' || ext === '.yml' || ext === '.yaml') {
    return text
      .split('\n')
      .map((l) => l.replace(/(^|\s)#.*$/, '$1'))
      .join('\n');
  }
  return text
    .replace(/\/\*[\s\S]*?\*\//g, ' ')
    .split('\n')
    .map((l) => l.replace(/\/\/.*$/, ''))
    .join('\n');
}

/**
 * READ EVIDENCE, not "the name appears somewhere".
 *
 * The first version of this checker collected every uppercase identifier it saw — so a compose
 * `environment:` ASSIGNMENT counted as proof that the variable is read, and so did a
 * `const X_ENV: &str = "HYDRA_X";` that nothing ever uses. Both are WRITES (or just names). The
 * hole was measured 2026-09-30: adding `| \`HYDRA_FOO\` | \`1\` | … |` to the table plus
 * `HYDRA_FOO: "1"` to a compose file passed the guard while no code reads `HYDRA_FOO` at all —
 * the exact failure the guard exists to catch ("the operator believes they turned something on").
 *
 * A name now counts as READ only in one of these shapes:
 *   1. a direct read:  `env::var("X")`, `env::var_os("X")`, `env!("X")`, `option_env!("X")`,
 *      `os.environ["X"]`, `os.environ.get("X")`, `getenv("X")`, `process.env.X`,
 *      `process.env["X"]`, `os.Getenv("X")`, or a shell `${X}` / `$X`;
 *   2. the name passed as a WHOLE string-literal argument to anything:
 *      `env_positive_u32("HYDRA_TENANT_API_LOCKOUT_SECS", 900)` — the tree reads most knobs through
 *      small helpers that take the variable name as `&str`, so requiring `env::var("X")` inline
 *      would report those as unread. The literal must be followed by `,` or `)` so a log message
 *      that merely begins with the name (`warn!("HYDRA_X is unset")`) is not evidence;
 *   3. a constant that CARRIES the name (`const TLS_LISTEN_ENV: &str = "HYDRA_TLS_LISTEN";`) AND is
 *      used as a call argument somewhere — one hop, the same indirection
 *      `check_documented_metrics.cjs` resolves for metric names. Declaration alone is not a read.
 *
 * Names honoured by a DEPENDENCY rather than by a literal in this tree are listed in
 * `READ_BY_DEPENDENCY` with the reason and printed on every run, so the list cannot grow quietly.
 */
// The map is REPLACEABLE (not mergeable) so a focused fixture can state its own exemption; the default
// is the shipped one, because the repository's own fixtures legitimately rely on the RUST_LOG entry.
const READ_BY_DEPENDENCY = records(process.env.DOC_ENV_READ_BY_DEPENDENCY, [
  ['RUST_LOG', "read by the tracing subscriber (main.rs uses EnvFilter::from_default_env()); the name never appears as a literal in this tree"],
]);

/** A direct read of `name` on one line, or null. */
function directRead(line, ext, name) {
  if (ext === '.rs') {
    if (line.includes(`env::var("${name}"`) || line.includes(`env::var_os("${name}"`)) return 'env::var';
    if (line.includes(`env!("${name}"`) || line.includes(`option_env!("${name}"`)) return 'env!/option_env!';
  }
  if (ext === '.py') {
    if (line.includes(`environ["${name}"]`) || line.includes(`environ.get("${name}"`)) return 'os.environ';
    if (line.includes(`getenv("${name}"`)) return 'os.getenv';
  }
  if (ext === '.js' || ext === '.cjs' || ext === '.mjs' || ext === '.ts') {
    if (line.includes(`process.env.${name}`)) return 'process.env.X';
    if (line.includes(`process.env["${name}"]`) || line.includes(`process.env['${name}']`)) return 'process.env["X"]';
  }
  if (ext === '.go' && line.includes(`Getenv("${name}"`)) return 'os.Getenv';
  if (ext === '.sh' && (line.includes('${' + name + '}') || new RegExp(`\\$${name}\\b`).test(line))) {
    return 'shell ${X}';
  }
  return null;
}

/** The name handed to a helper as a whole literal argument: `f("X", …)` / `f("X")`. */
function helperArgument(line, name) {
  return new RegExp(`[(,]\\s*"${name}"\\s*[,)]`).test(line) ? 'passed as a literal argument' : null;
}

/** `const IDENT: &str = "X";` / `static IDENT: &str = "X";` on one line, or null. */
function nameCarrier(line, name) {
  const m = line.match(new RegExp(`(?:const|static)\\s+([A-Z][A-Z0-9_]*)\\s*:\\s*&str\\s*=\\s*"${name}"`));
  return m ? m[1] : null;
}

/** Local functions that read the environment through one of their PARAMETERS. */
function envReaderHelpers(sources) {
  const helpers = new Set();
  for (const f of sources) {
    for (const m of f.text.matchAll(/fn\s+([a-z_][a-z0-9_]*)\s*(?:<[^>]*>)?\s*\(([^)]*)\)/g)) {
      const params = m[2]
        .split(',')
        .map((x) => x.trim().split(':')[0].trim())
        .filter((x) => /^[a-z_][a-z0-9_]*$/.test(x));
      if (params.length === 0) continue;
      const body = f.text.slice(m.index, m.index + 2500);
      if (params.some((param) => new RegExp(`env::var(?:_os)?\\(\\s*${param}\\b`).test(body))) {
        helpers.add(m[1]);
      }
    }
  }
  return helpers;
}

/**
 * Evidence that a CARRIER constant is actually used to read the environment (one hop).
 *
 * The first version accepted any use as a call argument — `validate(TLS_LISTEN_ENV, v)` (a
 * validation call) or a `format!` argument would do (measured 2026-09-30: two of the three real
 * uses of the listeners' constants are a validation call and an error-message argument, neither of
 * which reads anything). The rule is "this name is read", so the hop now has to END in a read:
 *   * `env::var(IDENT)` / `env::var_os(IDENT)` — the constant names the variable being read;
 *   * `helper(IDENT, …)` where `helper` is a local function that reads the environment through one
 *     of its parameters (`env_positive_u32("HYDRA_X", 900)`-style helpers).
 */
function carrierRead(line, ident, envHelpers) {
  if (new RegExp(`env::var(?:_os)?\\(\\s*${ident}\\b`).test(line)) {
    return 'carrier const read by env::var';
  }
  if (!new RegExp(`[(,]\\s*${ident}\\s*[,)]`).test(line)) return null;
  for (const helper of envHelpers) {
    if (new RegExp(`\\b${helper}\\s*\\(`).test(line)) {
      return `carrier const passed to ${helper}(), which reads the environment`;
    }
  }
  return null;
}

/** Test artifacts: the shapes `check_ci_wiring.cjs` discovers, plus anything under a test dir. */
function isTestArtifact(rel) {
  const base = path.basename(rel);
  if (/\.test\.(cjs|js|mjs|ts)$/.test(base) || /\.spec\.(cjs|js|mjs|ts)$/.test(base)) return true;
  if (/^test_.*\.py$/.test(base) || /_test\.go$/.test(base)) return true;
  return rel.split(path.sep).some((seg) => seg === 'tests' || seg === 'test');
}

function collectReadSites(root) {
  const sites = new Map(); // name -> [{ where, shape }]
  const record = (name, where, shape) => {
    if (!sites.has(name)) sites.set(name, []);
    sites.get(name).push({ where, shape });
  };
  const files = [];
  const walk = (dir, depth) => {
    if (depth > 8) return;
    let entries;
    try {
      entries = fs.readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const e of entries) {
      if (e.isDirectory()) {
        if (SKIP_DIRS.has(e.name)) continue;
        walk(path.join(dir, e.name), depth + 1);
        continue;
      }
      const ext = path.extname(e.name);
      if (!CODE_EXT.has(ext) && !CODE_NAMES.has(e.name)) continue;
      const full = path.join(dir, e.name);
      const rel = path.relative(ROOT, full);
      // TEST ARTIFACTS ARE NOT CONSUMERS. Measured 2026-09-30: a tree whose only mention of a
      // documented name was the CHECKER'S OWN FIXTURE — a string in
      // `scripts/check_documented_env.test.cjs` — was reported as "wired" (exit 0), so deleting the
      // real read from the product would not have been noticed for that name. The rule is about the
      // code an operator RUNS; a fixture that copies the shape of a read is evidence of nothing.
      if (isTestArtifact(rel)) continue;
      files.push({ file: full, ext, rel });
    }
  };
  walk(root, 0);

  const carriers = []; // { name, ident, rel }
  const texts = [];
  for (const f of files) {
    let text;
    try {
      const raw = fs.readFileSync(f.file, 'utf8');
      // Per language, and string-aware: Rust gets `#[cfg(test)]` items blanked as well (a unit test
      // that calls `env_positive_u32("HYDRA_X", 1)` is not the product reading the knob), JS/TS gets
      // its string and comment contents blanked, and the hash-comment formats keep their own rule.
      if (f.ext === '.rs') text = stripCommentsAndTestItems(raw);
      // NOT `maskJsLiterals` here: masking blanked the CONTENTS of `process.env["X"]`, i.e. it
      // destroyed the very evidence this guard reads (measured while writing it — the control case
      // in the test file went red). JS/TS keeps the comment-stripping rule, so a string that
      // happens to spell `process.env.HYDRA_X` inside a doc line still counts; that residual is
      // recorded in the header rather than papered over.
      else text = stripComments(raw, f.ext);
    } catch {
      continue;
    }
    texts.push({ ...f, text });
    for (const [i, line] of text.split('\n').entries()) {
      for (const m of line.matchAll(/\b(HYDRA_[A-Z0-9_]+)\b/g)) {
        const name = m[1];
        const shape = directRead(line, f.ext, name) || helperArgument(line, name);
        if (shape) record(name, `${f.rel}:${i + 1}`, shape);
        const ident = nameCarrier(line, name);
        if (ident) carriers.push({ name, ident, rel: f.rel });
      }
    }
  }
  // One hop: the constant is a name carrier only if something READS the environment through it.
  const envHelpers = envReaderHelpers(texts);
  for (const c of carriers) {
    for (const f of texts) {
      const line = f.text.split('\n').find((l) => carrierRead(l, c.ident, envHelpers));
      if (line) {
        record(
          c.name,
          `${f.rel} (via const ${c.ident} declared in ${c.rel})`,
          carrierRead(line, c.ident, envHelpers),
        );
        break;
      }
    }
  }
  return sites;
}

function main(argv) {
  const opts = parseArgs(argv);
  if (opts.help) {
    console.log('usage: node scripts/check_documented_env.cjs [--docs=FILE] [--code-root=DIR]');
    return 0;
  }
  if (!fs.existsSync(opts.docs)) throw new ScanError(`docs file not found: ${opts.docs}`);
  if (!fs.existsSync(opts.codeRoot)) throw new ScanError(`code root not found: ${opts.codeRoot}`);

  const markdown = fs.readFileSync(opts.docs, 'utf8');
  // Two separate guards, because they measure different things: the row floor
  // catches a table that was reworded/truncated away (a parse problem), while the
  // `checked` floor below catches "we parsed rows but verified almost nothing"
  // (a coverage problem). Rows skipped as honest "not read" notes still count
  // towards the parse floor.
  const parsed = tableNames(markdown);
  if (parsed.length < MIN_ROWS) {
    throw new ScanError(`only ${parsed.length} table row(s) with a backticked name found in ${path.relative(ROOT, opts.docs)} (< floor ${MIN_ROWS}); refusing to "pass" a table that was reworded away`);
  }
  const rows = parsed.filter((r) => !NOT_READ_MARKERS.some((re) => re.test(r.rest)));

  const sites = collectReadSites(opts.codeRoot);
  const missing = [];
  let checked = 0;
  let viaDependency = 0;
  // The FLOOR counts DISTINCT names, not occurrences (round 160): one name repeated across rows
  // satisfied `MIN_ROWS` while almost nothing was verified, and the OK line called the occurrence
  // count "N documented env name(s)". Measured on the real table: **44 occurrences over 40 distinct
  // names** (four names appear twice) — small today, but the two numbers are free to drift apart and
  // the floor must follow the one that means "how much was verified".
  const checkedNames = new Set();
  for (const row of rows) {
    for (const name of row.names) {
      // Only configuration names are promised here; a table may also name other
      // constants in its first cell, so skip anything that is not env-shaped.
      if (!/^(HYDRA|RUST_LOG|LLM|OPENAI|ANTHROPIC)/.test(name)) continue;
      checked += 1;
      checkedNames.add(name);
      if (sites.has(name)) continue;
      if (READ_BY_DEPENDENCY.has(name)) {
        viaDependency += 1;
        continue;
      }
      missing.push({ name, row: row.line });
    }
  }
  if (checkedNames.size < MIN_ROWS) {
    throw new ScanError(
      `only ${checkedNames.size} DISTINCT env name(s) checked (< floor ${MIN_ROWS}) across ` +
        `${checked} occurrence(s)`,
    );
  }

  // Prose-only names (see PROSE_ONLY_OK): the table rule above cannot see them, so they are recorded
  // instead of ignored — and a new one is a finding, not a silent gap.
  const proseOnly = new Map();
  const firstColumn = new Set(parsed.flatMap((r) => r.names));
  for (const line of markdown.split('\n')) {
    for (const m of line.matchAll(/\b(HYDRA_[A-Z0-9_]+)\b/g)) {
      const name = m[1];
      if (firstColumn.has(name)) continue;
      if (!proseOnly.has(name)) proseOnly.set(name, line.trim().slice(0, 120));
    }
  }
  const { unrecorded, stale: staleRecords } = audit({
    records: PROSE_ONLY_OK,
    needed: [...proseOnly.keys()],
    applies: (n) => proseOnly.has(n),
  });
  if (unrecorded.length > 0) {
    console.error(
      `[doc-env] FAIL: ${unrecorded.length} env name(s) appear in ${path.relative(ROOT, opts.docs)} ` +
        `OUTSIDE the table's first column, where this rule cannot check them:`,
    );
    for (const n of unrecorded) console.error(`[doc-env]   ${n}\n[doc-env]     ${proseOnly.get(n)}`);
    console.error(
      '[doc-env] Either give the name a table row (so its READ site is verified) or record it in ' +
        'PROSE_ONLY_OK with the reason it needs no read site (a name documented as NOT wired belongs there)',
    );
    return 1;
  }
  if (staleRecords.length > 0) {
    console.error(
      `[doc-env] FAIL: PROSE_ONLY_OK records ${staleRecords.length} name(s) that no longer apply ` +
        `(they now have a table row, or they have left the document): ${staleRecords.join(', ')}`,
    );
    return 1;
  }

  // READ_BY_DEPENDENCY: an exemption that says "no literal read is needed because a DEPENDENCY reads
  // it". The dangerous direction is a DEAD exemption — the tree grew a literal read site, so the entry
  // no longer exempts anything and it silently hides that read (round 182). That direction is a finding
  // EVERYWHERE, fixtures included. The other direction (the name left the documented table) can only be
  // judged against the shipped table, so it is a NOTE here — a focused fixture document has three rows
  // and would otherwise report every entry as "no longer documented".
  for (const [name, why] of READ_BY_DEPENDENCY) {
    if (sites.has(name)) {
      console.error(
        `[doc-env] FAIL: ${name} is exempted as dependency-read, but this tree HAS a literal READ site ` +
          `for it — the exemption is dead and hides that read; delete the entry`,
      );
      console.error(`[doc-env]   recorded reason: ${why}`);
      return 1;
    }
    if (!firstColumn.has(name)) {
      console.log(
        `[doc-env]   note: READ_BY_DEPENDENCY records ${name}, which is not a documented name in ` +
          `${path.relative(ROOT, opts.docs)} — the entry exempts nothing here (checked as a finding only ` +
          `against the shipped table)`,
      );
    }
  }

  if (missing.length === 0) {
    if (process.env.DOC_ENV_DUMP) {
      for (const [name, list] of [...sites].sort()) console.log(`DUMP ${name} :: ${list.map((x) => `${x.where} [${x.shape}]`).join(' | ')}`);
    }
    // Round 194: an adversarial review read this sentence as claiming a read site for `RUST_LOG`
    // too (the note printed underneath says it is read by a DEPENDENCY). MEASURED, and the review
    // was wrong: with the exemption map EMPTIED (`DOC_ENV_READ_BY_DEPENDENCY='{}'`) the guard still
    // passes, which proves `RUST_LOG` is not one of the names counted here — its row never reaches
    // `rows`, and the note is a standalone explanation of a recorded decision, not part of the 40.
    // The sentence is still made ARITHMETIC rather than absolute, because `viaDependency` counts the
    // case where a table row IS exempted: that branch is latent today (no table name is exempt) and
    // exists so this line cannot become false the day one is.
    const readSiteClaim = viaDependency === 0
      ? `all have a READ site in code (a literal argument, an env::var/env! read, or a used carrier `
        + `constant); a compose assignment or an unused constant is not a read`
      : `all but ${viaDependency} have a READ site in code (a literal argument, an env::var/env! read, `
        + `or a used carrier constant); a compose assignment or an unused constant is not a read — the `
        + `${viaDependency} exempted name(s) are read by a DEPENDENCY `
        + `(${[...READ_BY_DEPENDENCY.keys()].join(', ')}), see the note(s) below`;
    console.log(`[doc-env] OK: ${checkedNames.size} distinct documented env name(s) ` + `(${proseOnly.size} more appear in prose and are recorded as needing no read site) ` +
      `(${checked} occurrence(s)) in ${path.relative(ROOT, opts.docs)} `
      + readSiteClaim);
    for (const [name, why] of READ_BY_DEPENDENCY) {
      console.log(`[doc-env]   note: ${name} is read by a DEPENDENCY, not by a literal here — ${why}`);
    }
    return 0;
  }
  const where = path.relative(ROOT, opts.codeRoot) || path.basename(opts.codeRoot) || opts.codeRoot;
  console.error(`[doc-env] FAIL: ${missing.length} documented env name(s) have no READ site under ${where}`
    + ` (a compose \`environment:\` assignment, an unused constant or a comment is NOT a read):`);
  for (const m of missing) console.error(`[doc-env]   ${m.name}\n[doc-env]     ${m.row.slice(0, 140)}`);
  console.error('[doc-env] a config table is a promise: either wire the variable, or move it out of the table');
  return 1;
}

try {
  process.exit(main(process.argv.slice(2)));
} catch (err) {
  if (err instanceof ScanError) {
    console.error(`[doc-env] CANNOT SCAN: ${err.message}`);
    process.exit(err.code);
  }
  console.error(`[doc-env] ERROR: ${err.message}`);
  process.exit(2);
}
