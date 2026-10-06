#!/usr/bin/env node
/* Tests for scripts/check_tenant_error_codes.cjs.
 *
 * The checker guards the EXTERNAL tenant contract (error code -> HTTP status), so it must be
 * shown to fail in both directions — the table moving and the code moving — and to refuse a
 * pass when it could compare almost nothing. Each of the four emission shapes this tree uses
 * gets a fixture, because a shape the extractor stops recognising would silently shrink the
 * coverage instead of failing.
 *
 * Run: node --test scripts/check_tenant_error_codes.test.cjs
 */
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const SCRIPT = path.join(__dirname, "check_tenant_error_codes.cjs");

let failures = 0;
function assert(name, cond, detail) {
  if (cond) console.log("PASS  " + name);
  else {
    failures++;
    console.error("FAIL  " + name + (detail ? "  -> " + detail : ""));
  }
}

function doc(rows) {
  return `# Fixture\n\n## 6. 错误码总表\n\n| code | HTTP | 含义 | 可重试 |\n|---|---|---|---|\n${rows.join("\n")}\n\n## 7. Next\n`;
}

function run({ rows, src, min = 1 }) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tec-"));
  fs.writeFileSync(path.join(dir, "doc.md"), doc(rows));
  const srcDir = path.join(dir, "src");
  fs.mkdirSync(srcDir, { recursive: true });
  fs.writeFileSync(path.join(srcDir, "handlers.rs"), src);
  try {
    const out = execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, TEC_DOC: path.join(dir, "doc.md"), TEC_SRC: srcDir, TEC_MIN_COMPARED: String(min) },
    });
    return { status: 0, out };
  } catch (e) {
    return { status: e.status ?? 1, out: (e.stdout ?? "") + (e.stderr ?? "") };
  }
}

const ROW = (code, status) => `| \`${code}\` | ${status} | meaning | no |`;

// ---- shape 1: respond_error(status, "code") ---------------------------------
{
  const r = run({
    rows: [ROW("invalid_wait", 400)],
    src: `async fn h() { return respond_error(session, ctx, 400, "invalid_wait", "bad wait").await; }`,
  });
  assert("respond_error shape: a matching pair passes", r.status === 0, `status=${r.status} ${r.out.trim().slice(0, 140)}`);
}
{
  const r = run({
    rows: [ROW("invalid_wait", 429)],
    src: `async fn h() { return respond_error(session, ctx, 400, "invalid_wait", "bad wait").await; }`,
  });
  assert("a table/code mismatch FAILS", r.status === 1, `status=${r.status}`);
  assert("...and names both the table and the emitted status", /table says HTTP 429/.test(r.out) && /emits it with 400/.test(r.out), r.out.trim().slice(0, 200));
}
{
  const r = run({
    rows: [ROW("invalid_wait", 400)],
    src: `async fn h() { return respond_error(session, ctx, 500, "invalid_wait", "bad wait").await; }`,
  });
  assert("the CODE moving is caught too", r.status === 1 && /emits it with 500/.test(r.out), `status=${r.status}`);
}

// ---- shape 2: err_json(status, "code") --------------------------------------
{
  const r = run({
    rows: [ROW("too_many_requests", 429)],
    src: `fn t() { Some(err_json(429, "too_many_requests", "over budget", trace_id)) }`,
  });
  assert("err_json shape is recognised", r.status === 0, `status=${r.status} ${r.out.trim().slice(0, 140)}`);
}

// ---- shape 3: the status travels in a tuple next to a shared body builder ----
{
  const r = run({
    rows: [ROW("payload_too_large", 413)],
    src: `fn b() { return Err((413, error_body("payload_too_large", "too big", trace_id))); }`,
  });
  assert("tuple + error_body shape is recognised", r.status === 0, `status=${r.status} ${r.out.trim().slice(0, 140)}`);
}

// ---- shape 4: a mapping enum whose status comes from the caller --------------
{
  const r = run({
    rows: [ROW("invalid_since", 400)],
    src: [
      `impl BoundError { pub const fn code(&self) -> &'static str { match self { Self::InvalidSince => "invalid_since" } } }`,
      `async fn h(e: BoundError) { return respond_error(session, ctx, 400, e.code(), &e.message()).await; }`,
    ].join("\n"),
  });
  assert("mapping-function shape is recognised", r.status === 0, `status=${r.status} ${r.out.trim().slice(0, 140)}`);
}

