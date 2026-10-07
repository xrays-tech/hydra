#!/usr/bin/env node
/* Tests for scripts/check_alert_expressions.cjs.
 *
 * The checker guards the §9.1 alert table's PromQL against the metric registrations, so it
 * must be shown to FAIL on both kinds of drift (a metric that is not registered, a selector
 * on a label the metric does not have) and to refuse a pass when it found almost nothing.
 *
 * Run: node --test scripts/check_alert_expressions.test.cjs
 */
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const SCRIPT = path.join(__dirname, "check_alert_expressions.cjs");

let failures = 0;
function assert(name, cond, detail) {
  if (cond) console.log("PASS  " + name);
  else {
    failures++;
    console.error("FAIL  " + name + (detail ? "  -> " + detail : ""));
  }
}

const REGISTRATION = `pub fn m() {
    let a = register_int_gauge_vec!("hydra_fixture_gauge", "help", &["protocol"]);
    let b = register_int_counter!("hydra_fixture_counter", "help");
}`;

function run({ expression, extraRows = 0, min = 1, labelMin = 0, registration = REGISTRATION }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cae-"));
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(path.join(src, "metrics.rs"), registration);
  const rows = [`| A rule | \`${expression}\` | meaning |`];
  for (let i = 0; i < extraRows; i++) {
    rows.push(`| Filler ${i} | \`hydra_fixture_counter > ${i}\` | meaning |`);
  }
  fs.writeFileSync(
    path.join(dir, "ops.md"),
    `# Fixture\n\n### 9.1 Alerting: which metric means what\n\n| Alert | Expression | Meaning |\n|---|---|---|\n${rows.join("\n")}\n\n## 10 Next\n`,
  );
  try {
    const out = execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        CAE_OPS: path.join(dir, "ops.md"),
        CAE_SRC: src,
        CAE_MIN_REFS: String(min),
        // Focused fixtures carry one expression; the LABEL floor exists for the real §9.1 table and
        // has its own case below (which passes an explicit value), so overriding it here cannot
        // disable it — the same discipline the metric-reference floor already follows.
        CAE_MIN_LABELS: String(labelMin),
      },
    });
    return { status: 0, out };
  } catch (e) {
    return { status: e.status ?? 1, out: (e.stdout ?? "") + (e.stderr ?? "") };
  }
}

