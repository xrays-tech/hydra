#!/usr/bin/env node
'use strict';
/**
 * `CLUSTER_ONLY_ENV` (in `crates/hydra-server/src/cluster/mod.rs`) is the single owner of "what
 * counts as cluster wiring", and `NodeRole::from_env` prints the set ones in an ERROR when a node
 * configured cluster wiring but is not a cluster node. That ERROR is the ONLY signal an operator
 * gets, so a name missing from the table is a setting that is dropped in silence.
 *
 * Measured 2026-10-01 (round 193): the list was three literals inside
 * `ignored_cluster_wiring()` — `HYDRA_REDIS_URL`, `HYDRA_CLUSTER_TOKEN`, `HYDRA_CONTROL_URL` —
 * while `HYDRA_NODE_ID`, `HYDRA_CONTROL_POLL_MS`, `HYDRA_REDIS_MODE`, `HYDRA_PUBLIC_URL`,
 * `HYDRA_LEADER_LEASE_MS`, `HYDRA_REGISTRY_STALE_GRACE_SECS` and `HYDRA_FORWARD_TIMEOUT_SECS`
 * were all configured, all ignored, and none of them mentioned. A hand-written list ate the other
 * seven. So the table is checked in BOTH directions:
 *
 *   R1 COMPLETE: every literal `std::env::var("NAME")` read under `crates/hydra-server/src/cluster/`
 *      must be in the table. Every module there exists for cluster mode, so a read in it is
 *      cluster-only by construction — this is the decidable half, and it is the half that was
 *      wrong (all three of the reads living under `src/cluster/` were missing from the list).
 *      ANY name counts, not just `HYDRA_*` (round 194): the first version matched only the prefix,
 *      so `HOSTNAME` — read under `src/cluster/` as the node-id fallback tier — was invisible to
 *      the rule. A prefix filter, inside the guard written to catch filters that eat objects.
 *   R2 EARNED: every name in the table must have at least one literal read site somewhere under
 *      `crates/hydra-server/src/`. A name with no reader is a claim about a setting that does not
 *      exist — the same "must earn its place" rule the metrics and env guards use.
 *   R3 NO INVISIBLE READS: `env::var(<expression>)` cannot be judged by R1 at all, so every file
 *      under `src/cluster/` doing that must be recorded in `NON_LITERAL_READ_OK` with the reason
 *      the indirection cannot hide a cluster-only name. Today that is the `CLUSTER_ONLY_ENV` loop
 *      in `NodeRole::from_env` itself.
 *
 * Both record maps use the shared algebra (`recorded_exceptions.cjs`), so an unrecorded item AND a
 * record that no longer applies are both findings — which is what makes the two filters above
 * impossible to reintroduce quietly: restoring the `HYDRA_`-only prefix turns the `HOSTNAME` record
 * stale on this very tree (measured, round 194).
 *
 * The reads that only a cluster ROLE reaches but that live outside `src/cluster/`
 * (`HYDRA_PUBLIC_URL`, `HYDRA_LEADER_LEASE_MS`, `HYDRA_REGISTRY_STALE_GRACE_SECS` in `main.rs`,
 * `HYDRA_REDIS_MODE` in `redis/mod.rs`) cannot be told apart from single-node reads by a regex
 * over file paths — that is exactly why they were missed — so R2 covers them and the drill
 * (`integration/test_startup_knobs.py`, K10) asserts the wire behaviour: configure all ten and
 * the ERROR must name all ten.
 *
 * WHAT COUNTS AS EVIDENCE (round 198): both readers match on `stripCommentsAndTestItems()` from
 * `rust_blank.cjs`, so a mention in a comment — leading OR trailing — is not a read, and a
 * `#[cfg(test)]` block is not the product reading the knob. The earlier hand-rolled line-start
 * heuristic could be satisfied by a doc comment (measured, round 197) and required the paragraph that
 * used to live here, admitting that a `#[cfg(test)]` read would demand a record; sharing the single
 * owner removed both. Measured 2026-10-01 on the real tree: 9 files, 7 literal reads, 1 non-literal
 * site (the `CLUSTER_ONLY_ENV` loop), 2 recorded non-cluster names.
 *
 * Exit codes: 0 clean, 1 a violation, 2 CANNOT VERIFY (the table is unparseable, or a scan that
 * should have matched something matched nothing — an empty scan is not a pass).
 */
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');
const { records, audit } = require('./recorded_exceptions.cjs');
// Single owner of "strip Rust comments and `#[cfg(test)]` items, line numbers preserved" — the same
// module `check_documented_defaults.cjs` uses. Round 198: this guard hand-rolled a line-start
// heuristic instead of sharing it, which (a) made it the ONE guard a comment could satisfy (round 197
// measured exactly that: a name mentioned only in a `///` line counted as a reader), (b) left trailing
// comments counting, and (c) forced it to document a `#[cfg(test)]` limit this stripper removes.
const { stripCommentsAndTestItems } = require('./rust_blank.cjs');

