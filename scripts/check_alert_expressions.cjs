#!/usr/bin/env node
/* Guard: the §9.1 ALERT TABLE's PromQL must reference metrics and labels that exist.
 *
 * `dev-docs/ops.md` §9.1 is an explicit contract: "every expression uses a metric name and
 * label that really exists in this codebase". The alert-rule FILES live in the operations
 * repository, so a typo here is not a doc nit — it is a rule that can never fire, i.e. a
 * silent monitoring hole that only shows up during an incident. Nothing compared the table
 * to the registrations until now.
 *
 * Checked (both FAIL the run):
 *   - every `hydra_*` name in an expression is registered somewhere in `crates/<crate>/src`
 *     (names built from a `const` are resolved);
 *   - every label key inside a `{…}` selector is one of THAT metric's registered labels
 *     (a selector naming a label the metric does not have matches nothing, ever).
 * Reported but NOT failed (see the header note in the report):
 *   - label VALUES (`{reason=~"no_key|breaker_dead"}`). A presence check on the literal is
 *     both too weak and too strong: it cannot tell a live value from a dead one (the table
 *     itself warns that `stage="connect"` is emitted by nothing), and `result=~".+_throttled"`
 *     is composed at runtime so the literal legitimately does not appear. These are printed
 *     as observations for a human, never as a pass or a failure.
 *
 * Exit 0 = every metric name and label key resolves · 1 = drift, or too few references
 * checked (coverage floor, counted over DISTINCT names — round 151) · 2 = the inputs could not be read.
 *
 * Falsified: renaming a label key or a metric in an expression turns this red; so does an
 * empty table (see `check_alert_expressions.test.cjs`).
 */
"use strict";
const fs = require("fs");
const path = require("path");
const { stripCommentsAndTestItems } = require("./rust_blank.cjs");

const ROOT = path.join(__dirname, "..");
const OPS = process.env.CAE_OPS ?? path.join(ROOT, "dev-docs", "ops.md");
const SRC_DIRS = (process.env.CAE_SRC ?? "crates/hydra-server/src,crates/hydra-core/src")
  .split(",")
  .filter(Boolean);
/**
 * Floors on what the rule actually RAN OVER — counted as DISTINCT names, not occurrences.
 *
 * Measured 2026-09-30: §9.1 has 17 metric references (14 distinct metrics) and 6 label selectors
 * (4 distinct `metric{label}` pairs). Counting occurrences meant one metric mentioned ten times
 * satisfied `MIN_REFS`, so a section rewritten around a single series would have kept printing OK
 * while the rules inspected almost nothing. The distinct numbers are what the floors now use.
 */
const MIN_REFS = Number(process.env.CAE_MIN_REFS ?? 10);   // distinct metrics; measured 14
// Label floor: measured 4 distinct `metric{label}` selectors. A floor EQUAL to the measurement has
// zero margin — consolidating two series onto one label key would redden a correct document — so this
// sits at 3 (the `MIN_E2E_SPECS` lesson, round 133).
const MIN_LABELS = Number(process.env.CAE_MIN_LABELS ?? 3);  // distinct label selectors; measured 4
const SECTION = "### 9.1 Alerting";

