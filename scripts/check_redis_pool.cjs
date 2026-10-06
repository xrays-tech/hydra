#!/usr/bin/env node
'use strict';
/**
 * Every fred `Pool::new` must go through the single owner in
 * `crates/hydra-server/src/redis/mod.rs`.
 *
 * Why this exists (measured 2026-09-30): this tree had **five** hand-written `Pool::new`
 * calls — production, the lib-test harness, and three integration-test sites — and **four of
 * them passed fred's defaults**. fred keeps three load-bearing settings OUTSIDE `Config` and
 * passes them as separate constructor arguments, and its default for each is wrong here:
 *
 *   perf.default_command_timeout = 0  -> wait FOREVER (a half-open Redis stalls the data
 *                                        plane: the rate-limit EVAL runs before routing)
 *   connection.unresponsive      off  -> a dead socket is never recycled
 *   policy                       None -> a closed connection is NEVER re-dialled, which is
 *                                        how a ~2 s Redis outage left the leader demoted and
 *                                        every limit failing open for 90 s+ (P1, round 99)
 *
 * Those four sites were found by reading the tree after the P1, not by any guard, and a
 * test pool that cannot time out and cannot reconnect **tests a different program than the
 * one that ships**. So this check asserts two rules:
 *
 *   R1 single owner: `Pool::new(` may appear only in `crates/hydra-server/src/redis/mod.rs`.
 *      A new call site belongs in the shared builder (`build_pool` / `build_pool_with`).
 *   R2 no `None` policy: in that file the 4th argument of every `Pool::new(` call is `Some(`.
 *      (`None` is the defect itself; it is invisible until Redis actually goes away.)
 *
 * The 4th argument is located by taking the argument list of the call and splitting it at
 * top level on commas — not by a regex over the whole file, because the call spans lines and
 * its arguments contain nested parens.
 *
 * Exit codes: 0 clean, 1 a violation, 2 CANNOT VERIFY (the owner file is missing or has no
 * `Pool::new` call at all — a scan that matches nothing is not a pass).
 */
const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const OWNER = 'crates/hydra-server/src/redis/mod.rs';
// `path.resolve` (not `join`) so an ABSOLUTE override — how the test suite points the
// script at a temporary tree — is honoured instead of being appended to ROOT.
const OWNER_ABS = path.resolve(ROOT, process.env.CRP_OWNER || OWNER);
const CRATES = path.resolve(ROOT, process.env.CRP_CRATES || 'crates');

/** Every `.rs` file under `dir` (skipping build output). */
function rustFiles(dir, out = []) {
  if (!fs.existsSync(dir)) return out;
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === 'target' || entry.name === '.git') continue;
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) rustFiles(full, out);
    else if (entry.name.endsWith('.rs')) out.push(full);
  }
  return out;
}

/** The argument list of the `Pool::new(` call starting at `from`, with its source offset. */
function callArguments(src, from) {
  const open = src.indexOf('(', from);
  if (open === -1) return null;
  let depth = 0;
  for (let i = open; i < src.length; i += 1) {
    const c = src[i];
    if (c === '(') depth += 1;
    else if (c === ')') {
      depth -= 1;
      if (depth === 0) return { text: src.slice(open + 1, i), offset: open + 1 };
    }
  }
  return null;
}

/**
 * Split an argument list on TOP-LEVEL commas only.
 *
 * "Top level" has to mean "not inside a string literal" as well: the split used to walk the raw
 * text, so a comma inside a TOP-LEVEL string argument shifted every later slot. Measured
 * 2026-09-30 on `Pool::new(cfg, "a,b", Some(conn), None, 2)`: the old splitter produced
 * `["cfg","\"a","b\"","Some(conn)","None","2"]`, i.e. `parts[3]` was `Some(conn)` while the REAL
 * 4th argument is `None` — R2's "the policy must be `Some(...)`" was satisfied by an argument that
 * is not the policy at all.
 *
 * (A comma inside a NESTED call such as `label("a,b")` was never dangerous: the depth counter
 * already covers parentheses. Only depth-0 commas can shift the slots.)
 *
 * Commas are therefore located on a blanked copy (identical offsets) and the arguments are sliced
 * out of the ORIGINAL text, so the values themselves are unchanged.
 */
