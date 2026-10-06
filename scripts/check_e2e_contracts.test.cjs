#!/usr/bin/env node
/* Tests for scripts/check_e2e_contracts.cjs.
 *
 * The checker is a guard against "the e2e suite silently stops asserting
 * anything", so the checker itself gets the same treatment: each branch must be
 * shown to FAIL on drift. A guard nobody can falsify is the failure mode this
 * whole remediation keeps running into.
 *
 * Run: node scripts/check_e2e_contracts.test.cjs   (CI runs it via `node --test`)
 */
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const ROOT = path.join(__dirname, "..");
const SCRIPT = path.join(__dirname, "check_e2e_contracts.cjs");
const REAL_SPEC = path.join(ROOT, "tests", "e2e", "admin.spec.cjs");

let failures = 0;
function assert(name, cond, detail) {
  if (cond) console.log("PASS  " + name);
  else {
    failures++;
    console.error("FAIL  " + name + (detail ? "  -> " + detail : ""));
  }
}

function run(specPath, env = {}) {
  try {
    // `stdio` is explicit: execFileSync INHERITS stderr by default, which would
    // spray the expected DRIFT output of the failing fixtures into the test log.
    const opts = {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      // Focused fixtures carry almost no selectors; the floor exists for the real
      // suite (see the checker's MIN_TOKENS comment). The floor's OWN test passes
      // an explicit value, so overriding it here cannot silently disable it.
      env: {
        ...process.env,
        E2E_CONTRACTS_MIN_TOKENS: "0",
        // Focused fixtures carry ONE api() call; the floor exists for the real suite and has its
        // OWN case below (which passes an explicit value), so overriding it here cannot disable it.
        E2E_CONTRACTS_MIN_API_SITES: "0",
        ...env,
      },
    };
    return { status: 0, out: execFileSync("node", [SCRIPT, "--spec", specPath], opts).toString() };
  } catch (e) {
    return { status: e.status === undefined ? 1 : e.status, out: (e.stdout || "") + (e.stderr || "") };
  }
}

/** A copy of the real spec with one textual replacement applied. */
function specWith(name, replacements) {
  let src = fs.readFileSync(REAL_SPEC, "utf8");
  for (const [from, to] of replacements) {
    if (!src.includes(from)) throw new Error(`fixture: "${from}" not found in admin.spec.cjs`);
    src = src.split(from).join(to);
  }
  const p = path.join(os.tmpdir(), `e2e_contracts_${name}.spec.cjs`);
  fs.writeFileSync(p, src);
  return p;
}

/* 1. The real suite is clean (this is the state CI asserts). */
{
  const r = run(REAL_SPEC);
  assert("the real spec passes", r.status === 0, "status=" + r.status + " out=" + r.out.trim().slice(0, 160));
  assert("it reports how many tokens it examined", /\(\d+ selector\/nav tokens/.test(r.out), r.out.trim().slice(0, 120));
}

/* 2-5. One drifted token per branch: id, class, data-field, nav key. */
const drift = [
  ["an id", "toast-root", "toast-rooot"],
  ["a class", "modal-overlay", "modal-overlai"],
  ["a data-field", 'data-field="max_concurrency"', 'data-field="max_concurreny"'],
  ["a nav key", "navItem(page, 'health')", "navItem(page, 'healt')"],
];
for (const [what, from, to] of drift) {
  const p = specWith(what.replace(/\s+/g, "_"), [[from, to]]);
  const r = run(p);
  assert(`drift in ${what} exits non-zero`, r.status !== 0, "status=" + r.status);
  assert(`drift in ${what} is reported`, /DRIFT/.test(r.out), r.out.trim().slice(0, 140));
}

/* 6. The vacuous-pass floor: a spec with nothing to check must NOT report OK.
 *    (A checker that extracts zero tokens and prints OK is exactly the kind of
 *    "green that proves nothing" this repository has been bitten by.) */
{
  const p = path.join(os.tmpdir(), "e2e_contracts_empty.spec.cjs");
  fs.writeFileSync(p, 'const { test } = require("@playwright/test");\ntest("x", async () => {});\n');
  const r = run(p, { E2E_CONTRACTS_MIN_TOKENS: "40" });
  assert("a spec with no selectors is not reported OK", r.status !== 0 && /token\(s\) examined/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 140));
}

