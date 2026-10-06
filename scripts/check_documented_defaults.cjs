#!/usr/bin/env node
/* Guard: the DEFAULTS in `dev-docs/ops.md`'s environment tables must match the code.
 *
 * `check_documented_env.cjs` already proves every documented knob is READ somewhere —
 * it says nothing about the VALUE. A wrong default in that table is worse than a missing
 * knob: the operator reads "default 20", sizes `terminationGracePeriodSeconds` from it,
 * and the process actually drains for 60. Every number in that table was written by hand
 * and (until this checker) nothing compared it to the code.
 *
 * Method (deliberately conservative — a guard that invents defaults is worse than none):
 *   1. read the documented default out of the row (a bare/numbered second cell, or a
 *      `default N` clause in the description);
 *   2. find the knob's code site(s) and read the fallback literal out of the same
 *      expression (`env_positive_u32("VAR", N)`, `env_millis("VAR", N)`, `unwrap_or(N)`,
 *      `return Ok(N)`, `Duration::from_secs(N)`);
 *   3. COMPARE only when both sides produced a number. A row where either side has no
 *      extractable number is reported as UNVERIFIED, never as a pass;
 *   4. require a coverage floor, so the checker cannot silently degrade into
 *      "everything is unverified" and still exit 0.
 *
 * Round 119 closed two holes in that claim:
 *   1. NON-NUMERIC defaults were never compared (14 of 33 rows printed `????`), including
 *      `HYDRA_ADMIN_ADDR`, whose documented default is loopback and whose cell is an ADDRESS —
 *      a security-relevant default that could drift in either direction unnoticed. String
 *      defaults (cell literal vs `const … : &str` / `unwrap_or("…")`) are now compared.
 *   2. A row the extraction cannot compare is a RECORDED decision (`UNVERIFIED_OK`, with the
 *      reason, printed every run) instead of a silent gap: an unrecorded one FAILS, and an entry
 *      that is no longer needed fails too (checked against the shipped table only, so fixtures
 *      with two rows are unaffected).
 *
 * Round 143: WHAT COUNTS AS EVIDENCE. The code side is scanned on a copy with comments and whole
 * `#[cfg(test)]` items blanked (line numbers preserved, string contents kept — the extraction
 * matches the `"VAR"` literal itself), because the raw scan let a COMMENT be the witness and, worse,
 * let a comment that spells `env_millis("VAR", 7)` supply the fallback: measured on the real tree,
 * `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` was reported at `usage_query.rs:340` (a comment) while the real
 * read is at 343, and a fixture whose only "default" was a comment PASSED while the code fell back to
 * another number. A default that lives only inside `#[cfg(test)]` is not shipped behaviour either.
 *
 * Exit 0 = every comparable row agrees · 1 = a mismatch (or too little coverage)
 * · 2 = the input files could not be read.
 *
 * Falsified: changing a documented default to another number, and changing a code
 * fallback, each turn this red (see `scripts/check_documented_defaults.test.cjs`).
 */
"use strict";
const fs = require("fs");
const path = require("path");
// Single owner of "strip Rust comments and `#[cfg(test)]` items, line numbers preserved"
// (`scripts/rust_blank.cjs`, shared with the other Rust-scanning guards).
const { stripCommentsAndTestItems } = require("./rust_blank.cjs");
const { records, audit } = require("./recorded_exceptions.cjs");

const ROOT = path.join(__dirname, "..");
// Overridable so the test suite can point the checker at fixtures (the same trick
// `check_e2e_contracts.cjs` uses with E2E_CONTRACTS_MIN_TOKENS).
const OPS = process.env.CDD_OPS ?? path.join(ROOT, "dev-docs", "ops.md");
const SRC_DIRS = (process.env.CDD_SRC ?? "crates/hydra-core/src,crates/hydra-server/src")
  .split(",")
  .filter(Boolean);
const MIN_COMPARED = Number(process.env.CDD_MIN_COMPARED ?? 10);

/**
 * Rows whose documented default this guard CANNOT compare, each with the reason, printed on every
 * run so the list cannot grow quietly.
 *
 * The reason must be about the ROW (a secret with no default, a switch whose documented default is
 * a word the code expresses as a boolean), never about the guard being lazy: a row whose shape the
 * extraction could learn belongs in the extractor instead. Measured 2026-09-30 (round 119): 12 rows
 * **Re-measured 2026-10-01 (round 182): 10 rows here, 23 compared** — the numbers in this comment read
 * "12 rows here, 21 compared" (round 119) for thirteen rounds, which is exactly the kind of stale count
 * this repository keeps finding; the guard's OK line prints the live pair
 * (`N documented default(s) match the code, M unverifiable by extraction`), so re-measure by running it.
 * The historical note stands: before round 119, 14 rows were unverifiable with no record at all and the
 * guard still printed OK.
 */
