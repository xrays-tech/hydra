#!/usr/bin/env node
'use strict';
/**
 * Every metric name the OPERATOR docs mention must exist in the code.
 *
 * Why: a documented series that does not exist is a dashboard panel or an alert rule that can
 * never resolve — the operator writes it, Prometheus accepts it, and nothing ever fires. The
 * repository already guards one slice of this (`check_alert_expressions.cjs` resolves the §9.1
 * ALERT TABLE), but the surrounding prose — §9's "Key series" list, cluster.md's metric table,
 * the ops instructions that name a series inline — was unguarded, and those are the names an
 * operator copies into a dashboard by hand.
 *
 * Measured 2026-09-30 while auditing this by hand: the first pass flagged
 * `hydra_sni_host_mismatch_total` as "documented but not registered" — and the audit was wrong,
 * not the doc: that counter is registered from a CONSTANT (`register_int_counter!(MISMATCH_METRIC,
 * …)` in `tls.rs`, into the default registry the exporter gathers). So this guard resolves the
 * three registration shapes the tree actually uses:
 *   1. `register_<kind>_<type>!("hydra_x", …)`            — a literal name
 *   2. `register_<kind>_<type>!(CONST_NAME, …)`            — a constant, whose `&str` literal is
 *                                                            looked up in the same file set
 *   3. `…with_opts(Opts::new("hydra_x", …))`               — an explicit `Opts` name
 * Anything else (a name built at runtime) is outside this guard's reach, which is why the checked
 * count is printed and floored: a scan that matches nothing must not pass.
 *
 * Names the docs mention ON PURPOSE as absent (e.g. "there is deliberately no
 * `hydra_proxy_listener_*` alias") are handled in TWO different ways, and the difference matters:
 *   • a WILDCARD (`hydra_x_*`) is skipped by the trailing-underscore rule — that is what the real
 *     note in `ops.md` §9.1 is, and the number of skipped wildcards is printed;
 *   • an EXACT name spelled as absent belongs in `ABSENT_ON_PURPOSE` with its reason.
 * An entry in that map which is NEVER consulted is DRIFT (round 144): it is the mechanism that would
 * suppress a real drift, and the four entries that used to live there were all dead — measured, none
 * of those names appears in any of the three operator docs, while the OK line advertised them as
 * "deliberately absent". The USED count is printed, so the claim matches the work actually done.
 *
 * Exit codes: 0 clean, 1 a documented name that does not exist (or a dead allowlist entry),
 * 2 CANNOT VERIFY.
 */
const fs = require('fs');
const path = require('path');
const { stripCommentsAndTestItems } = require('./rust_blank.cjs');
const { records, audit } = require('./recorded_exceptions.cjs');

// `__dirname` is `<repo>/scripts`; the override exists so the tests can point the guard at a
// throwaway tree. (The first version had an extra `'..'` inside the same `path.resolve`, which
// resolved to the PARENT of the repository and made every document "missing".)
const ROOT = process.env.CDM_DOCS_ROOT
  ? path.resolve(process.env.CDM_DOCS_ROOT)
  : path.resolve(__dirname, '..');
const DOCS = (process.env.CDM_DOCS || 'dev-docs/ops.md,dev-docs/cluster.md,dev-docs/tenant-api-integration.md')
  .split(',')
  .map((s) => s.trim())
  .filter(Boolean);
const CRATES = path.resolve(ROOT, process.env.CDM_CRATES || 'crates');
const MIN_CHECKED = Number(process.env.CDM_MIN_CHECKED ?? 20);

/** Names the docs mention only to say they do NOT exist. Each needs a reason.
 *
 * EMPTY ON PURPOSE, and an entry that is never consulted is now DRIFT (see `main`). The real note in
 * `ops.md` §9.1 is written as a WILDCARD — "there is deliberately **no** `hydra_proxy_listener_*`
 * alias" — and a wildcard token is skipped before this map is consulted, so the four entries that
 * used to live here (`hydra_proxy_listener_bound`, `…_tenant_certs`, `hydra_proxy_tenant_certs`,
 * `…_tls`) were dead: measured 2026-09-30, none of those exact names appears anywhere in the three
 * operator docs, while the OK line printed "4 name(s) allowlisted as deliberately absent" — a claim
 * about work that never happened, and a place where a real drift could have hidden unnoticed.
 *
 * Add a name here only when a doc line spells that EXACT name as absent.
 */