/** metric name -> { labels, file } from every `register_*!` macro under crates/<crate>/src. */
function registeredMetrics(sources) {
  const consts = {};
  for (const f of sources) {
    for (const m of f.text.matchAll(/const\s+([A-Z][A-Z0-9_]*)\s*:\s*&str\s*=\s*"([^"]+)"/g)) {
      consts[m[1]] = m[2];
    }
  }
  const name = (expr) => {
    const e = expr.trim();
    return e.startsWith('"') ? e.slice(1, -1) : consts[e] ?? null;
  };
  const out = new Map();
  for (const f of sources) {
    // with a label list
    for (const m of f.text.matchAll(/register_\w+!\(\s*([A-Za-z0-9_:]+|"[^"]+")\s*,\s*"[^"]*"\s*,\s*&\[([^\]]*)\]/g)) {
      const n = name(m[1]);
      if (n) out.set(n, { labels: [...m[2].matchAll(/"([^"]+)"/g)].map((x) => x[1]), file: f.rel });
    }
    // without one
    for (const m of f.text.matchAll(/register_\w+!\(\s*([A-Za-z0-9_:]+|"[^"]+")\s*,\s*"([^"]*)"\s*\)/g)) {
      const n = name(m[1]);
      if (n && !out.has(n)) out.set(n, { labels: [], file: f.rel, help: m[2] });
    }
  }
  return out;
}

/** The §9.1 table's expression cells (one per row). */
function alertExpressions(opsText) {
  const parts = opsText.split(SECTION);
  if (parts.length < 2) return null;
  const body = parts[1].split("\n## ")[0];
  const cells = [];
  for (const line of body.split("\n")) {
    if (!line.startsWith("| ") || !line.includes("hydra_")) continue;
    const parts2 = line.replace(/^\|/, "").replace(/\|$/, "").split("|");
    if (parts2.length > 2) cells.push(parts2[1]);
  }
  return cells;
}

function walk(dir, out = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) walk(full, out);
    else if (e.name.endsWith(".rs")) out.push(full);
  }
  return out;
}

function main() {
  let opsText;
  try {
    opsText = fs.readFileSync(OPS, "utf8");
  } catch (e) {
    console.error(`cannot read ${OPS}: ${e.message}`);
    process.exit(2);
  }
  const unreadable = [];
  const files = SRC_DIRS.flatMap((d) => {
    const full = path.isAbsolute(d) ? d : path.join(ROOT, d);
    if (!fs.existsSync(full)) return [];
    // A tree the checker cannot READ is CANNOT VERIFY (2), never a crash. `walk()` used to throw
    // straight out of `main()`: node exits 1 on an uncaught exception, and 1 is this script's
    // "violations found" verdict — so an unreadable directory would have been reported as a real
    // alert-document problem. Measured in CI (2026-10-07, the first run that reached this step):
    // the test suite pointed `CAE_SRC` at the whole `os.tmpdir()`, whose entries are not the
    // test's to control, hit one it could not read, and the suite saw `status=1` where the
    // asserted contract is 2.
    try {
      return walk(full);
    } catch (e) {
      unreadable.push(`${full}: ${e.code ?? e.message}`);
      return [];
    }
  });
  if (unreadable.length > 0) {
    console.error(`cannot read the source tree — nothing could be checked:\n  ${unreadable.join("\n  ")}`);
    process.exit(2);
  }
  if (files.length === 0) {
    console.error("no source files found — nothing could be checked");
    process.exit(2);
  }
  // Comments AND whole `#[cfg(test)]` items are blanked (string literals kept: the registration
  // name IS a literal). A series registered only in a test module is never exported by a
  // deployment, so an alert on it is exactly as dead as an alert on a name that does not
  // exist — the failure this guard exists to catch. Found in round 118: without this,
  // `admin/metrics.rs`'s test-only `hydra_unused_test_marker` counted as a live series.
  const sources = files.map((f) => ({
    rel: path.relative(ROOT, f),
    text: stripCommentsAndTestItems(fs.readFileSync(f, "utf8")),
  }));

  const metrics = registeredMetrics(sources);
  // The floor protects the REAL tree (a broken extractor must not look like a clean run).
  // An explicit CAE_SRC is a deliberate override — that is how the test suite points this
  // at a fixture with two registrations.
  if (metrics.size < 20 && !process.env.CAE_SRC) {
    console.error(`only ${metrics.size} metric registration(s) parsed — the extractor is probably broken`);
    process.exit(2);
  }
  if (metrics.size === 0) {
    console.error("no metric registration parsed at all — nothing could be checked");
    process.exit(2);
  }
  const cells = alertExpressions(opsText);
  if (cells === null) {
    console.error(`cannot find the \`${SECTION}\` table in ${OPS}`);
    process.exit(2);
  }

  const problems = [];
  const observations = [];
  let refs = 0;
  let labelsChecked = 0;
  // The DISTINCT sets are what the floors below use: an occurrence count lets one metric mentioned
  // many times (or one label repeated across expressions) satisfy a floor meant to prove breadth.
  const distinctMetrics = new Set();
  const distinctLabels = new Set();
  for (const expr of cells) {
    for (const m of expr.matchAll(/(hydra_[a-z0-9_]+)(\{[^}]*\})?/g)) {
      const [, name, selector] = m;
      refs++;
      distinctMetrics.add(name);
      if (!metrics.has(name)) {
        const near = [...metrics.keys()].filter((k) => k.startsWith(name.slice(0, 12))).slice(0, 3);
        problems.push(`${name} is not registered anywhere in crates/*/src (did you mean: ${near.join(", ") || "—"}?)`);
        continue;
      }
      const known = metrics.get(name).labels;
      if (!selector) continue;
      for (const s of selector.matchAll(/([a-z_][a-z0-9_]*)\s*(=~|!~|=|!=)\s*"([^"]*)"/g)) {
        const [, label, op, pattern] = s;
        labelsChecked++;
        distinctLabels.add(`${name}{${label}}`);
        if (!known.includes(label)) {
          problems.push(
            `${name}{${label}…}: the metric registers labels [${known.join(", ") || "none"}] ` +
              `@ ${metrics.get(name).file}, so a selector on \`${label}\` can never match`,
          );
        }
        const alts = pattern.split("|").filter((a) => a && !a.includes("."));
        if (alts.length) {
          observations.push(`value ${label}${op}"${pattern}" — alternatives ${JSON.stringify(alts)}`);
        }
      }
    }
  }

  for (const o of observations) console.log(`  note  ${o}`);
  // The FLOORS are recorded as problems, not as their own exits (round 129). They used to sit
  // AFTER the `problems` exit, so the situation they exist for — "the extraction collapsed and this
  // check proves nothing" — could never be printed when a drift was also present; and when they
  // were merely moved first, they hid the drift instead. Both now land in the same list, so one run
  // reports everything that is wrong.
  if (distinctMetrics.size < MIN_REFS) {
    problems.push(
      `only ${distinctMetrics.size} DISTINCT metric(s) referenced in ${SECTION} (< ${MIN_REFS}) ` +
        `across ${refs} reference(s); the table or the extraction changed, so this check proves nothing`,
    );
  }
  if (distinctLabels.size < MIN_LABELS) {
    problems.push(
      `only ${distinctLabels.size} DISTINCT label selector(s) in ${SECTION} (< ${MIN_LABELS}) across ` +
        `${labelsChecked} selector(s); the rule "every label key in an alert expression must exist on ` +
        `that metric" is not being exercised — either the expressions stopped using {label="…"} or the ` +
        `extraction broke`,
    );
  }
  if (problems.length) {
    for (const p of problems) console.error("DRIFT  " + p);
    console.error(
      `${problems.length} alert-expression reference problem(s) (${refs} metric reference(s), ` +
        `${distinctMetrics.size} distinct)`,
    );
    process.exit(1);
  }
  console.log(
    `OK  (${refs} metric reference(s) over ${distinctMetrics.size} distinct metric(s) and ` +
      `${labelsChecked} label selector(s) over ${distinctLabels.size} distinct pair(s) in ${SECTION} ` +
      `resolve against ${metrics.size} registered metric(s); label VALUES are reported above, not asserted)`,
  );
}

if (require.main === module) main();

module.exports = { registeredMetrics, alertExpressions };