// ---- coverage floor and missing table ---------------------------------------
{
  const r = run({
    rows: [ROW("invalid_wait", 400)],
    src: `fn h() { return respond_error(session, ctx, 400, "invalid_wait", "x"); }`,
    min: 20,
  });
  assert("too little coverage is CANNOT VERIFY (exit 2), never a pass", r.status === 2 && /could be compared/.test(r.out), `status=${r.status}`);
}
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tec-none-"));
  fs.writeFileSync(path.join(dir, "doc.md"), "# Fixture without the error table\n");
  try {
    execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, TEC_DOC: path.join(dir, "doc.md"), TEC_SRC: dir, TEC_MIN_COMPARED: "1" },
    });
    assert("a missing §6 table exits non-zero (2), never 0", false, "it exited 0");
  } catch (e) {
    assert("a missing §6 table exits non-zero (2), never 0", e.status === 2, `status=${e.status}`);
  }
}

/* Round 136: a drift and a collapsed comparison must be reported in the SAME run. The coverage floor
 * used to sit after the `problems` exit, so precisely when the extraction was broken the operator saw
 * only the drift — never the "this check proves nothing" line. */
{
  const r = run({
    rows: [ROW("invalid_wait", 429)],
    src: `async fn h() { return respond_error(session, ctx, 400, "invalid_wait", "bad wait").await; }`,
    min: 50,
  });
  // Round 157: the floor keeps the STRONGER exit code (2) while BOTH findings are still printed in
  // the same run — "this check proves nothing" outranks "a status drifted".
  assert("a drift AND a broken coverage floor are both reported (exit 2)", r.status === 2, `status=${r.status}`);
  assert("...the drift is named", /table says HTTP 429/.test(r.out), r.out.trim().slice(0, 160));
  assert("...and so is the floor, with its own exit code (2)", r.status === 2 && /could be compared/.test(r.out), `status=${r.status} ${r.out.trim().slice(0, 160)}`);
}

/* Round 137: the extraction ran on RAW file text, so a documented code counted as "emitted" if the
 * only occurrence was a COMMENT or a unit test. Measured: both shapes printed
 * `OK unknown_path = 404 (table says 404)` and exited 0 while the product never emits it.
 * The sources are now blanked with `rust_blank.cjs` (comments + whole `#[cfg(test)]` items; string
 * CONTENTS kept, because the extraction matches `"code_name"` literals). */
{
  const r = run({
    rows: [ROW("unknown_path", 404)],
    src: '// historical: respond_error(session, ctx, 404, "unknown_path", "x").await;\nfn unrelated() {}\n',
  });
  assert("a code emitted only in a COMMENT is not accepted (exit 2: nothing comparable)", r.status === 2 && /is NOT recorded in UNVERIFIED_OK/.test(r.out), `status=${r.status} ${r.out.trim().slice(0, 160)}`);
}
{
  const r = run({
    rows: [ROW("unknown_path", 404)],
    src: '#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { respond_error(session, ctx, 404, "unknown_path", "x"); }\n}\n',
  });
  assert("a code emitted only inside #[cfg(test)] is not accepted (exit 2: nothing comparable)", r.status === 2 && /is NOT recorded in UNVERIFIED_OK/.test(r.out), `status=${r.status} ${r.out.trim().slice(0, 160)}`);
}
{
  // CONTROL: the same row with a REAL emission site passes — so the two cases above fail because of
  // the comment/test, not because the fixture stopped being comparable at all.
  const r = run({
    rows: [ROW("unknown_path", 404)],
    src: 'async fn h() { return respond_error(session, ctx, 404, "unknown_path", "x").await; }\n',
  });
  assert("CONTROL: a real emission site passes", r.status === 0, `status=${r.status} ${r.out.trim().slice(0, 160)}`);
}