const BUILTIN_ABSENT_ON_PURPOSE = new Map();

/** `CDM_ABSENT_ON_PURPOSE=a,b` exists so the suite can exercise both the used and the stale path. */
const ABSENT_ON_PURPOSE = new Map([
  ...BUILTIN_ABSENT_ON_PURPOSE,
  ...(process.env.CDM_ABSENT_ON_PURPOSE || '')
    .split(',')
    .map((s) => s.trim())
    .filter(Boolean)
    .map((n) => [n, 'named via CDM_ABSENT_ON_PURPOSE']),
]);

/**
 * `hydra_core` / `hydra_server` / … are crate, tool, package or binary names, not metrics.
 *
 * Every entry here must EARN its place (round 181): it either skips a name the scanned operator docs
 * really mention, or a real crate/tool/package directory (or a package's own name/bin) carries it.
 * Measured 2026-10-01: of the nine entries, only `hydra_core` is actually mentioned by the three docs;
 * `hydra_server` / `hydra_py` / `hydra_ts` / `hydra_go` are directories, `hydra_sdk` is the Python
 * package directory and `hydra_admin` the CLI package's own name — while **`hydra_dev` and `hydra_ui`
 * appeared in nothing but this list** (dead exclusions, deleted this round: an exclusion nobody uses is
 * exactly where a real documented metric can hide). The rule is asserted below, not assumed.
 */
const NOT_A_METRIC = records(process.env.CDM_NOT_A_METRIC, [
  ['hydra_core', 'the `crates/hydra-core` crate'],
  ['hydra_server', 'the `crates/hydra-server` crate'],
  ['hydra_sdk', 'the `tools/hydra-py/hydra_sdk` package directory'],
  ['hydra_admin', "the `tools/hydra-cli` package's own name and binary (`hydra-admin`)"],
  ['hydra_py', 'the `tools/hydra-py` tool'],
  ['hydra_ts', 'the `tools/hydra-ts` tool'],
  ['hydra_go', 'the `tools/hydra-go` tool'],
]);

/** Does a crate / tool / Python package / package name carry this `hydra_x` name? */
function notAMetricJustified(name) {
  const dashed = name.replace(/^hydra_/, 'hydra-');
  const dirs = [
    path.join(ROOT, 'crates', dashed),
    path.join(ROOT, 'tools', dashed),
    path.join(ROOT, 'tools', 'hydra-py', name),
    path.join(ROOT, 'tools', 'hydra-ts', name),
    path.join(ROOT, 'tools', 'hydra-go', name),
  ];
  if (dirs.some((d) => fs.existsSync(d) && fs.statSync(d).isDirectory())) return true;
  for (const rel of ['tools/hydra-cli/package.json', 'tools/hydra-ts/package.json', 'tools/hydra-py/pyproject.toml', 'tools/hydra-go/go.mod']) {
    const f = path.join(ROOT, rel);
    if (fs.existsSync(f) && fs.readFileSync(f, 'utf8').includes(dashed)) return true;
  }
  return false;
}

function walk(dir, out = []) {
  if (!fs.existsSync(dir)) return out;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'target' || entry.name === '.git') continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) {
      // `tests/` is the OTHER way a registration can be test-only: those files are compiled
      // only by `cargo test`, so a series registered there is never exported by a deployment.
      // See the `#[cfg(test)]` stripping in `registeredNames` for the in-file case.
      if (entry.name === 'tests') continue;
      walk(full, out);
    } else if (entry.name.endsWith('.rs')) out.push(full);
  }
  return out;
}