const ROOT = path.resolve(__dirname, '..');
const OWNER = 'crates/hydra-server/src/cluster/mod.rs';
const CLUSTER_DIR = 'crates/hydra-server/src/cluster';
const SRC_DIR = 'crates/hydra-server/src';

/**
 * Names read under `src/cluster/` that are deliberately NOT cluster-only, with the reason. Replace,
 * never merge (`recorded_exceptions.cjs`), so a fixture tree never inherits this record.
 *
 * `HYDRA_ROLE` is the one read in `src/cluster/mod.rs` that must not be in the table: it is the
 * selector itself — the variable that decides whether any of the rest of this code runs — and it is
 * read in every mode. `HOSTNAME` is the node-id fallback tier (`node_id_from`): the OS always sets
 * it, so it is not something an operator configures FOR the cluster and it cannot be "dropped
 * wiring". The staleness direction is what keeps a record honest: if the read moves out of
 * `src/cluster/`, `applies` turns false and the guard says the record expired.
 *
 * Round 194: this rule used to match only `"HYDRA_*"` literals, which silently excluded `HOSTNAME`
 * — a read under `src/cluster/` that the completeness rule never looked at. A filter that eats
 * objects, in the guard written to catch exactly that.
 */
const NOT_CLUSTER_ONLY = records(process.env.CLUSTER_ENV_NOT_CLUSTER_ONLY, [
  // `HYDRA_ROLE` used to be recorded here as "the selector itself, read in every mode".
  // ADR-0001 removed that selector: the member list (`HYDRA_CLUSTER_PEERS`) is the decision
  // now, and nothing under `src/cluster/` reads `HYDRA_ROLE` any more, so the record expired
  // — the staleness direction of this algebra is what caught it.
  ['HOSTNAME', 'the OS hostname (node-id fallback tier); always set, nothing an operator configures'],
]);

/**
 * The NON-literal env reads under `src/cluster/`, keyed by SITE, with the reason. A name the guard
 * cannot see is a name the completeness rule cannot judge, so each such site must be a recorded
 * decision — and the record expires if that site disappears.
 *
 * The key is the site, NOT the file (round 194, found by an adversarial review of the guard itself):
 * the first version recorded the FILE, so the reason written for the `CLUSTER_ONLY_ENV` loop —
 * "it reads exactly the table, by construction" — silently vouched for every future
 * `std::env::var(<expression>)` in that same file, including one reading a name that is not in the
 * table at all (measured with a fixture: `env::var(SECRET_KNOB)` with `HYDRA_SECRET_KNOB` absent
 * from the table passed with exit 0 and an OK line claiming every read was accounted for).
 *
 * AND THE KEY IS THE SITE'S OWN TEXT, not its ordinal (round 208, the second adversarial review of
 * this guard). `#<ordinal>` was "the site" only positionally: `applies()` asked whether the file still
 * had AT LEAST that many non-literal sites, never whether the one it now pointed at was the one the
 * reason was written about. Measured with a fixture before this change: a single recorded site whose
 * text was REPLACED by `std::env::var(format!("HYDRA_{}", "CLUSTER_PEERS"))` — an indirection that can
 * hide a cluster-only name, which is the whole reason the rule exists — still reported
 * "1 non-literal read(s) (all recorded) … OK" with exit 0.
 *
 * So the identity is a fingerprint of the site's comment-free text: `<file>@<8 hex>`, with `#2`, `#3`
 * … appended when a file contains the SAME text twice (identical lines are indistinguishable by
 * content, and one record must still cover exactly one site). This keeps the property the ordinal
 * scheme was chosen for — moving a site down its file does NOT churn the record, because its text did
 * not change — while closing the hole: editing that line changes its key, so the old record expires
 * and the new site has to be judged by a human. A stale record prints the file's current keys, so
 * updating one is mechanical.
 */