const UNVERIFIED_OK = records(process.env.CDD_UNVERIFIED_OK, [
  ['HYDRA_ADMIN_TOKEN', 'a secret with no default; the code refuses to start when it is missing'],
  ['HYDRA_ENCRYPTION_KEY', 'a secret with no default (see HYDRA_ENCRYPTION_KEY_FILE)'],
  ['HYDRA_RESEAL_SECRETS', 'a one-shot switch documented as OFF; the code parses a vocabulary, not a value'],
  ['HYDRA_TLS_LISTEN', 'documented default is "unset" — the TLS listener is bound iff the variable is set'],
  ['HYDRA_CLICKHOUSE_URL', 'documented default is *(unset)*; required only when the sink is clickhouse'],
  ['HYDRA_CLUSTER_PEERS', 'REQUIRED in cluster mode — there is no default to compare: the list is the decision itself (unset means single-node, which is a different mode rather than a fallback value)'],
  ['HYDRA_ARACHNE_LISTEN', 'REQUIRED in cluster mode — no default: the raft transport must be told where to bind'],
  ['HYDRA_CLUSTER_ID', 'optional; its default is a HASH of the member list (computed, not a literal), which the extractor cannot read'],
  ['HYDRA_NODE_ID', 'documented default is derived at startup (host:port), not a literal'],
  ['HYDRA_TRUSTED_PROXIES', 'documented default is empty; the code splits a list, no scalar fallback'],
  ['HYDRA_TENANT_API', 'documented default `on`; the code parses it as a boolean master switch'],
]);


/** `32 MiB (= 33554432 **bytes**)` → 33554432; `5_000` → 5000; `20` → 20. */
function toNumber(text) {
  const bytes = text.match(/([\d_]+)\s*(?:MiB|KiB|GiB)/i);
  if (bytes) {
    const n = Number(bytes[1].replace(/_/g, ""));
    const unit = text.match(/(MiB|KiB|GiB)/i)[1].toLowerCase();
    const mul = unit === "kib" ? 1024 : unit === "mib" ? 1024 * 1024 : 1024 * 1024 * 1024;
    return n * mul;
  }
  const plain = text.match(/([\d][\d_]*)/);
  return plain ? Number(plain[1].replace(/_/g, "")) : null;
}

/** Documented default for one table row, or null. */
function documentedDefault(row) {
  const cells = row.replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());
  const second = cells[1] ?? "";
  if (second && !/^\*\(unset\)\*$/.test(second) && !/^—$/.test(second)) {
    // A short second cell IS the default ("`60`", "60 s", "32 MiB (= 33554432 **bytes**)").
    const numericCell = /^[`\s]*[\d][\d_\s]*(?:s|ms|bytes|\*\*bytes\*\*)?[`\s]*$/i.test(second)
      || /(?:MiB|KiB|GiB|bytes)/i.test(second);
    if (numericCell && second.length <= 60) {
      const n = toNumber(second);
      if (n !== null) return { value: n, how: `cell: ${second}` };
    } else if (/(?:MiB|KiB|GiB)/i.test(second)) {
      const m = second.match(/([\d_]+)\s*(?:MiB|KiB|GiB)/i);
      if (m) return { value: toNumber(second), how: `cell: ${second.slice(0, 40)}` };
    }
  }
  const prose = row.match(/\bdefaults?(?:\s+to)?\D{0,14}?([\d][\d_]*)/i);
  if (prose) return { value: Number(prose[1].replace(/_/g, "")), how: `prose: ${prose[0].trim()}` };
  return null;
}

/**
 * The default the CODE falls back to for `varName`, or null.
 *
 * Four shapes carry it, all measured in this tree:
 *   1. inline:            `env_positive_u32("VAR", 60)`
 *   2. in the window:     `var("VAR")… .unwrap_or(120)`   (any number of lines apart)
 *   3. a local parser:    `parse_x(var("VAR")…)` whose body ends `.unwrap_or(20)`
 *   4. a named constant:  `.unwrap_or(DEFAULT_FORWARD_TIMEOUT_SECS)` → `const … = 5`
 * `32 * 1024 * 1024` is evaluated; anything else is reported UNVERIFIED rather than
 * guessed.
 */
