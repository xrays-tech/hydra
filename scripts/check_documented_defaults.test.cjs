#!/usr/bin/env node
/* Tests for scripts/check_documented_defaults.cjs.
 *
 * The checker guards "the defaults in ops.md match the code", so the checker itself must
 * be shown to FAIL on drift — in BOTH directions (the doc moving, the code moving) — and
 * to refuse a pass when it can compare almost nothing. A guard that quietly compares
 * nothing is the failure mode this whole remediation keeps running into.
 *
 * Run: node --test scripts/check_documented_defaults.test.cjs
 */
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const SCRIPT = path.join(__dirname, "check_documented_defaults.cjs");
const { documentedDefault, numericLiteral } = require(SCRIPT);

let failures = 0;
function assert(name, cond, detail) {
  if (cond) console.log("PASS  " + name);
  else {
    failures++;
    console.error("FAIL  " + name + (detail ? "  -> " + detail : ""));
  }
}

/** A throwaway fixture: an ops.md table + a Rust source with one knob. */
function fixture({ docDefault, codeDefault, rows = 1 }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cdd-"));
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  const lines = [];
  for (let i = 0; i < rows; i++) {
    lines.push(
      `| \`HYDRA_TEST_KNOB_${i}\` | \`${docDefault}\` | A fixture knob. |`,
    );
  }
  fs.writeFileSync(path.join(dir, "ops.md"), `# Fixture\n\n${lines.join("\n")}\n`);
  const fns = [];
  for (let i = 0; i < rows; i++) {
    fns.push(`fn knob_${i}() -> u64 { std::env::var("HYDRA_TEST_KNOB_${i}").ok().and_then(|v| v.parse().ok()).unwrap_or(${codeDefault}) }`);
  }
  fs.writeFileSync(path.join(src, "knobs.rs"), fns.join("\n") + "\n");
  return { dir, ops: path.join(dir, "ops.md"), src };
}

function run(fx, min = 1) {
  try {
    const out = execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, CDD_OPS: fx.ops, CDD_SRC: fx.src, CDD_MIN_COMPARED: String(min) },
    });
    return { status: 0, out };
  } catch (e) {
    return { status: e.status ?? 1, out: (e.stdout ?? "") + (e.stderr ?? "") };
  }
}

// ---- the two pure helpers ----------------------------------------------------
assert(
  "documentedDefault reads a numeric cell",
  documentedDefault("| `HYDRA_X` | `60` | desc |")?.value === 60,
);
assert(
  "documentedDefault reads a prose default",
  documentedDefault("| `HYDRA_X` | prose | drains (default 20) |")?.value === 20,
);
assert(
  "documentedDefault reads the byte count, not the MiB number",
  documentedDefault("| `HYDRA_X` | 32 MiB (= 33554432 **bytes**) | desc |")?.value === 33554432,
);
assert(
  "documentedDefault refuses a host:port cell (not a number)",
  documentedDefault("| `HYDRA_X` | `127.0.0.1:8081` | desc |") === null,
);
assert("documentedDefault refuses *(unset)*", documentedDefault("| `HYDRA_X` | *(unset)* | desc |") === null);
assert("numericLiteral evaluates 32 * 1024 * 1024", numericLiteral("32 * 1024 * 1024") === 33554432);
assert("numericLiteral undoes Rust digit separators", numericLiteral("5_000") === 5000);

// ---- end to end --------------------------------------------------------------
{
  const fx = fixture({ docDefault: 7, codeDefault: 7 });
  const r = run(fx);
  assert("a matching default passes", r.status === 0, `status=${r.status} out=${r.out.trim().slice(0, 160)}`);
}
{
  const fx = fixture({ docDefault: 8, codeDefault: 7 });
  const r = run(fx);
  assert("a DRIFTED default fails", r.status === 1, `status=${r.status}`);
  assert(
    "...and names BOTH numbers and the code site",
    /ops\.md says 8/.test(r.out) && /falls back to 7/.test(r.out) && /\.rs:/.test(r.out),
    r.out.trim().slice(0, 200),
  );
}
{
  const fx = fixture({ docDefault: 7, codeDefault: 8 });
  const r = run(fx);
  // The other direction: the CODE moved. Same comparison, so the same failure — and the
  // message must still point at the code, because that is what an operator has to fix.
  assert("the code moving is caught too", r.status === 1 && /falls back to 8/.test(r.out), `status=${r.status}`);
}
{
  const fx = fixture({ docDefault: 7, codeDefault: 7, rows: 2 });
  const r = run(fx, 5);
  assert(
    "too little coverage is a FAILURE, not a pass",
    r.status === 1 && /could be compared/.test(r.out),
    `status=${r.status} out=${r.out.trim().slice(0, 160)}`,
  );
}
{
  const fx = fixture({ docDefault: 7, codeDefault: 7, rows: 1 });
  fs.writeFileSync(fx.ops, "# Fixture with no table at all\n");
  const r = run(fx);
  assert(
    "an input with no comparable rows also fails (never a silent pass)",
    r.status === 1 && /could be compared/.test(r.out),
    `status=${r.status}`,
  );
}