const NON_LITERAL_READ_OK = records(process.env.CLUSTER_ENV_NON_LITERAL_OK, [
  [
    'crates/hydra-server/src/cluster/mod.rs@c78d022d',
    'the table loop in NodeRole::from_env: it iterates CLUSTER_ONLY_ENV chained with ' +
      'RETIRED_CLUSTER_ENV — both declared in that file, in that scope — and reads each element by ' +
      'value, so the indirection cannot name anything the table does not already contain',
  ],
  [
    'crates/hydra-server/src/cluster/arachne_node.rs@f00be949',
    'cluster_enabled(): the single read behind the cluster/single-node decision (ADR-0001). It reads PEERS_ENV, a constant declared in that file and nowhere else, so the indirection cannot hide a cluster-only name — that name is the one thing it exists to read. DEFERRED (tracked by plan T1.3 / T4.2): replacing the HYDRA_ROLE-based decision with this one, adding HYDRA_CLUSTER_PEERS and HYDRA_ARACHNE_LISTEN to the table, and removing the seven retired names all happen in the wiring step; until then the two new variables are deliberately absent from the table rather than listed while nothing reads them through it.',
  ],
]);

/**
 * How many lines ABOVE the `env::var(…)` line belong to the site's identity.
 *
 * One line is not enough, and the measurement that proved it is in this file's history: a fixture whose
 * site read `let _ = std::env::var(name);` was recorded, and REPLACING the line above it —
 * `let name = "HYDRA_ANYTHING";` becoming `let name = format!("HYDRA_{}", "CLUSTER_PEERS");` — kept the
 * record green, because the `env::var` line itself was byte-identical. The judgement a record makes is
 * about the EXPRESSION passed in, and in Rust that expression is usually built on the lines just above.
 *
 * The limit, stated rather than implied: a change MORE than {@link CONTEXT_LINES} lines above the site
 * does not change the key (e.g. rebinding the same identifier further up). This is a fingerprint of the
 * site and its immediate context, not a dataflow analysis; it closes the measured hole (the line that
 * builds the argument) without pretending to be one.
 */
const CONTEXT_LINES = 3;

/** The site's own text: the `env::var` line plus up to {@link CONTEXT_LINES} non-blank lines above it. */
function siteText(lines, index) {
  const parts = [];
  for (let k = Math.max(0, index - CONTEXT_LINES); k <= index; k += 1) {
    const line = lines[k].trim();
    if (line) parts.push(line);
  }
  return parts.join('\n');
}

/** The identity of one non-literal site: its FILE, plus a fingerprint of {@link siteText}.
 *
 * `duplicateOrdinal` separates sites whose context is identical (a file may read the environment twice
 * through the same shape): identical text is indistinguishable by content, and one record must still
 * cover exactly one site, so the second and later copies get `#2`, `#3`, … */
function siteKey(file, text, duplicateOrdinal = 1) {
  const digest = crypto.createHash('sha1').update(text).digest('hex').slice(0, 8);
  return `${file}@${digest}${duplicateOrdinal > 1 ? `#${duplicateOrdinal}` : ''}`;
}