function numericLiteral(text) {
  const product = text.match(/(\d[\d_]*)\s*\*\s*1024\s*\*\s*1024/);
  if (product) return Number(product[1].replace(/_/g, "")) * 1024 * 1024;
  const kib = text.match(/(\d[\d_]*)\s*\*\s*1024/);
  if (kib) return Number(kib[1].replace(/_/g, "")) * 1024;
  const m = text.match(/([\d][\d_]*)/);
  return m ? Number(m[1].replace(/_/g, "")) : null;
}

function constValue(sources, rawName) {
  const name = rawName.split("::").pop().replace(/[^A-Z0-9_]/gi, "");
  if (!name) return null;
  const re = new RegExp(`const\\s+${name}\\s*:[^=]+=\\s*([^;]+);`);
  for (const f of sources) {
    const m = f.text.match(re);
    if (m) {
      const n = numericLiteral(m[1]);
      if (n !== null) return { value: n, where: `${f.rel} (const ${name})` };
    }
  }
  return null;
}

function functionBody(sources, fnName) {
  const re = new RegExp(`fn\\s+${fnName}\\s*[(<]`);
  for (const f of sources) {
    const m = f.text.match(re);
    if (!m) continue;
    const rest = f.text.slice(m.index);
    const close = rest.indexOf("\n}");
    const body = close === -1 ? rest.slice(0, 1200) : rest.slice(0, close);
    return { body, where: f.rel };
  }
  return null;
}

/** `Duration::from_secs(hydra_server::http::DEFAULT_X)` → resolve DEFAULT_X. */
function anyConstIn(sources, text) {
  const names = text.match(/\b([A-Z][A-Z0-9_]{3,})\b/g) ?? [];
  for (const n of names) {
    const c = constValue(sources, n);
    if (c) return c;
  }
  return null;
}

/** `ProxyConfig::default().max_request_body_hard` → the field's initialiser. */
function defaultFieldValue(sources, text) {
  const m = text.match(/\b([A-Za-z_][A-Za-z0-9_]*)::default\(\)\.([a-z_][A-Za-z0-9_]*)/);
  if (!m) return null;
  const [, typeName, field] = m;
  for (const f of sources) {
    const start = f.text.search(new RegExp(`impl\\s+Default\\s+for\\s+${typeName}\\b`));
    if (start === -1) continue;
    // From the `impl` header to the first column-0 closing brace — i.e. that impl block
    // only. (Scanning the whole file instead would match the struct's field declaration
    // `pub field: u64,` and report "no number" for a knob that HAS a default.)
    const rest = f.text.slice(start);
    const end = rest.indexOf("\n}");
    const body = end === -1 ? rest : rest.slice(0, end);
    const fm = body.match(new RegExp(`\\b${field}\\s*:\\s*([^,\\n]{1,60})`));
    if (fm) {
      const n = numericLiteral(fm[1]);
      if (n !== null) return { value: n, where: `${f.rel} (${typeName}::default().${field})` };
    }
  }
  return null;
}

/**
 * Does this line mention `varName` as a TOKEN?
 *
 * The lookup used to be `line.includes(varName)`, so a NEIGHBOURING knob's line matched: measured
 * 2026-09-30 with `HYDRA_LISTEN_EXTRA` — the guard took that line's default (9999) as the value of
 * `HYDRA_LISTEN` and reported "OK: documented default matches the code" while the real
 * `HYDRA_LISTEN` fallback in the same file was 1111. Since the loop stops at the first line that
 * yields a value, a neighbour appearing earlier wins outright.
 */
function lineMentions(line, varName) {
  const escaped = varName.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  return new RegExp(`\\b${escaped}\\b`).test(line);
}