{
  const r = run({ expression: 'hydra_fixture_gauge{protocol="tls"} > 0' });
  assert("a registered metric with a real label passes", r.status === 0, `status=${r.status} ${r.out.trim().slice(0, 160)}`);
}
{
  const r = run({ expression: 'hydra_fixture_gauge{protocoll="tls"} > 0' });
  assert("a selector on an UNKNOWN label fails", r.status === 1, `status=${r.status}`);
  assert(
    "...and names the metric's real labels and the file",
    /registers labels \[protocol\]/.test(r.out) && /metrics\.rs/.test(r.out),
    r.out.trim().slice(0, 200),
  );
}
{
  const r = run({ expression: "hydra_fixture_gaugez > 0" });
  assert("an UNREGISTERED metric fails", r.status === 1, `status=${r.status}`);
  assert("...and suggests the near miss", /did you mean: hydra_fixture_gauge/.test(r.out), r.out.trim().slice(0, 200));
}
{
  // A metric registered WITHOUT labels must reject any selector on it.
  const r = run({ expression: 'hydra_fixture_counter{reason="full"} > 0' });
  assert("a selector on a metric that has NO labels fails", r.status === 1 && /registers labels \[none\]/.test(r.out), `status=${r.status}`);
}
{
  const r = run({ expression: 'hydra_fixture_gauge{protocol="tls"} > 0', min: 10, extraRows: 2 });
  assert(
    "too little coverage is a FAILURE, not a pass",
    r.status === 1 && /could be compared|proves nothing/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cae-empty-"));
  fs.writeFileSync(path.join(dir, "ops.md"), "# Fixture with no alert table\n");
  // `CAE_SRC` MUST point at a fixture this case owns, never at `os.tmpdir()`. Pointing a scanner at
  // the whole temp directory hands its verdict to whatever else is on the machine: measured in CI
  // on 2026-10-07 (the first run that reached this step), the runner's temp dir held an entry the
  // process could not read, `walk()` threw, node exited 1, and this assertion — whose whole point
  // is "2, never 0" — failed with `status=1` while passing on every developer machine.
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(path.join(src, "metrics.rs"), 'register_int_counter!("hydra_fixture_gauge", "h");\n');
  try {
    execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, CAE_OPS: path.join(dir, "ops.md"), CAE_SRC: src, CAE_MIN_REFS: "1" },
    });
    assert("a missing §9.1 table exits non-zero (2), never 0", false, "it exited 0");
  } catch (e) {
    assert("a missing §9.1 table exits non-zero (2), never 0", e.status === 2, `status=${e.status}`);
    assert(
      "...and says which document it could not find the table in",
      /cannot find the `### 9\.1 Alerting` table in /.test((e.stdout ?? "") + (e.stderr ?? "")),
      ((e.stdout ?? "") + (e.stderr ?? "")).trim().slice(0, 160),
    );
  }
}
{
  // An UNREADABLE source tree is CANNOT VERIFY (2), not a crash. `node` exits 1 on an uncaught
  // exception and 1 is this script's "violations found" verdict, so a checker that dies on an
  // inaccessible directory reports a real problem in the alert document that does not exist.
  // A regular FILE as `CAE_SRC` reproduces that deterministically on every platform (`readdir`
  // on a file is ENOTDIR — no chmod, and no dependence on running as root or not).
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cae-unreadable-"));
  const src = path.join(dir, "not-a-directory.rs");
  fs.writeFileSync(src, 'register_int_counter!("hydra_fixture_gauge", "h");\n');
  // The document must be READABLE, or the case would red on "cannot read <ops.md>" instead — the
  // source-tree failure is the one under test.
  fs.writeFileSync(path.join(dir, "ops.md"), "# Fixture\n\n### 9.1 Alerting\n\n| A | `hydra_fixture_gauge > 0` | m |\n\n## 10 Next\n");
  try {
    execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, CAE_OPS: path.join(dir, "ops.md"), CAE_SRC: src, CAE_MIN_REFS: "1" },
    });
    assert("an unreadable source tree is CANNOT VERIFY (2), not a crash (1)", false, "it exited 0");
  } catch (e) {
    const out = (e.stdout ?? "") + (e.stderr ?? "");
    assert("an unreadable source tree is CANNOT VERIFY (2), not a crash (1)", e.status === 2, `status=${e.status}`);
    assert(
      "...and names the path it could not read",
      /cannot read the source tree/.test(out) && /not-a-directory\.rs/.test(out),
      out.trim().slice(0, 200),
    );
  }
}