/** The file a site key belongs to, for messages (`<file>@<8 hex>[#n]` → `<file>`). */
function siteFileOf(key) {
  const m = String(key).match(/^(.*)@[0-9a-f]{8}(?:#\d+)?$/);
  return m ? m[1] : null;
}

class ScanError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function parseArgs(argv) {
  const opts = {
    root: process.env.CLUSTER_ENV_ROOT ? path.resolve(process.env.CLUSTER_ENV_ROOT) : ROOT,
    quiet: false,
  };
  for (const arg of argv) {
    if (arg === '--quiet') opts.quiet = true;
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else throw new ScanError(2, `unknown argument: ${arg}`);
  }
  return opts;
}

function usage() {
  console.log(`usage: node scripts/check_cluster_env.cjs [--quiet]
Checks that CLUSTER_ONLY_ENV is complete (every env read under src/cluster/ is listed) and that
every listed name is read somewhere. CLUSTER_ENV_ROOT overrides the repository root (tests).`);
}

/** `.rs` files under `dir` (recursive), sorted so the output is stable. */
function rustFiles(dir) {
  if (!fs.existsSync(dir)) throw new ScanError(2, `${path.relative(ROOT, dir)} does not exist`);
  const out = [];
  const walk = (d) => {
    for (const entry of fs.readdirSync(d, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
      const p = path.join(d, entry.name);
      if (entry.isDirectory()) walk(p);
      else if (entry.name.endsWith('.rs')) out.push(p);
    }
  };
  walk(dir);
  return out;
}

/** The names inside the `CLUSTER_ONLY_ENV: [&str; N] = [ ... ];` literal. */
function tableNames(source, label) {
  const decl = source.match(/const\s+CLUSTER_ONLY_ENV\s*:\s*\[&str;\s*\d+\]\s*=\s*\[/);
  if (!decl) {
    throw new ScanError(2, `${label}: no \`const CLUSTER_ONLY_ENV: [&str; N] = [\` declaration — re-verify, then update this checker`);
  }
  const start = decl.index + decl[0].length;
  const end = source.indexOf('];', start);
  if (end === -1) throw new ScanError(2, `${label}: the CLUSTER_ONLY_ENV literal is not terminated`);
  return [...source.slice(start, end).matchAll(/"([A-Z0-9_]+)"/g)].map((m) => m[1]);
}

/**
 * Every env read in `files`, split into the two shapes the guard can tell apart:
 *   * `literals`   — `env::var("NAME")` / `env::var_os("NAME")` → name → `file:line` sites. ANY name,
 *                    not just `HYDRA_*`: `HOSTNAME` is read under `src/cluster/` too, and a pattern
 *                    hardcoded to one prefix is a filter that eats objects (round 194).
 *   * `nonLiteral` — `env::var(<expression>)` → `file` → sites, which the completeness rule cannot
 *                    judge at all, so each file must carry a recorded reason.
 */
function envReads(files, root) {
  const literals = new Map();
  const nonLiteral = new Map();
  for (const file of files) {
    const rel = path.relative(root, file);
    // Match on the COMMENT-FREE, TEST-FREE copy: a comment (leading OR trailing) is never
    // evidence of execution, and a `#[cfg(test)]` read is not the product reading the knob.
    // Split once: a site's identity includes the lines ABOVE it (`siteText`), so the loop needs the
    // whole comment-free, test-free file, not one line at a time.
    const lines = stripCommentsAndTestItems(fs.readFileSync(file, 'utf8')).split('\n');
    for (const [i, line] of lines.entries()) {
      // `env::vars()` / `env::vars_os()` walk the WHOLE environment, so the guard can no more name
      // what they read than it can for `env::var(<expression>)` — round 195: they matched nothing at
      // all here, i.e. an unrecorded whole-environment read was invisible to every rule in this file.
      for (const m of line.matchAll(/env::vars(?:_os)?\(\s*\)|env::var(?:_os)?\(\s*/g)) {
        const at = `${rel}:${i + 1}`;
        const rest = line.slice(m.index + m[0].length);
        const lit = rest.match(/^"([A-Za-z0-9_]+)"/);
        if (lit) {
          if (!literals.has(lit[1])) literals.set(lit[1], []);
          literals.get(lit[1]).push(at);
        } else {
          if (!nonLiteral.has(rel)) nonLiteral.set(rel, []);
          // The site's OWN text (plus its immediate context) travels with it: the record's identity
          // is a fingerprint of that (`siteText`), not of its position in the file.
          nonLiteral.get(rel).push({ at, text: siteText(lines, i) });
        }
      }
    }
  }
  return { literals, nonLiteral };
}

/** The `HYDRA_*` names read somewhere under `SRC_DIR` (the "must earn its place" direction). */
function hydraLiteralReads(files, root) {
  const sites = new Map();
  for (const file of files) {
    const rel = path.relative(root, file);
    // Match on the COMMENT-FREE, TEST-FREE copy: a comment (leading OR trailing) is never
    // evidence of execution, and a `#[cfg(test)]` read is not the product reading the knob.
    for (const [i, line] of stripCommentsAndTestItems(fs.readFileSync(file, 'utf8')).split('\n').entries()) {
      for (const m of line.matchAll(/env::var(?:_os)?\(\s*"(HYDRA_[A-Z0-9_]+)"/g)) {
        if (!sites.has(m[1])) sites.set(m[1], []);
        sites.get(m[1]).push(`${rel}:${i + 1}`);
      }
    }
  }
  return sites;
}

function main(argv) {
  const opts = parseArgs(argv);
  if (opts.help) {
    usage();
    return 0;
  }

  const ownerPath = path.join(opts.root, OWNER);
  if (!fs.existsSync(ownerPath)) {
    throw new ScanError(2, `${path.relative(ROOT, ownerPath)} is missing — this guard owns the table in it`);
  }
  const table = tableNames(fs.readFileSync(ownerPath, 'utf8'), path.relative(ROOT, ownerPath));
  if (table.length === 0) {
    throw new ScanError(2, 'CLUSTER_ONLY_ENV parsed as empty — refusing to report a clean tree from it');
  }

  const clusterFiles = rustFiles(path.join(opts.root, CLUSTER_DIR));
  const { literals: clusterReads, nonLiteral: nonLiteralFiles } = envReads(clusterFiles, opts.root);
  // The completeness rule needs at least one read to be about something: if `src/cluster/` reads no
  // literal env name at all, the rule cannot fail and this scan is not evidence.
  const literalCount = [...clusterReads.values()].reduce((n, v) => n + v.length, 0);
  if (literalCount === 0) {
    throw new ScanError(2, `${CLUSTER_DIR} has no literal \`env::var("NAME")\` read — a rule with no subject is not a pass`);
  }

  const crateReads = hydraLiteralReads(rustFiles(path.join(opts.root, SRC_DIR)), opts.root);

  const problems = [];
  // R1 COMPLETE, with the "read here but not cluster-only" names recorded instead of guessed.
  const unlisted = [...clusterReads.keys()].filter((name) => !table.includes(name));
  const { unrecorded, stale } = audit({
    records: NOT_CLUSTER_ONLY,
    needed: unlisted,
    applies: (name) => unlisted.includes(name),
  });
  for (const name of unrecorded) {
    problems.push(
      `${name} is read in ${clusterReads.get(name)[0]} (cluster-only code) but is NOT in ` +
        `CLUSTER_ONLY_ENV — a node that falls back to single-node mode would drop it in silence; ` +
        `list it, or record it in NOT_CLUSTER_ONLY with the reason it is read in every mode`,
    );
  }
  for (const name of stale) {
    problems.push(
      `NOT_CLUSTER_ONLY records ${name}, but that no longer applies (nothing under ${CLUSTER_DIR}/ ` +
        `reads it any more, or it is in the table now): a recorded decision that cannot expire is a stale claim`,
    );
  }
  // R3 NO INVISIBLE READS: a read the guard cannot name is a read the completeness rule cannot judge,
  // so every such SITE carries a recorded reason (and the record expires when the site goes away).
  const nonLiteralKeys = [];
  /** key → `file:line` (for messages). */
  const siteAt = new Map();
  /** file → its CURRENT keys with their locations, so a stale record prints what to write instead. */
  const keysByFile = new Map();
  for (const [file, sites] of nonLiteralFiles) {
    const seen = new Map();
    const here = [];
    for (const site of sites) {
      const n = (seen.get(site.text) || 0) + 1;
      seen.set(site.text, n);
      const key = siteKey(file, site.text, n);
      siteAt.set(key, site.at);
      here.push(`${key} (${site.at})`);
      nonLiteralKeys.push(key);
    }
    keysByFile.set(file, here);
  }
  const nonLiteralAudit = audit({
    records: NON_LITERAL_READ_OK,
    needed: nonLiteralKeys,
    // IDENTITY, not position (round 208): the record applies only while a site with EXACTLY that text
    // is present. The previous predicate asked whether the file still had at least that many sites,
    // which is satisfied by ANY site — measured: replacing the recorded line's text kept it green.
    applies: (key) => siteAt.has(key),
  });
  for (const key of nonLiteralAudit.unrecorded) {
    problems.push(
      `${siteAt.get(key)} reads the environment through a NON-literal argument — the completeness ` +
        `rule cannot see which name that is, so this SITE must be recorded in NON_LITERAL_READ_OK ` +
        `with the reason the indirection cannot hide a cluster-only name. Its key is "${key}" ` +
        `(a fingerprint of that line: change the line and the record expires, which is the point)`,
    );
  }
  for (const key of nonLiteralAudit.stale) {
    const file = siteFileOf(key);
    const now = file ? keysByFile.get(file) || [] : [];
    problems.push(
      `NON_LITERAL_READ_OK records ${key}, but no site in that file reads the environment through ` +
        `exactly that text any more (the line CHANGED, or the site is gone) — a recorded decision ` +
        `that cannot expire is a stale claim. Current non-literal site(s) in that file: ` +
        `${now.length ? now.join(', ') : '(none)'}`,
    );
  }
  for (const name of table) {
    if (!crateReads.has(name)) {
      problems.push(
        `${name} is listed in CLUSTER_ONLY_ENV but nothing in ${SRC_DIR} reads it — a name that ` +
          `no reader earns is a claim about a setting that does not exist`,
      );
    }
  }
  for (const [i, name] of table.entries()) {
    if (table.indexOf(name) !== i) problems.push(`${name} is listed twice in CLUSTER_ONLY_ENV`);
  }

  const unrecordedSites = nonLiteralAudit.unrecorded.length;
  const info =
    `[cluster-env] ${table.length} cluster-only name(s); ${literalCount} literal read(s) under ` +
    `${CLUSTER_DIR}/ (${clusterFiles.length} file(s)); ${nonLiteralKeys.length} non-literal read(s)` +
    `${unrecordedSites === 0 ? ' (all recorded)' : ` (${unrecordedSites} UNRECORDED)`}; ` +
    `${NOT_CLUSTER_ONLY.size} recorded non-cluster name(s)`;
  if (problems.length === 0) {
    if (!opts.quiet) {
      console.log(`${info}: OK`);
      console.log(
        `[cluster-env] OK: every read under ${CLUSTER_DIR}/ is listed or recorded (including the ` +
          `non-literal ones), and every listed name is read somewhere in ${SRC_DIR}`,
      );
      // WHAT THIS PRINTED LINE MAY CLAIM (round 194, adversarial review of this guard): it used to
      // say "the fallback ERROR names: <table>" — a claim about a chain this file never reads. A
      // fixture whose CLUSTER_ONLY_ENV was declared and never used (no `from_env`, no ERROR) printed
      // the same sentence for all ten names. So the table is reported as the table it is, and the
      // part that only the wire can show is named as checked THERE.
      console.log(`[cluster-env] OK: CLUSTER_ONLY_ENV is ${table.join(', ')}`);
      console.log(
        '[cluster-env] note: that the fallback ERROR actually prints this table is NOT visible to a ' +
          'static scan — integration/test_startup_knobs.py reads this same table from the source and ' +
          'asserts on the wire that the ERROR names every entry (K10)',
      );
    }
    return 0;
  }

  console.error(`${info}: FAIL`);
  for (const p of problems) console.error(`[cluster-env]   ${p}`);
  console.error('[cluster-env] update the table in ' + OWNER + ' (it is what the operator sees)');
  return 1;
}

try {
  process.exit(main(process.argv.slice(2)));
} catch (err) {
  if (err instanceof ScanError) {
    console.error(`[cluster-env] CANNOT VERIFY: ${err.message}`);
    process.exit(err.code);
  }
  console.error(`[cluster-env] ERROR: ${err.message}`);
  process.exit(2);
}