/* ---- round 119: string defaults, and "unverifiable" as a recorded decision -------------- */

/** A fixture whose knob has a STRING default carried by a constant (the address shape). */
function stringFixture({ docDefault, codeDefault }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cdd-str-"));
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "ops.md"),
    `# Fixture\n\n| \`HYDRA_TEST_ADDR\` | \`${docDefault}\` | A fixture address. |\n`,
  );
  fs.writeFileSync(
    path.join(src, "knobs.rs"),
    `const DEFAULT_TEST_ADDR: &str = "${codeDefault}";\nfn addr() -> String { std::env::var("HYDRA_TEST_ADDR").unwrap_or_else(|_| DEFAULT_TEST_ADDR.to_string()) }\n`,
  );
  return { dir, ops: path.join(dir, "ops.md"), src };
}

{
  const r = run(stringFixture({ docDefault: "127.0.0.1:8081", codeDefault: "127.0.0.1:8081" }));
  assert(
    "a STRING default documented in a cell is compared against the code constant",
    r.status === 0 && /HYDRA_TEST_ADDR = 127\.0\.0\.1:8081/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  // The security-relevant direction: the doc says loopback, the code binds everywhere.
  const r = run(stringFixture({ docDefault: "127.0.0.1:8081", codeDefault: "0.0.0.0:8081" }));
  assert(
    "a STRING default that differs is reported as drift (doc says loopback, code binds all)",
    r.status === 1 && /DRIFT/.test(r.out) && /0\.0\.0\.0:8081/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  // An unverifiable row is now a DECISION: nothing in UNVERIFIED_OK matches this fixture name.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cdd-unv-"));
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "ops.md"),
    "# Fixture\n\n| `HYDRA_TEST_UNCOMPARABLE` | `on` | a word, not a value. |\n",
  );
  fs.writeFileSync(path.join(src, "knobs.rs"), 'fn f() -> u8 { 1 }\n');
  const r = run({ dir, ops: path.join(dir, "ops.md"), src });
  assert(
    "a row the extraction cannot compare FAILS unless it is recorded (no silent ?????)",
    r.status === 1 && /is not recorded in UNVERIFIED_OK/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}

/* Round 132: the knob lookup used `line.includes(varName)`, so a NEIGHBOURING knob's line matched.
 * Measured: with `HYDRA_LISTEN` documented, the line `env::var("HYDRA_LISTEN_EXTRA").unwrap_or(9999)`
 * was taken as its code default and the guard reported "OK: documented default matches the code"
 * (9999) while the REAL `HYDRA_LISTEN` fallback in the same file was 1111 — a documented default
 * validated by the wrong knob. The lookup now requires a TOKEN match (`\bNAME\b`). */
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cdd-neighbour-"));
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "ops.md"),
    "# Fixture\n\n| Variable | Default |\n|---|---|\n| `HYDRA_LISTEN` | `9999` |\n",
  );
  fs.writeFileSync(
    path.join(src, "knobs.rs"),
    'fn extra() -> u32 { std::env::var("HYDRA_LISTEN_EXTRA").unwrap_or(9999) }\n' +
      'fn real() -> u32 { std::env::var("HYDRA_LISTEN").unwrap_or(1111) }\n',
  );
  const r = run({ dir, ops: path.join(dir, "ops.md"), src });
  assert(
    "a neighbouring knob's default does not validate the documented row",
    r.status === 1 && /HYDRA_LISTEN = 1111/.test(r.out) && /ops\.md says 9999/.test(r.out),
    `status=${r.status} out=${r.out.trim().slice(0, 200)}`,
  );
}
{
  // CONTROL: documenting the REAL default passes — so the case above fails because of the VALUE,
  // not because the guard stopped comparing this row at all.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cdd-neighbour-ok-"));
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "ops.md"),
    "# Fixture\n\n| Variable | Default |\n|---|---|\n| `HYDRA_LISTEN` | `1111` |\n",
  );
  fs.writeFileSync(
    path.join(src, "knobs.rs"),
    'fn extra() -> u32 { std::env::var("HYDRA_LISTEN_EXTRA").unwrap_or(9999) }\n' +
      'fn real() -> u32 { std::env::var("HYDRA_LISTEN").unwrap_or(1111) }\n',
  );
  const r = run({ dir, ops: path.join(dir, "ops.md"), src });
  assert("CONTROL: the real default passes", r.status === 0, `status=${r.status} out=${r.out.trim().slice(0, 160)}`);
}