function codeDefault(sources, varName, rawLines) {
  for (let i = 0; i < rawLines.length; i++) {
    if (!lineMentions(rawLines[i], varName)) continue;
    // Two windows on purpose: `wide` keeps the call that WRAPS the var (the inline
    // helper pattern needs the text before it), while `window` starts at the var and
    // stops at the next `HYDRA_*` literal, so a fallback belonging to a neighbouring
    // knob can never be attributed to this one.
    const wide = rawLines.slice(Math.max(0, i - 3), i + 12).join("\n");
    const atVar = wide.search(new RegExp(`\\b${varName.replace(/[.*+?^${}()|[\]\\]/g, '\\\\$&')}\\b`));
    const afterVar = wide.slice(atVar + varName.length);
    const nextKnob = afterVar.search(/HYDRA_[A-Z0-9_]+/);
    const window = nextKnob === -1 ? wide : wide.slice(0, atVar + varName.length + nextKnob);
    const inline = wide.match(new RegExp(`\\w+\\s*\\(\\s*"${varName}"\\s*,\\s*([\\d_]+)`));
    if (inline) return { value: numericLiteral(inline[1]), how: "helper called with an inline default", line: i + 1 };
    const unwrapped = window.match(new RegExp(`"${varName}"[\\s\\S]{0,900}?unwrap_or(?:_else)?\\(\\s*(?:\\|[^|]*\\|\\s*)?([^\\n]{1,120})`));
    if (unwrapped) {
      const inner = unwrapped[1].trim();
      const n = numericLiteral(inner);
      if (n !== null) return { value: n, how: "unwrap_or", line: i + 1 };
      const c = constValue(sources, inner) || anyConstIn(sources, inner) || defaultFieldValue(sources, inner);
      if (c) return { value: c.value, how: `unwrap_or(${inner.trim()}) -> ${c.where}`, line: i + 1 };
    }
    const ret = window.match(new RegExp(`"${varName}"[\\s\\S]{0,900}?return\\s+Ok\\(\\s*\\{?\\s*([^,)}]{1,200})`));
    if (ret) {
      const n = numericLiteral(ret[1]);
      if (n !== null) return { value: n, how: "early return", line: i + 1 };
      const c = constValue(sources, ret[1]) || anyConstIn(sources, ret[1]) || defaultFieldValue(sources, ret[1]);
      if (c) return { value: c.value, how: `return ${ret[1].trim()} -> ${c.where}`, line: i + 1 };
    }
    const secs = window.match(new RegExp(`"${varName}"[\\s\\S]{0,300}?from_secs\\(\\s*([\\d_]+)`));
    if (secs) return { value: numericLiteral(secs[1]), how: "Duration::from_secs", line: i + 1 };
    // shape 3: the site calls a local parser — follow it one level. The call may be on
    // the PREVIOUS line (the var is one of its arguments).
    const callCandidates = [];
    // The parser call can be a few lines ABOVE the var (its argument list spans lines).
    const nearby = [];
    for (let k = Math.max(0, i - 3); k <= i + 1; k++) nearby.push(rawLines[k] ?? "");
    for (const ln of nearby) {
      for (const m of ln.matchAll(/([a-z_][a-z0-9_]{4,})\s*\(/g)) callCandidates.push(m[1]);
    }
    for (const fnName of callCandidates) {
      if (fnName === "var" || fnName === "ok" || fnName === "filter" || fnName === "map") continue;
      const fn = functionBody(sources, fnName);
      if (!fn) continue;
      const inner = fn.body.match(/unwrap_or(?:_else)?\(\s*(?:\|[^|]*\|\s*)?([^\n]{1,120})/);
      if (!inner) continue;
      const viaField = defaultFieldValue(sources, inner[1]);
      if (viaField) return { value: viaField.value, how: `${fnName}() -> ${viaField.where}`, line: i + 1 };
      const n = numericLiteral(inner[1]);
      if (n !== null) return { value: n, how: `${fnName}().unwrap_or`, line: i + 1 };
      const c = constValue(sources, inner[1]) || anyConstIn(sources, inner[1]) || defaultFieldValue(sources, inner[1]);
      if (c) return { value: c.value, how: `${fnName}() -> ${c.where}`, line: i + 1 };
    }
  }
  return null;
}

/**
 * The DOCUMENTED default when it is a literal string rather than a number.
 *
 * `HYDRA_ADMIN_ADDR` documents `` `127.0.0.1:8081` `` and "**Bind loopback only**" — a
 * security-relevant default — while this guard only understood numbers, so the row printed
 * `????` and a change to `DEFAULT_ADMIN_LISTEN` was invisible (measured 2026-09-30: the code
 * said `127.0.0.1:8081`, the doc agreed, and NOBODY would have noticed if either moved).
 */
function documentedStringDefault(row) {
  const cells = row.replace(/^\|/, "").replace(/\|$/, "").split("|").map((c) => c.trim());
  const second = cells[1] ?? "";
  if (/^\*\(unset\)\*$/.test(second) || /^—$/.test(second)) return { value: null, how: "unset (no default documented)" };
  const m = second.match(/^`([^`]+)`$/);
  if (m && !/^[\d_\s]+$/.test(m[1])) return { value: m[1], how: `cell: ${second}` };
  return null;
}

/** A `const NAME: &str = "…"` anywhere in the scanned sources, or null. */
function stringConstValue(sources, rawName) {
  const name = rawName.split("::").pop().replace(/[^A-Z0-9_]/gi, "");
  if (!name) return null;
  const re = new RegExp(`const\\s+${name}\\s*:[^=]+=\\s*"([^"]*)"`);
  for (const f of sources) {
    const m = f.text.match(re);
    if (m) return { value: m[1], where: `${f.rel} (const ${name})` };
  }
  return null;
}

