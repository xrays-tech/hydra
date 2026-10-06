#!/usr/bin/env node
/* Guard: the tenant API error table (§6) must match the status the code really sends.
 *
 * `dev-docs/tenant-api-integration.md` §6 is the EXTERNAL contract: integrators write retry
 * logic against `code` + HTTP (`429` ⇒ wait for `Retry-After`, `503 no_leader` ⇒ switch
 * node, `504 forward_result_unknown` ⇒ re-read before retrying). A row whose HTTP column
 * drifts from the code silently breaks every client that followed the document — and unlike
 * a missing endpoint, nothing fails: the client just retries the wrong way.
 *
 * Method: for each documented `code`, find the site(s) that EMIT it and read the status out
 * of the same expression. Six shapes exist in this tree:
 *   1. `respond_error(session, ctx, 400, "invalid_wait", …)`        — status is arg 3
 *   2. a code/status pair inside one mapping function               — `code()` + `status()`
 *   3. `… 504, "forward_result_unknown" …` inside a JSON/tuple      — adjacent literals
 *   4. `Err((413, error_body("payload_too_large", …)))`             — status in a tuple
 *   5. a short-circuit helper whose BODY builds the response        — followed into the helper, whose
 *      call may SPAN LINES (`proxy.rs:859`/`:885`)
 *   6. a hand-built JSON body sent by `respond_json*`               — bound to the call that SENDS it
 * Everything is attributed to the CALL, never to a character window: both shape 5 and shape 6 used to
 * look at a fixed slice of text, which let a NEIGHBOUR handler's call answer for this literal
 * (measured 2026-09-30: a literal whose own call passes 404 was reported as emitting 418 — a false
 * DRIFT that would have an operator "fix" a correct file).
 * A row this guard cannot read is a RECORDED decision, not a silent gap: an unreadable row must be
 * listed in `UNVERIFIED_OK` with its reason (otherwise it is DRIFT — teach the extraction that shape,
 * or record why it cannot be read), the list is printed on every run, and an entry whose row CAN be
 * read is DRIFT too (the record must be deleted so the row is compared again). Judged against the
 * SHIPPED document only, so focused fixtures are unaffected.
 * A code emitted with TWO DIFFERENT statuses at different sites is a finding too: the table can only
 * name one, so a client following it would be wrong at one of the sites.
 *
 * Exit 0 = every comparable row agrees · 1 = a mismatch, or too few rows could be compared
 * · 2 = the inputs could not be read.
 *
 * Falsified: changing a documented HTTP column, and changing a code site's status, each turn
 * this red (see `check_tenant_error_codes.test.cjs`).
 */
"use strict";
const fs = require("fs");
const { records, audit } = require("./recorded_exceptions.cjs");
const path = require("path");

const ROOT = path.join(__dirname, "..");
const SHIPPED_DOC = path.join(ROOT, "dev-docs", "tenant-api-integration.md");
const DOC = process.env.TEC_DOC ?? SHIPPED_DOC;
/** Is this run reading the SHIPPED table? Staleness of the recorded rows is judged only then. */
const usingShippedDoc = path.resolve(DOC) === SHIPPED_DOC;
const SRC_DIRS = (process.env.TEC_SRC ?? "crates/hydra-server/src,crates/hydra-core/src")
  .split(",")
  .filter(Boolean);
// `Math.max(1, …)`: `TEC_MIN_COMPARED=0` used to switch the ONLY floor off entirely, and an
// empty §6 table then printed `OK (0 documented error code(s) …)` with exit 0 (round 157).
const MIN_COMPARED = Math.max(1, Number(process.env.TEC_MIN_COMPARED ?? 15));
const SECTION = "## 6. 错误码总表";