/* Round 136: drift + collapsed coverage in ONE run (the floor used to sit after the drift exit and
 * was therefore never printed exactly when the extraction was broken). */
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cdd-both-"));
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(path.join(dir, "ops.md"), "# F\n\n| Variable | Default |\n|---|---|\n| `HYDRA_LOUD` | `1` |\n");
  fs.writeFileSync(path.join(src, "k.rs"), 'fn f() -> u32 { std::env::var("HYDRA_LOUD").unwrap_or(2) }\n');
  const r = run({ dir, ops: path.join(dir, "ops.md"), src }, 50);
  assert(
    "a drift AND a broken coverage floor are both reported",
    r.status === 1 && /ops\.md says 1/.test(r.out) && /could be compared/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}

/* Round 143: comments and `#[cfg(test)]` items are not the code.
 *
 * Measured on the real tree before the fix: the scan ran on RAW text, so the witness line for
 * `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` was `usage_query.rs:340` — a comment — while the real read is
 * at 343. Worse, a comment that spells the helper call hands the guard a fabricated fallback, and a
 * TEST that passes a different default can "validate" a documented one. */
function blankingFixture({ docDefault, code }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "cdd-blank-"));
  const src = path.join(dir, "src");
  fs.mkdirSync(src, { recursive: true });
  fs.writeFileSync(
    path.join(dir, "ops.md"),
    `# Fixture\n\n| \`HYDRA_TEST_KNOB_0\` | \`${docDefault}\` | A fixture knob. |\n`,
  );
  fs.writeFileSync(path.join(src, "knobs.rs"), code);
  return { dir, ops: path.join(dir, "ops.md"), src };
}

{
  // Line 1 is a COMMENT spelling the helper call with the DOCUMENTED value; line 2 is the real code,
  // which falls back to 9. Before the fix the comment's call was found first and the row "passed"
  // (this is the strongest form of the bug: it fabricates the evidence, not just the line number).
  const fx = blankingFixture({
    docDefault: 7,
    code: '// env_millis("HYDRA_TEST_KNOB_0", 7)\nfn knob_0() -> u64 { std::env::var("HYDRA_TEST_KNOB_0").ok().and_then(|v| v.parse().ok()).unwrap_or(9) }\n',
  });
  const r = run(fx);
  assert(
    "a COMMENT spelling the helper call is not the code's default (it used to pass)",
    r.status === 1 && /falls back to 9/.test(r.out),
    `status=${r.status} out=${r.out.trim().slice(0, 200)}`,
  );
  assert(
    "...and the witness points at the CODE line (2), never the comment (1)",
    /knobs\.rs:2\b/.test(r.out) && !/knobs\.rs:1\b/.test(r.out),
    r.out.trim().slice(0, 200),
  );
}
{
  // CONTROL: the same call, this time in real code (line 1 is code).
  const fx = blankingFixture({
    docDefault: 7,
    code: 'fn knob_0() -> u64 { env_millis("HYDRA_TEST_KNOB_0", 7) }\n',
  });
  const r = run(fx);
  assert("CONTROL: the same helper call in real code is still read", r.status === 0, `status=${r.status}`);
}
{
  // A default that exists only inside `#[cfg(test)]` is not shipped behaviour. The row then has no
  // code side at all, so it must be reported as unverifiable/not-recorded — never compared, never OK.
  const fx = blankingFixture({
    docDefault: 7,
    code: '#[cfg(test)]\nmod tests {\n    fn knob_0() -> u64 { std::env::var("HYDRA_TEST_KNOB_0").unwrap_or(7) }\n}\nfn real() -> u64 { 0 }\n',
  });
  const r = run(fx);
  assert(
    "a default that lives only in `#[cfg(test)]` is NOT evidence (must not pass)",
    r.status === 1,
    `status=${r.status} out=${r.out.trim().slice(0, 200)}`,
  );
}

/* Round 183: the two obligations of a recorded exception live in the shared `recorded_exceptions.cjs`
 * now, and the STALE direction is only enforceable against the SHIPPED table (a fixture's two rows
 * legitimately contain none of the repository's names). This case exercises it the only way it can be
 * exercised: the shipped table plus a record that no longer describes it — a bogus name, which is
 * "recorded but not unverifiable" there. */
{
  let out = "";
  let status = 0;
  try {
    out = execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, CDD_UNVERIFIED_OK: JSON.stringify({ HYDRA_GHOST_RECORD: "no longer applies" }) },
    }).toString();
  } catch (e) {
    status = e.status ?? 1;
    out = (e.stdout ?? "") + (e.stderr ?? "");
  }
  assert(
    "a record that no longer describes the shipped table is reported (never silently kept)",
    status === 1 && /HYDRA_GHOST_RECORD is in UNVERIFIED_OK but that row IS comparable \(or absent\) now/.test(out),
    `status=${status} ${out.trim().slice(0, 200)}`,
  );
}

console.log(failures === 0 ? "\nALL DOCUMENTED-DEFAULT TESTS PASSED" : `\n${failures} TEST(S) FAILED`);
process.exit(failures ? 1 : 0);