/** The STRING the code falls back to for `varName`, or null. */
function codeStringDefault(sources, varName, rawLines) {
  for (let i = 0; i < rawLines.length; i++) {
    if (!lineMentions(rawLines[i], varName)) continue;
    const window = rawLines.slice(i, i + 12).join("\n");
    const m = window.match(new RegExp(`"${varName}"[\\s\\S]{0,600}?unwrap_or(?:_else)?\\(\\s*(?:\\|[^|]*\\|\\s*)?([^\\n]{1,120})`));
    if (!m) continue;
    const inner = m[1].trim();
    const lit = inner.match(/^"([^"]*)"/);
    if (lit) return { value: lit[1], how: "unwrap_or with a string literal", line: i + 1 };
    // The fallback is usually `DEFAULT_X.to_string()` / `DEFAULT_X`: take the LEADING identifier
    // (`stringConstValue` used to be handed the whole expression, whose non-alphanumerics were
    // then stripped into a name that matches no constant — that is why the two address rows kept
    // printing `????` after the string path was added).
    const ident = inner.match(/^([A-Za-z_][A-Za-z0-9_:]*)/);
    const c = ident ? stringConstValue(sources, ident[1]) : null;
    if (c) return { value: c.value, how: `unwrap_or(${ident[1]}) -> ${c.where}`, line: i + 1 };
  }
  return null;
}

function sourceFiles() {
  const out = [];
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, e.name);
      if (e.isDirectory()) walk(full);
      else if (e.name.endsWith(".rs")) out.push(full);
    }
  };
  for (const d of SRC_DIRS) {
    const full = path.isAbsolute(d) ? d : path.join(ROOT, d);
    if (fs.existsSync(full)) walk(full);
  }
  return out;
}