/**
 * Rows the extraction CANNOT compare, each with the reason — a RECORDED decision, not a silent `????`.
 *
 * Two rules keep the list honest: a row that cannot be compared and is NOT recorded here is a finding
 * (a new code whose emission shape this guard cannot read must be decided on, not ignored), and an
 * entry that is no longer needed is a finding too (the extraction improved ⇒ delete the record).
 *
 * The three below are the real occurrences. Their statuses ARE written down in the code — at
 * `tenant_api/handlers.rs:467`, `respond_error(session, ctx, 400, e.code(), …)` — but the literals live
 * in `tenant_api/time_bound.rs` inside `BoundError::code()`, i.e. the status and the code name are in
 * DIFFERENT FILES. Following `.code()` across files is exactly the route that earlier produced a wrong
 * status for a different enum (this file's own note records `rate_limited` coming back as 400 through
 * it), so the honest choice is to state the limit here rather than guess.
 */
const UNVERIFIED_OK = records(process.env.CTEC_UNVERIFIED_OK, [
  ["invalid_since", "emitted by `BoundError::code()` (`tenant_api/time_bound.rs:51`); the 400 lives at the call site in `tenant_api/handlers.rs:467`, and following `.code()` across files once attributed a DIFFERENT enum's status"],
  ["invalid_until", "same shape as `invalid_since` (`time_bound.rs:52` + `handlers.rs:467`)"],
  ["window_too_large", "same shape as `invalid_since` (`time_bound.rs:53` + `handlers.rs:467`)"],
]);

/** code -> documented HTTP status, from the §6 table. */
function documentedCodes(docText) {
  const parts = docText.split(SECTION);
  if (parts.length < 2) return null;
  const body = parts[1].split("\n## ")[0];
  const out = new Map();
  for (const line of body.split("\n")) {
    if (!line.startsWith("|")) continue;
    const cells = line.replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());
    const status = cells[1] && cells[1].match(/^(\d{3})$/);
    if (!status) continue;
    // A row may document several codes (`a` / `b` / `c`) sharing one status.
    for (const m of (cells[0] ?? "").matchAll(/`([a-z_][a-z0-9_]*)`/g)) {
      if (!out.has(m[1])) out.set(m[1], Number(status[1]));
    }
  }
  return out;
}