/* 118: a series registered ONLY inside `#[cfg(test)]` is not a live series — an alert on it can
 *      never fire, which is the failure this guard exists to catch. The fixture keeps one live
 *      registration so the scan stays non-empty (an empty scan is a separate CANNOT VERIFY path)
 *      and the char literal is deliberate: it is what used to derail test-item brace matching. */
{
  const registration = `pub fn m() {
    let b = register_int_counter!("hydra_fixture_counter", "help");
}

#[cfg(test)]
mod tests {
    const L: char = '{';
    #[test]
    fn t() { register_int_counter!("hydra_only_in_tests", "help"); }
}`;
  const r = run({ expression: "hydra_only_in_tests > 0", registration });
  assert(
    "an alert on a metric registered only inside #[cfg(test)] is reported",
    r.status === 1 && /hydra_only_in_tests/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}

/* ...and the CONTROL: the same macro outside the test module resolves, so the case above cannot
 *     be passing because string literals were blanked or the scanner broke. */
{
  const registration = `pub fn m() {
    let a = register_int_counter!("hydra_live_counter", "help");
}

#[cfg(test)]
mod tests {
    #[test]
    fn t() {}
}`;
  const r = run({ expression: "hydra_live_counter > 0", registration });
  assert(
    "CONTROL: a literal registration outside the test module resolves",
    r.status === 0,
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}

/* Round 122: the label-selector floor. `MIN_REFS` alone let the "every label key must exist"
 *      rule go vacuous — rewriting §9.1 into a shape without `{label="…"}` selectors (e.g.
 *      `sum by (…)`) still printed OK. */
{
  const r = run({ expression: 'hydra_fixture_gauge{protocol="tls"} > 0' });
  assert(
    "the label floor is satisfied by a normal expression (control)",
    r.status === 0,
    `status=${r.status} ${r.out.trim().slice(0, 140)}`,
  );
}/* Round 129: the FLOORS must be reported in the same run as a drift. They used to sit after the
 *      `problems` exit (so they never ran when a drift was present); moving them first hid the drift
 *      instead. Both now land in the same list. */
{
  const r = run({ expression: 'hydra_typo_metric > 0', min: 50 });
  assert(
    "a collapsed extraction AND a drift are both reported in one run",
    r.status === 1 && /hydra_typo_metric/.test(r.out) && /proves nothing/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}



/* Round 151: the floors count DISTINCT names, not occurrences. Measured on the real §9.1: 17 metric
 * references over 14 distinct metrics, 6 label selectors over 4 distinct `metric{label}` pairs — so
 * an occurrence floor could be satisfied by ONE series repeated, letting the rules inspect almost
 * nothing while still printing OK. */
{
  // The SAME metric ten times: occurrences = 10, distinct = 1.
  const repeated = Array.from({ length: 10 }, () => "hydra_fixture_counter > 0").join(" or ");
  const r = run({ expression: repeated, min: 10 });
  assert(
    "one metric repeated 10× does NOT satisfy the metric floor (it used to)",
    r.status === 1 && /only 1 DISTINCT metric\(s\)/.test(r.out),
    `status=${r.status} ${r.out.trim().split("\n")[0].slice(0, 170)}`,
  );
}
{
  // CONTROL: ten DIFFERENT metrics do satisfy it (the floor is not simply stricter for everyone).
  const many = Array.from({ length: 10 }, (_, i) => `hydra_fixture_${i} > 0`).join(" or ");
  const registration = Array.from(
    { length: 10 },
    (_, i) => `let m${i} = register_int_counter!("hydra_fixture_${i}", "help");`,
  ).join("\n");
  const r = run({ expression: many, min: 10, registration: `pub fn m() {\n${registration}\n}` });
  assert(
    "CONTROL: ten distinct metrics satisfy the same floor",
    r.status === 0,
    `status=${r.status} ${r.out.trim().slice(0, 170)}`,
  );
}

/* Round 153: the LABEL floor's own case. It was only ever probed by hand (`CAE_MIN_LABELS=7` in
 * round 122's notes) — every automated case neutralised it, so nothing would have noticed if the
 * check stopped working. The fixture carries ONE label selector, so a floor of 2 must fire. */
{
  const r = run({ expression: 'hydra_fixture_gauge{protocol="tls"} > 0', labelMin: 2 });
  assert(
    "the label-selector floor fires when the rule reaches too few distinct labels",
    r.status === 1 && /DISTINCT label selector\(s\)/.test(r.out),
    `status=${r.status} ${r.out.trim().split("\n")[0].slice(0, 170)}`,
  );
}
{
  // CONTROL: the same fixture with a floor of 1 passes, so the case above is about the floor only.
  const r = run({ expression: 'hydra_fixture_gauge{protocol="tls"} > 0', labelMin: 1 });
  assert(
    "CONTROL: the same expression passes with a floor of 1",
    r.status === 0,
    `status=${r.status} ${r.out.trim().slice(0, 170)}`,
  );
}

console.log(failures === 0 ? "\nALL ALERT-EXPRESSION TESTS PASSED" : `\n${failures} TEST(S) FAILED`);
process.exit(failures ? 1 : 0);