/* 7. An API-based fixture must send the timestamp fields its entity requires.
 *    Two cases in this session omitted them and were RED the first time the suite
 *    was ever run locally (the handler deserialises straight into the struct, so
 *    a missing non-Option field is a 400 `invalid_json`). A selector-only check
 *    cannot see this. */
{
  const dir = path.join(os.tmpdir(), "e2e_contracts_ts");
  fs.mkdirSync(dir, { recursive: true });
  const spec = (body) => `const { test } = require("@playwright/test");\ntest("x", async () => {\n  const r = await api('POST', '/providers', {\n    body: {\n${body}    },\n  });\n});\n`;
  const good = path.join(dir, "good.spec.cjs");
  const bad = path.join(dir, "bad.spec.cjs");
  fs.writeFileSync(good, spec("      id: '', created_at: '', updated_at: '', key: 'k',\n"));
  fs.writeFileSync(bad, spec("      id: '', key: 'k',\n"));

  const okRun = run(good);
  assert("a body with timestamps passes", okRun.status === 0, "status=" + okRun.status + " out=" + okRun.out.trim().slice(0, 160));
  const badRun = run(bad);
  assert("a body without timestamps fails", badRun.status !== 0, "status=" + badRun.status);
  assert(
    "...and both missing fields are named",
    /omits `created_at`/.test(badRun.out) && /omits `updated_at`/.test(badRun.out),
    badRun.out.trim().slice(0, 200),
  );
}

/* 8. ...but a resource whose struct has NO timestamps is not required to send
 *    them (ProviderModel), so the rule must not fire there. */
{
  const dir = path.join(os.tmpdir(), "e2e_contracts_no_ts");
  fs.mkdirSync(dir, { recursive: true });
  const p = path.join(dir, "models.spec.cjs");
  fs.writeFileSync(p, `const { test } = require("@playwright/test");\ntest("x", async () => {\n  await api('POST', '/provider-models', {\n    body: { id: '', key: 'gpt-4', name: 'gpt-4', provider_id: 'p1', status: 1 },\n  });\n});\n`);
  const r = run(p);
  assert("a struct without timestamps needs none", r.status === 0, "status=" + r.status + " out=" + r.out.trim().slice(0, 200));
}

/* The name-kill rule (2026-09-30): `pkill -x hydra` in a suite ALSO killed the user's
 * local dev stack (three Docker containers running a process literally named `hydra`;
 * RestartCount=14, all unhealthy). Each executable shape must be caught, and prose that
 * merely explains the old command must NOT be. */
{
  const { nameKillLines } = require(SCRIPT);
  const hits = (src) => nameKillLines(src).length;
  assert(
    "name-kill: an argv-list pkill is caught",
    hits('    subprocess.run(["pkill", "-x", "hydra"], capture_output=True)\n') === 1,
  );
  assert(
    "name-kill: a shell-string pkill is caught",
    hits('    subprocess.run("pkill -x hydra", shell=True)\n') === 1,
  );
  assert("name-kill: a bare shell command is caught", hits("pkill -9 hydra\n") === 1);
  assert("name-kill: `sudo killall hydra` is caught", hits("sudo killall hydra\n") === 1);
  assert("name-kill: a command after `;` is caught", hits("touch x; pkill -x hydra\n") === 1);
  assert(
    "name-kill: prose in a docstring is NOT a violation",
    hits('    `pkill -x hydra` (the first version) killed the local dev stack too\n') === 0,
  );
  assert(
    "name-kill: a comment is NOT a violation",
    hits("    # never `pkill -x hydra` here\n") === 0,
  );
  assert(
    "name-kill: `pkill -f \"$BIN\"` (a PATH, not a name) is NOT a violation",
    hits('cleanup() { pkill -f "$BIN" 2>/dev/null; }\n') === 0,
  );
  assert(
    "name-kill: killing a different, unrelated name is NOT a violation",
    hits('subprocess.run(["pkill", "-x", "my-test-server"])\n') === 0,
  );
}