function splitArgs(text) {
  const code = blankCommentsAndStrings(text);
  const args = [];
  let depth = 0;
  let start = 0;
  for (let i = 0; i < text.length; i += 1) {
    const c = code[i];
    if ('([{<'.includes(c)) depth += 1;
    else if (')]}>'.includes(c)) depth -= 1;
    else if (c === ',' && depth === 0) {
      args.push(text.slice(start, i).trim());
      start = i + 1;
    }
  }
  const tail = text.slice(start).trim();
  if (tail) args.push(tail);
  return args;
}

/** Line number (1-based) of an offset in `src`. */
function lineOf(src, offset) {
  return src.slice(0, offset).split('\n').length;
}

/**
 * A copy of `src` with comments and string/raw-string contents blanked, padded so every
 * offset keeps its position.
 *
 * R1 used to decide "is this a call or a comment?" with
 * `src.slice(lineStart, idx).includes('//')` — a purely textual, line-local heuristic. A call
 * site on a line that also contains a URL was therefore invisible: measured 2026-09-30, a
 * second pool in a non-owner file
 * (`let cfg = Config::from_url("redis://127.0.0.1:6379")…; let pool = Pool::new(cfg, None, None, None, 4);`)
 * printed `OK (one Pool::new call …)` / exit 0, while the SAME line without the URL exited 1.
 * Offsets are preserved, so the index-based call-argument parsing below still uses `src`.
 */
function blankCommentsAndStrings(src) {
  const out = src.split('');
  const blank = (from, to) => {
    for (let k = from; k < to && k < out.length; k += 1) if (out[k] !== '\n') out[k] = ' ';
  };
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    const next = src[i + 1];
    if (c === '/' && next === '/') {
      let j = i;
      while (j < src.length && src[j] !== '\n') j += 1;
      blank(i, j);
      i = j;
      continue;
    }
    if (c === '/' && next === '*') {
      let depth = 1;
      let j = i + 2;
      while (j < src.length && depth > 0) {
        if (src[j] === '/' && src[j + 1] === '*') { depth += 1; j += 2; continue; }
        if (src[j] === '*' && src[j + 1] === '/') { depth -= 1; j += 2; continue; }
        j += 1;
      }
      blank(i, j);
      i = j;
      continue;
    }
    if (c === 'r' && (next === '"' || next === '#')) {
      let hashes = 0;
      let j = i + 1;
      while (src[j] === '#') { hashes += 1; j += 1; }
      if (src[j] === '"') {
        const terminator = `"${'#'.repeat(hashes)}`;
        const end = src.indexOf(terminator, j + 1);
        const stop = end === -1 ? src.length : end;
        blank(j + 1, stop);
        i = end === -1 ? src.length : end + terminator.length;
        continue;
      }
    }
    if (c === '"') {
      let j = i + 1;
      while (j < src.length) {
        if (src[j] === '\\') { j += 2; continue; }
        if (src[j] === '"') break;
        j += 1;
      }
      blank(i + 1, j);
      i = Math.min(j + 1, src.length);
      continue;
    }
    i += 1;
  }
  return out.join('');
}