/** The status(es) the code sends together with `code`, or [] when not extractable. */
function codeStatuses(sources, code) {
  const found = new Set();
  const literal = `"${code}"`;
  for (const f of sources) {
    let idx = -1;
    while ((idx = f.text.indexOf(literal, idx + 1)) !== -1) {
      const before = f.text.slice(Math.max(0, idx - 260), idx);
      // shape 1: respond_error(…, <status>, "<code>"
      const viaRespond = before.match(/respond_error\([^;]{0,240}?,\s*(\d{3})\s*,\s*$/);
      if (viaRespond) {
        found.add(Number(viaRespond[1]));
        continue;
      }
      // shape 3: two adjacent literals: `<status>, "<code>"`
      const adjacent = before.match(/[,(\s](\d{3})\s*,\s*$/);
      if (adjacent) {
        found.add(Number(adjacent[1]));
        continue;
      }
      // shape 4: the status travels in a tuple next to a shared body builder:
      //   `return Err((413, error_body("payload_too_large", …)))`
      const viaTuple = before.match(/\(\s*(\d{3})\s*,\s*\w+_body\(\s*$/);
      if (viaTuple) {
        found.add(Number(viaTuple[1]));
        continue;
      }
      // shape 5: the literal is an argument of a short-circuit helper whose BODY builds the
      // response (`short_circuit_rate_limited(session, "rate_limited", …)` ->
      // `ResponseHeader::build(429, …)`). Resolved through the helper, not guessed. The call may
      // SPAN SEVERAL LINES — both real occurrences (`proxy.rs:859`/`:885`) do — so the helper name is
      // also found by walking back over the argument list (see the block below).
      const lineStart = f.text.lastIndexOf("\n", idx) + 1;
      const prevEnd = Math.max(0, lineStart - 1);
      const prevStart = f.text.lastIndexOf("\n", Math.max(0, prevEnd - 1)) + 1;
      const prevLine = f.text.slice(prevStart, prevEnd);
      const thisLine = f.text.slice(lineStart, f.text.indexOf("\n", idx) === -1 ? undefined : f.text.indexOf("\n", idx));
      const helper = prevLine.match(/([a-z_][a-z0-9_]{4,})\(\s*$/) || thisLine.match(/([a-z_][a-z0-9_]{4,})\(\s*$/);
      let helperName = helper ? helper[1] : null;
      if (!helperName) {
        // The call may SPAN LINES. The two real occurrences (`proxy.rs:859` and `:885`) are
        //   `return short_circuit_rate_limited(` / `    session,` / `    "rate_limited",` / `    …`
        // so neither the literal's own line nor the line above it ENDS with `call(`. Walk back over
        // the argument list to the line that OPENED this call, stopping at a line that TERMINATES a
        // previous statement (`;`, `{`, `}`) — a completed call above must never be mistaken for this
        // one's opening. Measured 2026-09-30: without this walk both real sites yielded nothing, so
        // `rate_limited`'s 429 evidence came only from the tenant-API subsystem, and changing the
        // proxy's `429` to `418` left this guard GREEN (the row was merely `????`).
        let lineIdx = 0;
        const text = f.text;
        for (let k = 0; k < idx; k += 1) if (text[k] === "\n") lineIdx += 1;
        const allLines = text.split("\n");
        for (let k = lineIdx - 1; k >= 0 && k >= lineIdx - 8; k -= 1) {
          const t = allLines[k].trim();
          if (t === "") continue;
          const open = t.match(/([a-z_][a-z0-9_]{4,})\(\s*$/);
          if (open) {
            helperName = open[1];
            break;
          }
          if (/[;{}]$/.test(t)) break;
        }
      }
      if (helperName) {
        const fn = functionBody(sources, helperName);
        const built = fn && fn.body.match(/build\(\s*(\d{3})/);
        if (built) {
          found.add(Number(built[1]));
          continue;
        }
      }
      // shape 6: the literal sits inside a hand-built JSON body; the status travels as an
      // argument of the `respond_json*` call that sends it. Resolved through the CALL, not through a
      // character window: the old `text.slice(idx - 1500, idx + 1500)` search took the FIRST
      // `respond_json*` in the window, so a NEIGHBOUR handler's call answered for this literal —
      // measured 2026-09-30 with two handlers in one file (the neighbour sending 418, the literal's
      // own call sending 404): the guard reported "the code emits it with 418" for a literal whose own
      // call passes 404, a FALSE DRIFT that would have an operator "fix" a correct file.
      if (/serde_json::json!/.test(before.slice(-400)) || /"error"/.test(before.slice(-200))) {
        const sent = statusOfSendingCall(f.text, idx);
        if (sent !== null) {
          found.add(sent);
          continue;
        }
      }
      // shape 2: a MAPPING function — the literal lives in a `code()`-style match and the
      // status is supplied by the caller (`respond_error(session, ctx, 400, e.code(), …)`).
      //
      // This used to scan EVERY source file for that pattern, which made it match a call
      // site belonging to a DIFFERENT enum: `rate_limited` came back as 400 (the
      // `BoundError` call site) the moment its first occurrence moved into the proxy. It is
      // now scoped to the file that actually defines the `code()` accessor — otherwise the
      // code is reported UNVERIFIED, which is honest.
      // REMOVED (round 155): a blanket `f.text.match(/fn status(&self) -> u16 …/)` used to add EVERY
      // 3-digit number in that function to this code. It is not a mapping between THIS code and a
      // status: any unrelated `fn status` in the same file (e.g. another enum's) injected its value
      // here. Measured 2026-09-30 on a file whose only emission is `respond_error(…, 400, e.code(), …)`
      // plus an unrelated `fn status(&self) -> u16 { 500 }`: FALSE DRIFT "emitted with DIFFERENT
      // statuses (500/400)". The mapping-function shape below reads the statuses from the CALL SITES
      // that use `.code()` instead, which is where the status actually travels.
      if (/fn\s+code\(&self\)\s*->\s*&'static str/.test(f.text)) {
        for (const m of f.text.matchAll(/respond_error\([^;]{0,200}?,\s*(\d{3})\s*,\s*[a-z_]*\w*\.code\(\)/g)) {
          found.add(Number(m[1]));
        }
      }
    }
  }
  return [...found];
}

/**
 * The status of the `respond_json*` call that SENDS the JSON body containing position `idx`, or null.
 *
 * Two real shapes, both present in `crates/hydra-server/src/tenant_api/`:
 *   A. the body is inline      — `respond_json(session, ctx, 404, json!({ … "code": … }))`: the call
 *      CONTAINS the literal, and the status argument precedes it;
 *   B. the body is a variable  — `let body = json!({ … "code": … });` … `respond_json(session, ctx,
 *      200, &body).await`: the sending call comes AFTER the literal.
 * Rules that make this attribution honest:
 *   • never leave the enclosing FUNCTION (a neighbour handler cannot answer for this literal);
 *   • prefer the call that actually CONTAINS the literal; only then fall back to the nearest call
 *     AFTER it (shape B) — a call BEFORE it that does not contain it is never the sender.
 * Returns null when nothing can be attributed, so the row stays honestly "unverifiable".
 */
function statusOfSendingCall(text, idx) {
  const fnStart = Math.max(
    text.lastIndexOf("\nasync fn ", idx),
    text.lastIndexOf("\npub async fn ", idx),
    text.lastIndexOf("\nfn ", idx),
    text.lastIndexOf("\npub fn ", idx),
  );
  let fnEnd = text.indexOf("\n}\n", idx);
  if (fnEnd === -1) fnEnd = text.length;
  const from = fnStart === -1 ? 0 : fnStart;
  const scope = text.slice(from, fnEnd);
  const base = from;
  const candidates = [];
  const re = /respond_json\w*\(/g;
  let m;
  while ((m = re.exec(scope)) !== null) {
    const open = base + m.index + m[0].length - 1; // index of the `(`
    const span = callSpan(text, open);
    // The STATUS is the first 3-digit literal argument (`respond_json(session, 404, …)` and
    // `respond_json(session, ctx, 200, …)` are both real). Counting the preceding arguments instead
    // was wrong: the number of them varies, and requiring two made the containing call unresolvable
    // (measured: the shape-A fixture came back `????`).
    const statusMatch = text.slice(open, Math.min(open + 240, text.length)).match(/(?:^|,)\s*(\d{3})\s*,/);
    candidates.push({ open, span, status: statusMatch ? Number(statusMatch[1]) : null });
  }
  const inside = candidates.find((c) => c.span !== null && c.open < idx && idx < c.span);
  if (inside && inside.status !== null) return inside.status;
  const after = candidates.filter((c) => c.open > idx && c.status !== null).sort((a, b) => a.open - b.open);
  return after.length ? after[0].status : null;
}

/**
 * The index of the `)` matching the `(` at `open`, skipping string literals (a `)` inside a message
 * string must not close the call), or null when unbalanced.
 */
function callSpan(text, open) {
  let depth = 0;
  let q = null;
  for (let i = open; i < text.length; i += 1) {
    const c = text[i];
    if (q) {
      if (c === "\\") {
        i += 1;
        continue;
      }
      if (c === q) q = null;
      continue;
    }
    if (c === '"' || c === "'") {
      q = c;
      continue;
    }
    if (c === "(") depth += 1;
    else if (c === ")") {
      depth -= 1;
      if (depth === 0) return i;
    }
  }
  return null;
}

/**
 * The body of `fn <name>` in the scanned sources, or null.
 *
 * This was MISSING and the code below called it anyway: any hit of the "helper call on the previous
 * line" shape threw `ReferenceError: functionBody is not defined`, and because `main()` has no
 * try/catch the process died with a stack trace and **exit 1** — the code this guard's header defines
 * as "a mismatch". Measured 2026-09-30 with a two-function fixture (`short_circuit_rate_limited(` on
 * the line above the `"rate_limited"` literal). A crash is not a verdict; the shape must fall through
 * to the honest "unverifiable" path when the helper has no body to read.
 */
function functionBody(sources, fnName) {
  const re = new RegExp(`fn\\s+${fnName}\\s*[(<]`);
  for (const f of sources) {
    const m = f.text.match(re);
    if (!m) continue;
    const rest = f.text.slice(m.index);
    const close = rest.indexOf("\n}");
    return { body: close === -1 ? rest.slice(0, 1200) : rest.slice(0, close), where: f.rel };
  }
  return null;
}

function walk(dir, out = []) {
  // Round 157: this used to be unguarded, so a `TEC_SRC` that is a FILE (or unreadable) threw an
  // uncaught ENOTDIR and the process died with a stack trace — exit 1, which this guard's own header
  // defines as "a mismatch". An unreadable input is exit 2 (`the inputs could not be read`), exactly
  // as the document-reading path above already does.
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch (e) {
    throw new ScanError(`cannot read the source tree \`${dir}\`: ${e.message}`);
  }
  for (const e of entries) {
    const full = path.join(dir, e.name);
    if (e.isDirectory()) walk(full, out);
    else if (e.name.endsWith(".rs")) out.push(full);
  }
  return out;
}

/** A reason to stop with `CANNOT VERIFY` (exit 2) rather than to report a verdict. */
class ScanError extends Error {
  constructor(message) {
    super(message);
    this.code = 2;
  }
}

function main() {
  let docText;
  try {
    docText = fs.readFileSync(DOC, "utf8");
  } catch (e) {
    console.error(`cannot read ${DOC}: ${e.message}`);
    process.exit(2);
  }
  let files;
  try {
    files = SRC_DIRS.flatMap((d) => {
      const full = path.isAbsolute(d) ? d : path.join(ROOT, d);
      return fs.existsSync(full) ? walk(full) : [];
    });
  } catch (e) {
    // `ScanError` (exit 2) or anything else from the filesystem: an unreadable input is NOT a verdict.
    const code = e && e.code === 2 ? 2 : 2;
    console.error(`[tenant-error-codes] CANNOT VERIFY: ${e.message}`);
    process.exit(code);
  }
  if (files.length === 0) {
    console.error("no source files found — nothing could be checked");
    process.exit(2);
  }
  // Comments and whole `#[cfg(test)]` items are blanked, string CONTENTS are kept (the extraction
  // matches `"code_name"` literals, and `stripCommentsAndTestItems` preserves them on every line
  // that is not inside a test item). Measured 2026-09-30: with the raw text, a documented code whose
  // only "emission" was a COMMENT — or a unit test — was reported as `OK … = 404` while the product
  // never emits it at all.
  const { stripCommentsAndTestItems } = require('./rust_blank.cjs');
  const sources = files.map((f) => ({
    rel: path.relative(ROOT, f),
    text: stripCommentsAndTestItems(fs.readFileSync(f, "utf8")),
  }));

  const documented = documentedCodes(docText);
  if (documented === null) {
    console.error(`cannot find \`${SECTION}\` in ${DOC}`);
    process.exit(2);
  }
  if (documented.size === 0) {
    // Unconditional: an empty table proves NOTHING, whatever document was pointed at (round 157 —
    // with `TEC_MIN_COMPARED=0` this used to print `OK (0 documented error code(s) …)` and exit 0).
    console.error(`no documented error code parsed from ${SECTION} in ${DOC} — there is nothing to compare`);
    process.exit(2);
  }
  if (documented.size < 10 && !process.env.TEC_DOC) {
    console.error(`only ${documented.size} documented error code(s) parsed — the extractor is probably broken`);
    process.exit(2);
  }

  const problems = [];
  const compared = [];
  const unverified = [];
  const agreed = [];
  const unverifiedUsed = new Set();
  for (const [code, docStatus] of documented) {
    const statuses = codeStatuses(sources, code);
    if (statuses.length === 0) {
      if (UNVERIFIED_OK.has(code)) {
        unverifiedUsed.add(code);
        unverified.push(`${code}: documented ${docStatus}, no status extractable at its emission site (recorded)`);
        continue;
      }
      // A row nobody can check is a DECISION, not a gap: this used to print `????` and still exit 0,
      // so a new code emitted in an unreadable shape — or a documented status changed for one of the
      // recorded rows after its record went stale — could pass unnoticed.
      problems.push(
        `${code}: documented ${docStatus}, but no status could be extracted at its emission site and it ` +
          `is NOT recorded in UNVERIFIED_OK — teach the extraction this shape, or record why it cannot be read`,
      );
      continue;
    }
    compared.push(code);
    const distinct = [...new Set(statuses)];
    if (distinct.length > 1) {
      // More than one status at DIFFERENT sites used to be a PASS as long as the documented one was
      // among them (`statuses.includes(docStatus)`), and the OK line printed `404/500 (table says 404)`
      // while still saying OK (measured 2026-09-30: the real tree has 0 such codes, so this is a
      // LATENT hole — the table gives a client ONE status to follow, and a code emitted two ways means
      // either the table is incomplete or one site uses the code for a different outcome).
      problems.push(
        `${code}: emitted with DIFFERENT statuses (${distinct.join("/")}) at different sites, and the ` +
          `table says ${docStatus} — a client's retry logic follows the table, which can only name one`,
      );
      continue;
    }
    if (distinct[0] !== docStatus) {
      problems.push(
        `${code}: the table says HTTP ${docStatus} but the code emits it with ` +
          `${distinct.join("/")} — a client's retry logic follows the table`,
      );
      continue;
    }
    agreed.push(code);
  }

  for (const c of agreed) {
    const [code, docStatus] = [c, documented.get(c)];
    const statuses = codeStatuses(sources, c);
    console.log(`  OK    ${code} = ${statuses.join("/")} (table says ${docStatus})`);
  }
  for (const u of unverified) console.log(`  ????  ${u}`);

  // The coverage floor joins the problem list (round 136): it used to sit AFTER the `problems`
  // exit, so the one situation it exists for — "the extraction collapsed and this check proves
  // nothing" — could never be printed when a drift was also present.
  const floors = [];
  if (compared.length < MIN_COMPARED) {
    floors.push(
      `only ${compared.length} error code(s) could be compared (< ${MIN_COMPARED}); the extraction is probably broken, so this check proves nothing`,
    );
  }
  // A stale record is a finding too: when the extraction starts reading a shape, the row must be
  // compared again instead of staying excused by an entry nobody needs any more.
  // SCOPE: judged against the SHIPPED document only — the same rule `check_documented_defaults`
  // applies to its own `UNVERIFIED_OK` list. A fixture's own six-line table is not evidence that a
  // record written for the shipped table has become stale (measured: judging it made ten focused
  // fixtures fail for a reason they cannot influence), while the shipped document plus a source tree
  // that DOES extract the row is exactly the evidence that says "delete this record".
  if (usingShippedDoc) {
    const { stale } = audit({
      records: UNVERIFIED_OK,
      needed: [],
      // `applies` is TRUE when the record must be KEPT: the code is not in the documented table at
      // all, or its status is STILL unverifiable. Stale (record no longer needed) is the complement:
      // documented AND now extracted.
      applies: (code) => !documented.has(code) || unverifiedUsed.has(code),
    });
    for (const code of stale) {
      const why = UNVERIFIED_OK.get(code);
      problems.push(
        `the UNVERIFIED_OK entry for \`${code}\` is no longer needed: its status WAS extracted from the ` +
          `code this run — delete the entry so the row is compared (recorded reason: ${why})`,
      );
    }
  }
  if (problems.length || floors.length) {
    for (const p of problems) console.error("DRIFT  " + p);
    // Both are printed in ONE run (round 136), but they are counted and worded separately: a floor
    // failure is not a mismatch, and calling it one sent the reader looking for a status that was
    // never wrong (measured: `1 documented-vs-emitted status mismatch(es)` with zero mismatches).
    for (const f of floors) console.error("CANNOT VERIFY  " + f);
    console.error(
      `${problems.length} documented-vs-emitted status mismatch(es), ${floors.length} coverage problem(s)`,
    );
    process.exit(floors.length > 0 ? 2 : 1);
  }
  console.log(
    `OK  (${compared.length} documented error code(s) match the emitted status, ` +
      `${unverified.length} unverifiable by extraction and RECORDED in UNVERIFIED_OK)`,
  );
}

if (require.main === module) main();

module.exports = { documentedCodes, codeStatuses };