/* Round 140: the "helper call on the PREVIOUS line" shape (this file's shape 5) called
 * `functionBody`, which was NOT defined in this file — so any hit threw `ReferenceError` and the
 * process died with exit 1, the code the header defines as "a mismatch". A crash is not a verdict:
 * the shape must fall through to the honest "unverifiable" path. */
{
  const r = run({
    rows: [ROW("rate_limited", 429)],
    src: 'fn h() { ResponseHeader::build(429, Some(3))?; }\nasync fn p() {\n    return short_circuit_rate_limited(\n        "rate_limited",\n    ).await;\n}\n',
  });
  assert(
    "the helper-call shape does not crash the guard (no ReferenceError; the row is not given a verdict)",
    !/ReferenceError|is not defined/.test(r.out) && r.status !== 1,
    `status=${r.status} out=${r.out.trim().slice(0, 200)}`,
  );
  assert(
    "...and the row is reported honestly instead of being given a verdict",
    /rate_limited/.test(r.out),
    r.out.trim().slice(0, 200),
  );
}

/* Round 146: ONE CODE NAME, TWO DIFFERENT STATUSES at different sites.
 *
 * The comparison used to be `statuses.includes(docStatus)`, so a code emitted as 404 in one handler
 * and 500 in another PASSED as long as the table named one of them — and the OK line printed
 * `404/500 (table says 404)` while saying OK. Measured on the real tree: 0 codes are in this state,
 * so the rule is a LATENT hole, not a present misreport. */
{
  const r = run({
    rows: [ROW("not_found", 404)],
    src: 'async fn a() { return respond_error(session, ctx, 404, "not_found", "a").await; }\n'
      + 'async fn b() { return respond_error(session, ctx, 500, "not_found", "b").await; }\n',
  });
  assert(
    "the same code emitted with two DIFFERENT statuses FAILS (it used to pass)",
    r.status === 1 && /DIFFERENT statuses \(404\/500\)/.test(r.out),
    `status=${r.status} ${r.out.trim().split("\n")[0].slice(0, 160)}`,
  );
}
{
  // CONTROL: both sites agree with the table ⇒ pass (the rule must not fire on agreement).
  const r = run({
    rows: [ROW("not_found", 404)],
    src: 'async fn a() { return respond_error(session, ctx, 404, "not_found", "a").await; }\n'
      + 'async fn b() { return respond_error(session, ctx, 404, "not_found", "b").await; }\n',
  });
  assert(
    "CONTROL: two sites emitting the SAME status still pass",
    r.status === 0 && /OK    not_found = 404 \(table says 404\)/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}

/* Round 147: the short-circuit helper call SPANS LINES — the real shape at `proxy.rs:859`/`:885`:
 *
 *     return short_circuit_rate_limited(
 *         session,
 *         "rate_limited",
 *     )
 *
 * Neither the literal's line nor the line above it ends with `call(`, so the helper was never found:
 * `rate_limited`'s 429 came ONLY from the tenant-API subsystem, and changing the proxy's `429` to
 * `418` left this guard green (the row was merely `????`). Verified against a copy of the REAL
 * `crates/hydra-server/src/proxy.rs`: unmodified ⇒ `OK rate_limited = 429` exit 0; helper `429`→`418`
 * ⇒ DRIFT exit 1. */
const MULTILINE = (helperStatus) =>
  'async fn short_circuit_rate_limited(session: &mut Session, reason: &str) -> Result<()> {\n'
  + `    let mut resp_header = ResponseHeader::build(${helperStatus}, Some(3))?;\n`
  + '    Ok(())\n'
  + '}\n'
  + 'async fn h(session: &mut Session) -> Result<()> {\n'
  + '    return short_circuit_rate_limited(\n'
  + '        session,\n'
  + '        "rate_limited",\n'
  + '        None,\n'
  + '    )\n'
  + '    .await;\n'
  + '}\n';
{
  const r = run({ rows: [ROW("rate_limited", 429)], src: MULTILINE(418) });
  assert(
    "a MULTI-LINE helper call is followed into its body (418 vs the table's 429 is DRIFT)",
    r.status === 1 && /emits it with 418/.test(r.out),
    `status=${r.status} ${r.out.trim().split("\n")[0].slice(0, 160)}`,
  );
}
{
  // CONTROL: the same multi-line call with the documented status passes — so the case above fails
  // because of the status, not because the walk broke the extraction.
  const r = run({ rows: [ROW("rate_limited", 429)], src: MULTILINE(429) });
  assert(
    "CONTROL: the same multi-line call with 429 passes",
    r.status === 0 && /OK    rate_limited = 429 \(table says 429\)/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 160)}`,
  );
}
{
  // The walk must NOT attribute a COMPLETED call above to this literal: here the literal belongs to
  // `other_helper(` and the helper above it (which builds 418) is already finished by a `;`.
  const src = 'async fn other_helper(session: &mut Session) -> Result<()> {\n'
    + '    let mut resp_header = ResponseHeader::build(429, Some(3))?;\n'
    + '    Ok(())\n'
    + '}\n'
    + 'async fn short_circuit_rate_limited(session: &mut Session, r: &str) -> Result<()> {\n'
    + '    let mut resp_header = ResponseHeader::build(418, Some(3))?;\n'
    + '    Ok(())\n'
    + '}\n'
    + 'async fn h(session: &mut Session) -> Result<()> {\n'
    + '    let done = other_helper(session);\n'
    + '    return short_circuit_rate_limited(\n'
    + '        session,\n'
    + '        "rate_limited",\n'
    + '    )\n'
    + '    .await;\n'
    + '}\n';
  const r = run({ rows: [ROW("rate_limited", 429)], src });
  assert(
    "the walk stops at a statement boundary (it reads THIS call, not a completed one above)",
    r.status === 1 && /emits it with 418/.test(r.out),
    `status=${r.status} ${r.out.trim().split("\n")[0].slice(0, 160)}`,
  );
}

/* Round 147 (shape 6): the status must come from the call that SENDS this body, not from a character
 * window. The old `slice(idx - 1500, idx + 1500)` took the FIRST `respond_json*` in the window, so a
 * NEIGHBOUR handler's call answered for this literal — measured with two handlers in one file: the
 * guard reported "the code emits it with 418" for a literal whose own call passes 404 (a FALSE DRIFT
 * that would have an operator "fix" a correct file). Real shapes in
 * `crates/hydra-server/src/tenant_api/`: the status is the first 3-digit argument and the body is
 * either inline (A) or a variable built above (B). */
{
  const src = 'async fn neighbour(session: &mut Session) -> Response {\n'
    + '    respond_json(session, 418, serde_json::json!({"error": {"message": "tea", "code": "neighbour_code"}}))\n'
    + '}\n'
    + 'async fn mine(session: &mut Session) -> Response {\n'
    + '    respond_json(session, 404, serde_json::json!({"error": {"message": "gone", "code": "not_found"}}))\n'
    + '}\n';
  const r = run({ rows: [ROW("not_found", 404)], src });
  assert(
    "a NEIGHBOUR handler's status is not attributed to this literal (it used to report 418)",
    r.status === 0 && /OK    not_found = 404 \(table says 404\)/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 170)}`,
  );
}
{
  // shape B: the body is a variable and the sending call comes AFTER the literal.
  const ok = run({
    rows: [ROW("not_found", 404)],
    src: 'async fn h(session: &mut Session, ctx: &Ctx) -> Response {\n'
      + '    let body = serde_json::json!({"error": {"message": "gone", "code": "not_found"}});\n'
      + '    respond_json(session, ctx, 404, &body).await\n'
      + '}\n',
  });
  assert(
    "shape B (body in a variable, send below) is still followed",
    ok.status === 0 && /OK    not_found = 404/.test(ok.out),
    `status=${ok.status} ${ok.out.trim().slice(0, 170)}`,
  );
  const drift = run({
    rows: [ROW("not_found", 404)],
    src: 'async fn h(session: &mut Session, ctx: &Ctx) -> Response {\n'
      + '    let body = serde_json::json!({"error": {"message": "gone", "code": "not_found"}});\n'
      + '    respond_json(session, ctx, 418, &body).await\n'
      + '}\n',
  });
  assert(
    "...and a drifted status in shape B is caught",
    drift.status === 1 && /emits it with 418/.test(drift.out),
    `status=${drift.status} ${drift.out.trim().split("\n")[0].slice(0, 170)}`,
  );
}
{
  // The attribution must not leave the enclosing function, so a neighbour BELOW cannot answer either.
  const src = 'async fn mine(session: &mut Session, ctx: &Ctx) -> Response {\n'
    + '    let body = serde_json::json!({"error": {"message": "gone", "code": "not_found"}});\n'
    + '    respond_json(session, ctx, 404, &body).await\n'
    + '}\n'
    + 'async fn neighbour(session: &mut Session, ctx: &Ctx) -> Response {\n'
    + '    let body2 = serde_json::json!({"error": {"message": "tea", "code": "other"}});\n'
    + '    respond_json(session, ctx, 418, &body2).await\n'
    + '}\n';
  const r = run({ rows: [ROW("not_found", 404)], src });
  assert(
    "the attribution stops at the function boundary (a neighbour BELOW cannot answer either)",
    r.status === 0 && /OK    not_found = 404/.test(r.out),
    `status=${r.status} ${r.out.trim().slice(0, 170)}`,
  );
}