/* Round 122: the widened timestamp rule and its floors. */
{
  // The escape shape that used to be invisible: the payload is a same-file `const`, so the guard
  // must follow it (one hop) and still see the missing timestamps. Measured before the fix: this
  // fixture exited 0.
  const spec = specWith("var_payload", [[
    `    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',`,
    `    const payload = { id: '', name: 'probe' };
    const created = await api('POST', '/providers', {
      body: payload,
      ignored: {
        created_at: '',`,
  ]]);
  const r = run(spec);
  assert(
    "a payload built in a same-file const is still checked (the escape shape)",
    r.status === 1 && /omits `created_at`/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  // A payload this guard cannot read must be REPORTED, not skipped: the shape is the hole.
  const spec = specWith("call_payload", [[
    `    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',`,
    `    const created = await api('POST', '/providers', { body: makeProvider() });
    const ignored = {
        id: '',
        created_at: '',`,
  ]]);
  const r = run(spec);
  assert(
    "an unreadable payload is reported as unverifiable (never silently skipped)",
    r.status === 1 && /not an object literal in this file/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  // CONTROL for both: the same const WITH the timestamps passes.
  const spec = specWith("var_payload_ok", [[
    `    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',`,
    `    const payload = { id: '', created_at: '', updated_at: '', name: 'probe' };
    const created = await api('POST', '/providers', { body: payload });
    const ignored = {
        id: '',
        created_at: '',`,
  ]]);
  const r = run(spec);
  assert(
    "CONTROL: the same shape WITH the timestamps passes",
    r.status === 0,
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  // The api-call floor itself, with an explicit value.
  const spec = specWith("floor_probe", []);
  const r = run(spec, { E2E_CONTRACTS_MIN_API_SITES: "500" });
  assert(
    "the api-call floor turns a tiny scan into a failure (never a silent pass)",
    r.status === 1 && /api\('POST'\|'PUT'/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  // The harness-file floor for the pkill walk, with an explicit value.
  const spec = specWith("harness_floor_probe", []);
  const r = run(spec, { E2E_CONTRACTS_MIN_HARNESS_FILES: "500" });
  assert(
    "the harness-file floor catches a walk that scanned almost nothing",
    r.status === 1 && /harness file\(s\) scanned/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}

/* Round 123: the payload check must read CODE, not message strings — the same lesson the Rust
 *     guards learned with `'{'` in a char literal. Measured before this fix: the fixture below
 *     (both timestamps present ONLY inside a string, which also contains a `}`) exited 0. */
{
  const spec = specWith("stringy_payload", [[
    `    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',`,
    // NOTE: no real `created_at:` line may follow — an earlier version of this fixture appended
    // one inside `ignored`, which made the case pass for the right reason but test nothing.
    `    const created = await api('POST', '/providers', {
      body: {
        id: '',
        note: 'created_at: never sent, updated_at: never sent }',
        ignored: {`,
  ]]);
  const r = run(spec);
  assert(
    "a field name appearing only inside a STRING does not satisfy the timestamp rule",
    r.status === 1 && /omits `created_at`/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  // …and the mirrored risk of the fix: a COMMENTED-OUT call is not a call. Matching on the raw
  // text would have reported "could not read the request options object" for it.
  const spec = specWith("commented_call", [[
    `    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',`,
    `    // await api('POST', '/providers', { body: { id: '' } });
    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',`,
  ]]);
  const r = run(spec);
  assert(
    "a commented-out api() call is not treated as a call",
    r.status === 0,
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}

/* Round 127: an unterminated literal must not silently empty this rule.
 *
 * Measured before the fix: one stray backtick made the masker consume the rest of the file, so
 * every later `api()` call vanished and the timestamp rule never ran — the run stayed non-zero only
 * because the unrelated `MIN_API_SITES` floor happened to catch it. A stray single quote moved the
 * scan's idea of where code is in the same way. */
{
  const spec = specWith("unterminated_template", [[
    'async function api(',
    'const NOTE = `see tests/e2e/README.md;\nasync function api(',
  ]]);
  const r = run(spec);
  assert(
    "an unterminated template literal is reported (the file cannot be parsed)",
    r.status === 1 && /unterminated string or template literal/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 170)}`,
  );
}
{
  // A stray single quote BEFORE a real violation: the violation must still be found by the rule
  // itself (not by a floor), at the right line.
  const spec = specWith("stray_quote", [[
    `    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',
        updated_at: '',`,
    `    const stray = 'unterminated;
    const created = await api('POST', '/providers', {
      body: {
        id: '',
        created_at: '',
        removed_updated_at: '',`,
  ]]);
  const r = run(spec);
  assert(
    "a stray quote does not hide a later payload violation",
    r.status === 1 && /omits `updated_at`/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 170)}`,
  );
}

/* Round 129: the token floor used to `process.exit(1)` before the name-kill scan and its own
 *      `MIN_HARNESS_FILES` floor, making both unreachable in exactly the broken-extraction case. */
{
  const spec = specWith("floor_reachability", []);
  const r = run(spec, { E2E_CONTRACTS_MIN_TOKENS: "99999", E2E_CONTRACTS_MIN_HARNESS_FILES: "99999" });
  assert(
    "a tripped token floor does not hide the harness scan or its floor",
    r.status === 1 && /token\(s\) examined/.test(r.out) && /harness file\(s\) scanned/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}

/* Round 178: a REGEX LITERAL may contain a backtick — ``/`[^`]*`/`` is valid JS — and the shared
 * masker used to treat that backtick as the start of a TEMPLATE literal that never closed (round 176:
 * 74% of `scripts/check_redis_pool.test.cjs` was blanked and its `process.exit(` became invisible).
 * For THIS guard the consequence is worse than a wrong count: the file is reported as "cannot be
 * parsed as JS" and every `api()` check in it is skipped, so the rule goes silent over a spec it
 * cannot read. Both halves are pinned here. */
{
  const dir = path.join(os.tmpdir(), "e2e_contracts_regex_backtick");
  fs.mkdirSync(dir, { recursive: true });
  const spec = path.join(dir, "regex_backtick.spec.cjs");
  fs.writeFileSync(spec, [
    'const { test } = require("@playwright/test");',
    '// A regex containing a backtick; the api() call below must stay visible to the checker.',
    'const CODE_RE = /`[^`]*`/;',
    'test("x", async () => {',
    "  await api('POST', '/providers', {",
    '    body: {',
    "      id: '', key: 'k',",
    '    },',
    '  });',
    '});',
    '',
  ].join('\n'));
  const r = run(spec);
  assert(
    "a regex containing a backtick is NOT reported as an unterminated template",
    !/unterminated string or template literal/.test(r.out),
    r.out.trim().slice(0, 200),
  );
  assert(
    "...and the api() payload check still runs on that file (the missing timestamps are caught)",
    r.status !== 0 && /omits `created_at`/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}

/* Round 186: two preconditions about the UI SOURCE LIST itself. A listed-but-missing file made every
 * selector check read an EMPTY source (dozens of false drifts instead of the one true fact), and an
 * unlisted source file made a token declared only there read as missing. The root is overridable
 * (`E2E_CONTRACTS_ROOT`) so both directions can be driven against a fixture tree. */
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "e2e-contracts-ui-"));
  const ui = path.join(dir, "admin-ui");
  fs.mkdirSync(ui, { recursive: true });
  for (const f of ["app.js", "stats.js", "api-docs.js", "index.html", "style.css", "i18n.js"]) {
    fs.writeFileSync(path.join(ui, f), "// fixture\n");
  }
  fs.writeFileSync(path.join(ui, "extra-widget.js"), "// a new UI source the guard does not read\n");
  const r = run(REAL_SPEC, { E2E_CONTRACTS_ROOT: dir, E2E_CONTRACTS_MIN_HARNESS_FILES: "0" });
  assert(
    "a UI source that is neither listed nor recorded is reported",
    r.status === 1 && /admin-ui\/extra-widget\.js is a UI source under admin-ui\/ but is NOT in UI_FILES/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "e2e-contracts-ui-ok-"));
  const ui = path.join(dir, "admin-ui");
  fs.mkdirSync(ui, { recursive: true });
  for (const f of ["app.js", "stats.js", "api-docs.js", "index.html", "style.css", "i18n.js"]) {
    fs.writeFileSync(path.join(ui, f), "// fixture\n");
  }
  fs.writeFileSync(path.join(ui, "extra-widget.js"), "// recorded below\n");
  const r = run(REAL_SPEC, {
    E2E_CONTRACTS_ROOT: dir,
    E2E_CONTRACTS_MIN_HARNESS_FILES: "0",
    E2E_CONTRACTS_UNSCANNED_UI: JSON.stringify({ "admin-ui/extra-widget.js": "a vendored widget the guard does not parse" }),
  });
  assert(
    "CONTROL: the same file RECORDED passes (and the record is not reported stale)",
    !/extra-widget\.js is a UI source/.test(r.out) && !/UNSCANNED_UI_OK records/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "e2e-contracts-ui-missing-"));
  const ui = path.join(dir, "admin-ui");
  fs.mkdirSync(ui, { recursive: true });
  // only five of the six listed files exist
  for (const f of ["app.js", "stats.js", "api-docs.js", "index.html", "style.css"]) {
    fs.writeFileSync(path.join(ui, f), "// fixture\n");
  }
  const r = run(REAL_SPEC, { E2E_CONTRACTS_ROOT: dir, E2E_CONTRACTS_MIN_HARNESS_FILES: "0" });
  assert(
    "a LISTED UI file that is missing is reported as the one fact that matters",
    r.status === 1 && /admin-ui\/i18n\.js is listed in UI_FILES but does not exist/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}
{
  // A record that no longer applies: the file IS listed now.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "e2e-contracts-ui-stale-"));
  const ui = path.join(dir, "admin-ui");
  fs.mkdirSync(ui, { recursive: true });
  for (const f of ["app.js", "stats.js", "api-docs.js", "index.html", "style.css", "i18n.js"]) {
    fs.writeFileSync(path.join(ui, f), "// fixture\n");
  }
  const r = run(REAL_SPEC, {
    E2E_CONTRACTS_ROOT: dir,
    E2E_CONTRACTS_MIN_HARNESS_FILES: "0",
    E2E_CONTRACTS_UNSCANNED_UI: JSON.stringify({ "admin-ui/app.js": "was unlisted once" }),
  });
  assert(
    "...and a RECORD that no longer applies is reported as stale",
    r.status === 1 && /UNSCANNED_UI_OK records admin-ui\/app\.js/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}

/* Round 190: RECORDED DECISION — the guard does NOT check that a literal `toContainText('…')` value
 * exists in the UI or seed sources. Measured: 12 such literals across the three specs, 4 of which the
 * spec itself creates at runtime (`node-b`, `pw-upstream.example.com`, `auth.pw.example.com`), so the
 * naive rule would report false drift on a correct suite. A dead helper was written for it and never
 * called; it is deleted, and this case pins the decision — re-adding the rule reddens it first. */
{
  const dir = path.join(os.tmpdir(), "e2e_contracts_runtime_literal");
  fs.mkdirSync(dir, { recursive: true });
  const spec = path.join(dir, "runtime_literal.spec.cjs");
  fs.writeFileSync(spec, [
    'const { test } = require("@playwright/test");',
    'test("x", async () => {',
    '  await expect(page.locator("#toast-root")).toContainText("node-b-created-at-runtime");',
    '});',
    '',
  ].join("\n"));
  const r = run(spec);
  assert(
    "a runtime-created asserted value is NOT drift (the naive literal rule is deliberately absent)",
    r.status === 0,
    `status=${r.status} ${r.out.trim().slice(0, 200)}`,
  );
}

console.log(failures === 0 ? "\nALL E2E CONTRACT TESTS PASSED" : "\n" + failures + " E2E CONTRACT TEST(S) FAILED");
process.exit(failures ? 1 : 0);
