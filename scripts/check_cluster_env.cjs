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
 * The NON-literal env reads under `src/cluster/`, keyed by SITE (`<file>#<ordinal>`, one-based),
 * with the reason. A name the guard cannot see is a name the completeness rule cannot judge, so each
 * such site must be a recorded decision — and the record expires if that site disappears.
 *
 * The key is the site, NOT the file (round 194, found by an adversarial review of the guard itself):
 * the first version recorded the FILE, so the reason written for the `CLUSTER_ONLY_ENV` loop —
 * "it reads exactly the table, by construction" — silently vouched for every future
 * `std::env::var(<expression>)` in that same file, including one reading a name that is not in the
 * table at all (measured with a fixture: `env::var(SECRET_KNOB)` with `HYDRA_SECRET_KNOB` absent
 * from the table passed with exit 0 and an OK line claiming every read was accounted for). Ordinals
 * instead of line numbers so ordinary edits above a site do not churn the record.
 */
const NON_LITERAL_READ_OK = records(process.env.CLUSTER_ENV_NON_LITERAL_OK, [
  [
    'crates/hydra-server/src/cluster/mod.rs#1',
    'the CLUSTER_ONLY_ENV loop in NodeRole::from_env: it reads exactly the table, by construction',
  ],
  [
    'crates/hydra-server/src/cluster/arachne_node.rs#1',
    'cluster_enabled(): the single read behind the cluster/single-node decision (ADR-0001). It reads PEERS_ENV, a constant declared in that file and nowhere else, so the indirection cannot hide a cluster-only name — that name is the one thing it exists to read. DEFERRED (tracked by plan T1.3 / T4.2): replacing the HYDRA_ROLE-based decision with this one, adding HYDRA_CLUSTER_PEERS and HYDRA_ARACHNE_LISTEN to the table, and removing the seven retired names all happen in the wiring step; until then the two new variables are deliberately absent from the table rather than listed while nothing reads them through it.',
  ],
]);

/** `{key, file, ordinal}` for one recorded non-literal site. */
function parseSiteKey(key) {
  const m = String(key).match(/^(.*)#(\d+)$/);
  return m ? { file: m[1], ordinal: Number(m[2]) } : null;
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
    for (const [i, line] of stripCommentsAndTestItems(fs.readFileSync(file, 'utf8')).split('\n').entries()) {
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
          nonLiteral.get(rel).push(at);
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
  for (const [file, sites] of nonLiteralFiles) {
    for (let i = 1; i <= sites.length; i += 1) nonLiteralKeys.push(`${file}#${i}`);
  }
  const nonLiteralAudit = audit({
    records: NON_LITERAL_READ_OK,
    needed: nonLiteralKeys,
    applies: (key) => {
      const site = parseSiteKey(key);
      return Boolean(site) && (nonLiteralFiles.get(site.file) || []).length >= site.ordinal;
    },
  });
  for (const key of nonLiteralAudit.unrecorded) {
    const site = parseSiteKey(key);
    const at = site ? nonLiteralFiles.get(site.file)[site.ordinal - 1] : key;
    problems.push(
      `${at} reads the environment through a NON-literal argument — the completeness rule cannot ` +
        `see which name that is, so this SITE must be recorded in NON_LITERAL_READ_OK with the ` +
        `reason the indirection cannot hide a cluster-only name`,
    );
  }
  for (const key of nonLiteralAudit.stale) {
    const site = parseSiteKey(key);
    problems.push(
      `NON_LITERAL_READ_OK records ${key}, but that site is gone (${site ? site.file : key} now has ` +
        `${site ? (nonLiteralFiles.get(site.file) || []).length : 0} non-literal read(s)) ` +
        `— a recorded decision that cannot expire is a stale claim`,
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