/* Round 150: a row the extraction cannot read is a RECORDED decision, not a silent `????`.
 *
 * Before this, the three unreadable rows (`invalid_since`/`invalid_until`/`window_too_large`, whose
 * status lives at a call site in ANOTHER file) printed `????` and the guard exited 0 — so a new code
 * emitted in a shape this guard cannot read, or a changed documented status for a recorded row after
 * its record went stale, both passed unnoticed. */
{
  const r = run({
    rows: [ROW("brand_new_code", 418)],
    src: 'async fn h() -> u8 { 1 }\n',
  });
  assert(
    "an unrecorded unverifiable row FAILS (it used to print ???? and exit 0)",
    r.status === 2 && /is NOT recorded in UNVERIFIED_OK/.test(r.out),
    `status=${r.status} ${r.out.trim().split("\n")[0].slice(0, 170)}`,
  );
}
{
  // The staleness path needs the SHIPPED table (a record is only judged against the real document):
  // a source tree that DOES extract a recorded row must make the record a finding.
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tec-stale-"));
  const srcDir = path.join(dir, "src");
  fs.mkdirSync(srcDir, { recursive: true });
  fs.writeFileSync(
    path.join(srcDir, "h.rs"),
    'async fn h(session: &mut Session, ctx: &Ctx) -> Response {\n'
    + '    respond_error(session, ctx, 400, "invalid_since", "bad")\n'
    + '}\n',
  );
  try {
    execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      // NOTE: no TEC_DOC — the shipped table is what makes the record judgeable.
      env: { ...process.env, TEC_SRC: srcDir, TEC_MIN_COMPARED: "0" },
    });
    assert("a stale UNVERIFIED_OK record exits non-zero, never 0", false, "it exited 0");
  } catch (e) {
    const out = (e.stdout ?? "") + (e.stderr ?? "");
    assert(
      "a stale UNVERIFIED_OK record is a finding (the row can be compared now)",
      e.status === 1 && /UNVERIFIED_OK entry for `invalid_since` is no longer needed/.test(out),
      `status=${e.status} ${out.trim().split("\n")[0].slice(0, 170)}`,
    );
  }
}