/** Every metric name registered anywhere under `crates/`. */
function registeredNames() {
  const names = new Set();
  const files = walk(CRATES);
  // Blank comments/strings, then whole `#[cfg(test)]` items: a name registered only inside a
  // test module is NOT a live series (`register_int_counter!("hydra_unused_test_marker", …)` in
  // `admin/metrics.rs`'s test module used to count as registered — a documented panel pointing at
  // it would still never resolve, which is precisely what this guard claims to catch).
  const sources = files.map((f) => [f, stripCommentsAndTestItems(fs.readFileSync(f, 'utf8'))]);
  const literal = /register_(?:int_|uint_)?(?:counter|gauge|histogram)(?:_vec|_with_opts)?!\(\s*"([a-z][a-z0-9_]*)"/g;
  const viaConst = /register_(?:int_|uint_)?(?:counter|gauge|histogram)(?:_vec|_with_opts)?!\(\s*([A-Z][A-Z0-9_]*)/g;
  const viaOpts = /Opts::new\(\s*"([a-z][a-z0-9_]*)"/g;
  /**
   * The string a constant (or static) holds, in the whole crate set (registration uses a module
   * constant). ANY type annotation is accepted: requiring `: &str` meant a `&'static str` or `String`
   * declaration would leave the metric looking UNREGISTERED, i.e. a documented series reported as
   * DRIFT while the code registers it (round 152 — latent today: every real declaration is `: &str`).
   */
  const constValue = (name) => {
    const re = new RegExp(
      `(?:pub(?:\\([^)]*\\))?\\s+)?(?:const|static)\\s+${name}\\s*:[^=]+=\\s*"([a-z][a-z0-9_]*)"`,
    );
    for (const [, text] of sources) {
      const m = text.match(re);
      if (m) return m[1];
    }
    return null;
  };
  for (const [, text] of sources) {
    for (const m of text.matchAll(literal)) names.add(m[1]);
    for (const m of text.matchAll(viaOpts)) names.add(m[1]);
    for (const m of text.matchAll(viaConst)) {
      const resolved = constValue(m[1]);
      if (resolved) names.add(resolved);
    }
  }
  return names;
}

function main() {
  const missingDocs = DOCS.filter((d) => !fs.existsSync(path.join(ROOT, d)));
  if (missingDocs.length) {
    console.error(`[doc-metrics] CANNOT VERIFY: missing document(s): ${missingDocs.join(', ')}`);
    process.exit(2);
  }
  const names = registeredNames();
  if (names.size === 0) {
    console.error('[doc-metrics] CANNOT VERIFY: no registered metric found under crates/ (the scan is broken)');
    process.exit(2);
  }
  let checked = 0;
  // The FLOOR counts DISTINCT names, not occurrences (round 156): a single name mentioned
  // twenty times satisfied `MIN_CHECKED` while the rule inspected exactly one series, and the OK
  // line called the occurrence count "N documented metric name(s)" (measured on the real docs:
  // 69 mentions of 36 distinct names).
  const distinctNames = new Set();
  let wildcardsSkipped = 0;
  const absentUsed = new Set();
  const notAMetricSkipped = new Set();
  const problems = [];
  for (const doc of DOCS) {
    const text = fs.readFileSync(path.join(ROOT, doc), 'utf8');
    text.split('\n').forEach((line, i) => {
      for (const m of line.matchAll(/\bhydra_[a-z][a-z0-9_]*\b/g)) {
        const name = m[0];
        if (NOT_A_METRIC.has(name)) {
          notAMetricSkipped.add(name);
          continue;
        }
        if (name.endsWith('_')) {
          // A wildcard prefix, not a series. NOTE this — not the allowlist — is what makes the
          // `hydra_proxy_listener_*` note in ops.md §9.1 pass, and it is counted so the OK line can
          // say so instead of attributing the pass to allowlist entries that were never consulted.
          wildcardsSkipped += 1;
          continue;
        }
        checked += 1;
        distinctNames.add(name);
        if (names.has(name)) continue;
        if (ABSENT_ON_PURPOSE.has(name)) {
          absentUsed.add(name);
          continue;
        }
        const near = [...names].filter((n) => n.includes(name.split('_')[1] || '###')).slice(0, 3);
        problems.push(
          `${doc}:${i + 1}: \`${name}\` is not registered anywhere in crates/ — a dashboard or ` +
            `alert written from this line can never resolve${near.length ? ` (did you mean ${near.join(', ')}?)` : ''}`
        );
      }
    });
  }
  // An exclusion that skips nothing and names nothing here is a finding (round 181): the list is a
  // place a real "documented metric that does not exist" can hide, because a name in it is skipped
  // BEFORE the registration lookup. Measured when this rule was added: `hydra_dev` and `hydra_ui`
  // qualified on neither count and were deleted.
  // The shared algebra's STALE direction: a record applies only while the name it stands for is real —
  // either the docs mention it (so it really skips something) or a crate/tool/package carries it.
  const { stale: deadExclusions } = audit({
    records: NOT_A_METRIC,
    needed: [],
    applies: (name) => notAMetricSkipped.has(name) || notAMetricJustified(name),
  });
  for (const name of deadExclusions) {
    problems.push(
      `${name} is in NOT_A_METRIC but skips NOTHING in the scanned docs (${DOCS.join(', ')}) and names no ` +
        `crate/tool/package in this tree: an unused exclusion is where a documented-but-absent metric ` +
        `hides — delete the entry or point it at the crate/tool it stands for`,
    );
  }

  // A STALE allowlist entry is a finding, not a harmless leftover: it is the exact mechanism that
  // would suppress a real drift, and an entry nobody needs is an entry nobody has re-checked (the
  // same rule `check_documented_defaults` applies to its `UNVERIFIED_OK` list).
  const { stale: deadAllowlist } = audit({
    records: ABSENT_ON_PURPOSE,
    needed: [...absentUsed],
    applies: (name) => absentUsed.has(name),
  });
  const stale = [];
  for (const name of deadAllowlist) {
    stale.push(
      `[doc-metrics] DRIFT the ABSENT_ON_PURPOSE entry \`${name}\` is never consulted: no documented ` +
        `line spells that exact name, so the entry is dead — delete it (a wildcard note is handled by ` +
        `the trailing-underscore rule) or fix the name it was meant to cover`,
    );
  }
  // The floor is a CANNOT VERIFY (exit 2) — a STRONGER statement than a drift — but it used to print
  // alone and hide any drift that came with it (round 136). Both are printed now; the exit code stays
  // 2, because "this check did not really run" is the more important fact for the operator.
  const floorBroken = distinctNames.size < MIN_CHECKED;
  if (floorBroken) {
    for (const p of problems) console.error(`[doc-metrics] DRIFT ${p}`);
    for (const p of stale) console.error(p);
    console.error(
      `[doc-metrics] CANNOT VERIFY: only ${distinctNames.size} DISTINCT documented name(s) checked ` +
        `(< ${MIN_CHECKED}) across ${checked} mention(s); the scan is probably looking at the wrong files`
    );
    process.exit(2);
  }
  if (problems.length || stale.length) {
    for (const p of problems) console.error(`[doc-metrics] DRIFT ${p}`);
    for (const p of stale) console.error(p);
    // Both kinds are counted separately: the finding list carries two different problems, and a
    // summary that calls a dead allowlist entry a "missing metric name" misdescribes its own output.
    console.error(
      `[doc-metrics] ${problems.length} documented-but-missing metric name(s), ` +
        `${stale.length} dead allowlist entr(ies)`,
    );
    process.exit(1);
  }
  // Built as a value, not as a nested template literal: a backtick inside `${…}` inside a backtick
  // template terminates the outer one (`SyntaxError: Invalid or unexpected token` — measured).
  const allowlistNote = absentUsed.size
    ? `: ${[...absentUsed].join(', ')}`
    : ' (the deliberate-absence note in the docs is a wildcard, which the trailing-underscore rule handles)';
  console.log(
    `OK  (${checked} documented mention(s) over ${distinctNames.size} distinct metric name(s) across ` +
      `${DOCS.length} operator doc(s) all resolve against ${names.size} registered series; ` +
      `${notAMetricSkipped.size} crate/tool name(s) skipped by NOT_A_METRIC ` +
      `(${[...notAMetricSkipped].sort().join(', ') || 'none'}); ${wildcardsSkipped} wildcard prefix(es) skipped; ` +
      `${absentUsed.size} of ${ABSENT_ON_PURPOSE.size} allowlisted name(s) were actually needed` +
      `${allowlistNote})`
  );
  return 0;
}

if (require.main === module) process.exit(main());
module.exports = { registeredNames };