function main() {
  let ops;
  try {
    ops = fs.readFileSync(OPS, "utf8").split("\n");
  } catch (e) {
    console.error(`cannot read ${OPS}: ${e.message}`);
    process.exit(2);
  }
  const files = sourceFiles();
  // The floor protects the REAL tree from a wrong ROOT (it silently compares almost
  // nothing otherwise). An explicit CDD_SRC is a deliberate override — that is how the
  // test suite points this at a single-file fixture.
  if (files.length < 10 && !process.env.CDD_SRC) {
    console.error(`only ${files.length} source file(s) found — the tree is not where I think it is`);
    process.exit(2);
  }
  if (files.length === 0) {
    console.error("no source files found — nothing could be compared");
    process.exit(2);
  }
  const sources = files.map((f) => {
    const text = fs.readFileSync(f, "utf8");
    // Comments are not code, and `#[cfg(test)]` items are not shipped behaviour: a doc-comment that
    // shows `env_millis("HYDRA_X", 5_000)`, or a test that passes a different default, must not be
    // the evidence a DOCUMENTED default is checked against. Measured 2026-09-30: the raw scan took
    // `usage_query.rs:340` — a comment — as the witness line for
    // `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS`, while the real read is at 343; a comment that spells the
    // call is enough to hand the guard a fabricated fallback.
    // Line numbers survive (measured 473 -> 473 lines) and string CONTENTS are kept, because the
    // extraction matches the `"HYDRA_X"` literal itself.
    const code = stripCommentsAndTestItems(text);
    return { rel: path.relative(ROOT, f), text: code, lines: code.split("\n") };
  });

  const rows = new Map(); // varName -> row text (first occurrence wins)
  for (const line of ops) {
    const m = line.match(/^\|\s*`?(HYDRA_[A-Z0-9_]+)`?\s*\|/);
    if (m && !rows.has(m[1])) rows.set(m[1], line);
  }

  const problems = [];
  const compared = [];
  const unverified = [];
  for (const [name, row] of rows) {
    const doc = documentedDefault(row);
    let code = null;
    let where = null;
    for (const f of sources) {
      const hit = codeDefault(sources, name, f.lines);
      if (hit && hit.value !== null) {
        code = hit.value;
        where = `${f.rel}:${hit.line} (${hit.how})`;
        break;
      }
    }
    // Second pass: a NON-numeric default (an address, a URL, a mode word). Only attempted when
    // the numeric path found nothing, so every number keeps going through the old route.
    if (doc === null || code === null) {
      const docStr = documentedStringDefault(row);
      let codeStr = null;
      for (const f of sources) {
        const hit = codeStringDefault(sources, name, f.lines);
        if (hit) {
          codeStr = hit;
          break;
        }
      }
      if (docStr && docStr.value !== null && codeStr) {
        compared.push({ name, doc: docStr.value, code: codeStr.value, where: `${codeStr.how}`, how: docStr.how });
        if (docStr.value !== codeStr.value) {
          problems.push(
            `${name}: ops.md says \`${docStr.value}\` (${docStr.how}) but the code falls back to \`${codeStr.value}\` @ ${codeStr.how}`,
          );
        }
        continue;
      }
    }
    if (doc === null || code === null) {
      const reason = UNVERIFIED_OK.get(name);
      unverified.push(
        `${name}: documented=${doc ? doc.value + ` (${doc.how})` : "no number"} · code=${code === null ? "no fallback found" : code + ` @ ${where}`}${reason ? `  [recorded: ${reason}]` : ""}`,
      );
      // A row the extraction cannot compare is now a DECISION, not a silent gap: it must be in
      // UNVERIFIED_OK with a reason. Otherwise the guard can degrade one row at a time to
      // "????" and still exit 0 — which is exactly what 14 of 33 rows did until this round.
      if (!reason) {
        problems.push(
          `${name}: the documented default cannot be compared with the code and is not recorded in ` +
            `UNVERIFIED_OK — either teach the extraction this shape, or record why it cannot be compared`,
        );
      }
      continue;
    }
    compared.push({ name, doc: doc.value, code, where, how: doc.how });
    if (doc.value !== code) {
      problems.push(
        `${name}: ops.md says ${doc.value} (${doc.how}) but the code falls back to ${code} @ ${where}`,
      );
    }
  }

  for (const c of compared) {
    console.log(`  OK    ${c.name} = ${c.code} (ops.md ${c.doc}, ${c.how}) @ ${c.where}`);
  }
  for (const u of unverified) console.log(`  ????  ${u}`);
  // A stale allowlist entry is a claim about a row that is no longer unverifiable — remove it,
  // otherwise the list silently describes a state that does not exist. Enforced against the
  // SHIPPED table only: a fixture with two rows legitimately has none of these names, so running
  // the check there would report ten "stale" entries that are simply not in that document (the
  // first version of this check did exactly that and broke two fixture tests).
  const isShippedTable = path.resolve(OPS) === path.resolve(ROOT, 'dev-docs', 'ops.md');
  const stillUnverified = (name) => unverified.some((u) => u.startsWith(`${name}:`));
  const { stale } = audit({ records: UNVERIFIED_OK, needed: [], applies: stillUnverified });
  for (const name of stale) {
    const msg = `${name} is in UNVERIFIED_OK but that row IS comparable (or absent) now — drop the entry`;
    if (isShippedTable) problems.push(msg);
    else console.log(`  note  ${msg} (not enforced: checking ${path.relative(ROOT, OPS)}, not the shipped table)`);
  }

  // The coverage floor joins the problem list (round 136): it used to sit AFTER the `problems`
  // exit, so the one situation it exists for — "the extraction collapsed and this check proves
  // nothing" — could never be printed when a drift was also present.
  if (compared.length < MIN_COMPARED) {
    problems.push(
      `only ${compared.length} documented default(s) could be compared (< ${MIN_COMPARED}); the extraction is probably broken, so this check proves nothing`,
    );
  }
  if (problems.length) {
    for (const p of problems) console.error("DRIFT  " + p);
    console.error(`${problems.length} documented-default drift(s)`);
    process.exit(1);
  }
  console.log(
    `OK  (${compared.length} documented default(s) match the code, ${unverified.length} unverifiable by extraction)`,
  );
}

if (require.main === module) main();

module.exports = { documentedDefault, numericLiteral, codeDefault };