/* Round 155: a blanket `fn status(&self) -> u16` match in the SAME FILE used to add every 3-digit
 * number in it to this code. Measured on a file whose only emission is `respond_error(…, 400,
 * e.code(), …)` plus an unrelated `fn status … { 500 }`: FALSE DRIFT "emitted with DIFFERENT statuses
 * (500/400)" for a file that only ever sends 400. */
{
  const r = run({
    rows: [ROW("quota_exceeded", 400)],
    src: 'impl QuotaError {\n'
      + '    pub fn status(&self) -> u16 { 500 }\n'
      + '    pub const fn code(&self) -> &\'static str { match self { Self::Exceeded => "quota_exceeded", } }\n'
      + '}\n'
      + 'async fn h(e: QuotaError) -> Response { respond_error(session, ctx, 400, e.code(), "over quota").await }\n',
  });
  assert(
    "an unrelated `fn status(&self) -> u16` does not inject its value into this code",
    r.status === 0 && /OK    quota_exceeded = 400/.test(r.out),
    `status=${r.status} ${r.out.trim().split("\n")[0].slice(0, 170)}`,
  );
}

/* Round 157: three ways this guard could give the WRONG answer about its own inputs.
 *  1. an unreadable source tree threw an uncaught ENOTDIR and the process died with a stack trace,
 *     exit 1 — the code the header defines as "a mismatch";
 *  2. an EMPTY §6 table printed `OK (0 documented error code(s) …)` with exit 0 once
 *     `TEC_MIN_COMPARED=0` switched the only floor off;
 *  3. a broken floor was counted and worded as a "documented-vs-emitted status mismatch" when zero
 *     statuses had mismatched. */
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tec-srcfile-"));
  const srcFile = path.join(dir, "h.rs");
  fs.writeFileSync(srcFile, 'async fn h() { respond_error(session, ctx, 400, "invalid_wait", "x").await; }\n');
  fs.writeFileSync(path.join(dir, "doc.md"), doc([ROW("invalid_wait", 400)]));
  try {
    execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, TEC_DOC: path.join(dir, "doc.md"), TEC_SRC: srcFile, TEC_MIN_COMPARED: "1" },
    });
    assert("an unreadable source tree never exits 0", false, "it exited 0");
  } catch (e) {
    const out = (e.stdout ?? "") + (e.stderr ?? "");
    assert(
      "an unreadable source tree is CANNOT VERIFY (exit 2), not a crash (exit 1)",
      e.status === 2 && /CANNOT VERIFY: cannot read the source tree/.test(out),
      `status=${e.status} ${out.trim().split("\n")[0].slice(0, 150)}`,
    );
  }
}
{
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tec-empty-"));
  const srcDir = path.join(dir, "src");
  fs.mkdirSync(srcDir, { recursive: true });
  fs.writeFileSync(path.join(srcDir, "h.rs"), 'async fn h() {}\n');
  fs.writeFileSync(path.join(dir, "doc.md"), `${doc([])}`);
  for (const min of ["0", "1"]) {
    try {
      execFileSync("node", [SCRIPT], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "pipe"],
        env: { ...process.env, TEC_DOC: path.join(dir, "doc.md"), TEC_SRC: srcDir, TEC_MIN_COMPARED: min },
      });
      assert(`an empty table never exits 0 (TEC_MIN_COMPARED=${min})`, false, "it exited 0");
    } catch (e) {
      assert(
        `an empty table is CANNOT VERIFY (exit 2) even with TEC_MIN_COMPARED=${min}`,
        e.status === 2 && /there is nothing to compare/.test((e.stdout ?? "") + (e.stderr ?? "")),
        `status=${e.status}`,
      );
    }
  }
}
{
  // A floor failure is NOT a status mismatch, and it keeps the stronger exit code. The row below IS
  // comparable and correct, so the only finding is the floor — which is exactly the case that used to
  // print "1 documented-vs-emitted status mismatch(es)" with zero mismatches.
  const r = run({
    rows: [ROW("invalid_wait", 400)],
    src: 'async fn h() { respond_error(session, ctx, 400, "invalid_wait", "x").await; }\n',
    min: 15,
  });
  assert(
    "a broken floor reports 0 mismatches and exits 2 (it used to say \"1 mismatch\", exit 1)",
    r.status === 2 && /0 documented-vs-emitted status mismatch\(es\), 1 coverage problem\(s\)/.test(r.out),
    `status=${r.status} ${r.out.trim().split("\n").slice(-2).join(" | ").slice(0, 170)}`,
  );
}