function main() {
  if (!fs.existsSync(OWNER_ABS)) {
    console.error(`[redis-pool] CANNOT VERIFY: ${OWNER} does not exist (CRP_OWNER=${OWNER_ABS})`);
    process.exit(2);
  }
  const violations = [];
  let ownerCalls = 0;
  let scanned = 0;

  for (const file of rustFiles(CRATES)) {
    const rel = path.relative(ROOT, file);
    const src = fs.readFileSync(file, 'utf8');
    // Offset-preserving copy with comments and string contents blanked, so "is this byte
    // part of a call?" is answered by the lexer rather than by "does the line contain //".
    const code = blankCommentsAndStrings(src);
    scanned += 1;
    let idx = src.indexOf('Pool::new');
    while (idx !== -1) {
      const isOwner = path.resolve(file) === path.resolve(OWNER_ABS);
      const line = lineOf(src, idx);
      // A comment (or a string) that merely mentions the constructor is not a call site.
      const isComment = code.slice(idx, idx + 'Pool::new'.length) !== 'Pool::new';
      if (!isComment) {
        if (!isOwner) {
          violations.push(
            `${rel}:${line}: \`Pool::new\` outside the single owner (${OWNER}) — build the pool ` +
              `through build_pool/build_pool_with so the command timeout, the unresponsive ` +
              `watchdog and the reconnect policy are all supplied`);
        } else {
          ownerCalls += 1;
          const args = callArguments(src, idx);
          if (!args) {
            violations.push(`${rel}:${line}: could not read the \`Pool::new\` argument list`);
          } else {
            const parts = splitArgs(args.text);
            if (parts.length < 4) {
              violations.push(
                `${rel}:${line}: \`Pool::new\` has ${parts.length} argument(s); the policy is the ` +
                  `4th — passing fewer means fred's \`None\` (never reconnect)`);
            } else if (!parts[3].startsWith('Some(')) {
              violations.push(
                `${rel}:${line}: the 4th \`Pool::new\` argument is \`${parts[3].slice(0, 40)}\`, ` +
                  `not \`Some(...)\` — \`None\` means a closed connection is never re-dialled`);
            }
          }
        }
      }
      idx = src.indexOf('Pool::new', idx + 1);
    }
    // R1b/R1c — the two ways to build a pool WITHOUT the text `Pool::new`, both of which
    // defeat the rule above and both of which leave fred's defaults in place:
    //   * fred's `Builder::build_pool(size)` calls `Pool::new(config, Some(perf), Some(conn),
    //     self.policy, size)` internally and `Builder::default()` has `policy: None`
    //     (`~/.cargo/registry/.../fred-10.1.0/src/types/builder.rs:274`), so a `.build_pool(`
    //     call site is a pool that never re-dials;
    //   * `use fred::...::Pool as P;` renames the type so `P::new(...)` is invisible to a
    //     textual search. No such import exists today — the rule exists to keep it that way.
    const isOwnerFile = path.resolve(file) === path.resolve(OWNER_ABS);
    for (const m of code.matchAll(/\.build_pool\s*\(/g)) {
      if (isOwnerFile) continue;
      violations.push(
        `${rel}:${lineOf(code, m.index)}: \`.build_pool(\` outside the single owner (${OWNER}) — ` +
          `fred's \`Builder::build_pool\` passes \`self.policy\` (\`None\` by default), so the ` +
          `pool never reconnects; build it through build_pool/build_pool_with`);
    }
    for (const m of code.matchAll(/\bPool\s+as\s+[A-Za-z_]\w*/g)) {
      violations.push(
        `${rel}:${lineOf(code, m.index)}: \`${m[0].trim()}\` renames the pool type, which hides ` +
          `every \`Pool::new\` from the single-owner rule — import it under its own name`);
    }
  }

  // Violations are reported FIRST: a tree whose owner lost its call but which has a call
  // elsewhere must fail as a violation, not dissolve into "cannot verify" (the first version
  // of this script hid exactly that case behind the exit-2 branch).
  if (violations.length) {
    for (const v of violations) console.error(`[redis-pool] VIOLATION ${v}`);
    console.error(`[redis-pool] ${violations.length} violation(s); ${scanned} file(s) scanned`);
    process.exit(1);
  }
  if (ownerCalls === 0) {
    console.error(
      `[redis-pool] CANNOT VERIFY: no \`Pool::new\` call found in ${OWNER}; the scan matched ` +
        `nothing, which is not the same as "everything is fine"`);
    process.exit(2);
  }
  console.log(
    `OK  (one \`Pool::new\` call, in ${OWNER}, with a non-\`None\` reconnect policy; ` +
      `${scanned} Rust file(s) scanned)`);
  return 0;
}

if (require.main === module) process.exit(main());
module.exports = { splitArgs, callArguments };