{
  // The CLAMP (`Math.max(1, …)`) has its own case: an all-unverifiable run with `TEC_MIN_COMPARED=0`
  // used to print `OK (0 documented error code(s) …)` and exit 0 — the only floor switched off. The
  // shipped table's three RECORDED rows plus a source tree that extracts nothing is that run exactly
  // (no row is an unrecorded finding, so nothing else would fail).
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), "tec-nonextractable-"));
  const srcDir = path.join(dir, "src");
  fs.mkdirSync(srcDir, { recursive: true });
  fs.writeFileSync(path.join(srcDir, "h.rs"), 'async fn h() {}\n');
  try {
    execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      // NO TEC_DOC: the shipped table (this is what makes the three rows "recorded").
      env: { ...process.env, TEC_SRC: srcDir, TEC_MIN_COMPARED: "0" },
    });
    assert("an all-unverifiable run with TEC_MIN_COMPARED=0 never exits 0", false, "it exited 0");
  } catch (e) {
    const out = (e.stdout ?? "") + (e.stderr ?? "");
    assert(
      "an all-unverifiable run with TEC_MIN_COMPARED=0 is CANNOT VERIFY (the floor cannot be zeroed)",
      e.status === 2 && /could be compared/.test(out),
      `status=${e.status} ${out.trim().split("\n").slice(-1)[0].slice(0, 150)}`,
    );
  }
}

console.log(failures === 0 ? "\nALL TENANT-ERROR-CODE TESTS PASSED" : `\n${failures} TEST(S) FAILED`);
process.exit(failures ? 1 : 0);
