#!/usr/bin/env node
'use strict';
/**
 * No test case may sit AFTER a top-level `process.exit` / `sys.exit`.
 *
 * Why this exists (round 137, and it had already happened once in round 132): several test scripts in
 * this repository are plain programs — they run their cases, print a summary, and end with
 * `process.exit(failures ? 1 : 0)` (or `sys.exit(main())`). Appending a new case to the END of such a
 * file is the natural thing to do and it silently does nothing: the cases never run, the suite still
 * prints PASS, and the new coverage exists only in the diff. Both times it was caught by accident
 * (a falsification probe that failed to go red), which is exactly the sort of thing a check should
 * catch on purpose.
 *
 * Strings and comments are MASKED before scanning (the same lesson the other guards learned four
 * times): a fixture inside a template literal or a docstring that happens to spell a column-zero
 * `process.exit(0);` is not an exit — measured, it made real cases look like dead code.
 *
 * The rule is deliberately precise about WHERE the program ends, and strict about what may follow:
 * only a line that STARTS with `process.exit(` / `process.kill` (JS) or `sys.exit(` (Python) at column
 * zero ends the program (an `if (…) process.exit(1)` is indented, and is normal), and after such a
 * line NOTHING can execute — so every following line that is not blank and not a comment is dead.
 *
 * Reporting only "case-shaped" statements was the first version of this guard and it was too narrow:
 * the real occurrence appended indented cases inside a `{ … }` block, which that rule did not see.
 *
 * Exit: 0 clean · 1 dead cases found · 2 cannot verify (too few suite files found, or the rule
 * recognizes a program end in too few of them — printing OK over an almost-empty set is the failure
 * mode this guard was written to avoid, so it refuses instead).
 */
const fs = require('fs');
const path = require('path');

const { maskJsLiterals } = require('./js_blank.cjs');
const { records, audit } = require('./recorded_exceptions.cjs');

/**
 * Blank Python comments and string contents, padded so offsets and line numbers are preserved
 * (the Python suites here are scanned for `sys.exit(0)` at column zero, and a docstring may contain
 * one).
 */
function maskPythonLiterals(src) {
  const out = src.split('');
  const blank = (from, to) => {
    for (let k = from; k < to && k < out.length; k += 1) if (out[k] !== '\n') out[k] = ' ';
  };
  let i = 0;
  while (i < src.length) {
    const c = src[i];
    if (c === '#') {
      let j = i;
      while (j < src.length && src[j] !== '\n') j += 1;
      blank(i, j);
      i = j;
      continue;
    }
    if (c === '"' || c === "'") {
      const triple = src.startsWith(c.repeat(3), i);
      const quote = triple ? c.repeat(3) : c;
      let j = i + quote.length;
      let closed = false;
      while (j < src.length) {
        if (src[j] === '\\') { j += 2; continue; }
        if (src.startsWith(quote, j)) { closed = true; break; }
        if (!triple && src[j] === '\n') break;
        j += 1;
      }
      if (closed) blank(i + quote.length, j);
      i = closed ? j + quote.length : Math.min(j + 1, src.length);
      continue;
    }
    i += 1;
  }
  return out.join('');
}

/** Mask literals/comments for the file's language (offsets and line numbers unchanged). */
function masked(text, file) {
  return file.endsWith('.py') ? maskPythonLiterals(text) : maskJsLiterals(text);
}

const ROOT = path.resolve(process.env.CTT_ROOT || path.join(__dirname, '..'));
const MIN_FILES = Number(process.env.CTT_MIN_FILES ?? 20);
// Floor on how many files the rule actually JUDGES (recognized program end). Measured 2026-10-01 on
// the real tree: 48 JUDGED — 6 column-zero exits, 37 Python `__main__` blocks that exit, 1 `__main__`
// block calling an always-exiting `main()` (shape 4), 2 `__main__` blocks calling `unittest.main()`
// (shape 5), 1 IIFE. Without a floor, a tree that migrates to
// a shape this guard does not model would print a comfortable OK over an empty set.
const MIN_JUDGED = Number(process.env.CTT_MIN_JUDGED ?? 30);
// Files this rule does NOT judge that nevertheless CONTAIN an exit call. Each needs a recorded reason.
//
// Why this list exists (round 176): rounds 170 and 175 each discovered BY HAND a suite that sat outside
// the rule while a case appended after its exit was dead — `integration/test_crud.py` (`__main__: main()`
// with an always-exiting `main()`) and the two `unittest.main()` suites. Nothing in the guard said
// "there is a file with an exit that I am not judging"; the OK line only reported a COUNT, which is
// exactly the kind of number that hides the one file that matters. The count now comes with an
// obligation: an unjudged file containing `process.exit(`/`sys.exit(` is a finding unless it is
// recorded here, and a record that no longer applies is a finding too.
// The list is overridable so the guard's own fixtures can build small trees without the repository's
// two records (the same discipline as the floors below) — the REPLACE semantics and the two
// obligations (unrecorded ⇒ finding, stale ⇒ finding) live in `recorded_exceptions.cjs`, which five
// guards now share (round 183).
const UNJUDGED_WITH_EXIT_OK = records(process.env.CTT_UNJUDGED_WITH_EXIT_OK, [
  [
    'scripts/check_documented_metrics.test.cjs',
    '`if (failures.length) { … process.exit(1); }` — a CONDITIONAL exit: the summary line after it ' +
      'runs on the success path, so nothing appended there is dead',
  ],
  [
    'scripts/check_redis_pool.test.cjs',
    'the same conditional-harness shape as `check_documented_metrics.test.cjs`',
  ],
]);

// Only files that could contain "case" statements: the repository's suite-shaped scripts.
const SUITE_GLOBS = [/\.test\.(cjs|js|mjs|ts)$/, /^(test_.*|check_.*)\.py$/, /\.spec\.(cjs|js|mjs|ts)$/];

function walk(dir, out = [], depth = 0) {
  if (depth > 8) return out;
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return out;
  }
  for (const e of entries) {
    if (['node_modules', '.git', 'target', '.acceptance', 'dist', 'dist-test'].includes(e.name)) continue;
    const full = path.join(dir, e.name);
    if (e.isDirectory()) walk(full, out, depth + 1);
    else if (SUITE_GLOBS.some((re) => re.test(e.name))) out.push(full);
  }
  return out;
}

/**
 * The last line of the `if __name__ == "__main__":` block starting at `i`, or null.
 *
 * The SHAPE is checked HERE, not by the caller: the block end is computed from indentation alone, so
 * a version without this check would treat ANY line that opens an indented block containing
 * `sys.exit(` as the end of the program — measured on a fixture shaped
 * `def main():\n    sys.exit(1)\n\ntest_appended()` (nothing calls `main`, so the appended case DOES
 * run) which the check-less version reported as dead code. When this check was a separate prefilter
 * in `programEnd`, inverting it left the suite green: two parts did the work and only one was tested.
 *
 * NOTE: the scan runs on the MASKED text, where string CONTENTS are blanked — so the literal
 * `"__main__"` arrives as `"        "` and matching its text here would silently miss every Python
 * drill. The quotes survive masking, which is what this matches; a fixture written inside a docstring
 * is masked away and does not match. Measured on the real tree: the quote form judges 47 suite
 * files, the literal-text form 7 of 65 (the 37 Python ends vanish), so this detail is the difference
 * between a rule that covers the tree and one that covers almost none of it.
 */
function pythonMainBlock(lines, i) {
  if (!/^if __name__ ==\s*['"]/.test(lines[i])) return null;
  const indent = lines[i].match(/^\s*/)[0].length;
  let end = i;
  for (let j = i + 1; j < lines.length; j += 1) {
    const l = lines[j];
    if (l.trim() === '') continue;
    if (l.match(/^\s*/)[0].length <= indent) break;
    end = j;
  }
  return end > i ? end : null;
}

/**
 * The last line of a column-zero IIFE whose body calls `process.exit(…)`, or null.
 *
 * `scripts/admin_ui_render.test.cjs` is written as `(async () => { … process.exit(failures ? 1 : 0);
 * })();` — a case appended after that call never runs, and the first version of this guard (which
 * only looked for a column-zero `process.exit(`) did not see it.
 *
 * Round 155: an IIFE that does NOT exit must not END the scan. The old `return … : null` stopped at
 * the FIRST column-zero IIFE, so a file with two of them — the second one exiting — was left
 * unjudged, and the OK line called its shape one "this rule does not model" while the shape was in
 * fact modelled. Measured with a two-IIFE fixture: unjudged before, `DEAD … after the IIFE that
 * exits at line 4` after this change (the single-IIFE control was already caught).
 */
function jsIifeEnd(lines) {
  for (let i = 0; i < lines.length; i += 1) {
    if (!/^\(\s*(async\s+)?\(|^\(async function|^\(function/.test(lines[i])) continue;
    for (let j = i + 1; j < lines.length; j += 1) {
      if (!/^\}\)\(\);?\s*$/.test(lines[j])) continue;
      const body = lines.slice(i, j + 1).join('\n');
      if (/process\.exit\(/.test(body)) return j + 1;
      break; // this IIFE does not exit ⇒ try the NEXT column-zero opener
    }
  }
  return null;
}

/**
 * The line after which a column-zero `def <name>(…)` body can no longer fall through, or null.
 *
 * Shape 4 (round 170): `integration/test_crud.py` ends with
 * `if __name__ == "__main__": main()` where `main()` **always** calls `sys.exit`, so a case appended
 * after the block is dead — and the previous model did not see it, because it required the `sys.exit(`
 * to appear INSIDE the `__main__` block. The old comment here justified that by saying "plain `main()`
 * does not end the program, and code after it does run" — true only for a `main()` that can RETURN,
 * which is why the decision below is conservative in exactly that direction:
 *   * the body's last statement is `sys.exit(…)` at the function's own indentation, AND
 *   * the body contains NO `return` anywhere (a `return` in any branch is a path that leaves the
 *     function normally and makes the caller's next line live again).
 * Being conservative HERE is what keeps live code from being reported as dead; being silent about the
 * always-exit shape is what let one real drill (of 65) go unjudged.
 */
function defAlwaysExits(lines, name) {
  const head = new RegExp(`^def ${name}\\(`);
  for (let i = 0; i < lines.length; i += 1) {
    if (!head.test(lines[i])) continue;
    let last = -1;
    for (let j = i + 1; j < lines.length; j += 1) {
      const l = lines[j];
      if (l.trim() === '') continue;
      if (l.match(/^\s*/)[0].length === 0) break; // back at column zero: the def is over
      last = j;
    }
    if (last === -1) return null; // empty body
    const body = lines.slice(i + 1, last + 1);
    if (body.some((l) => /(^|\s)return(\s|$)/.test(l))) return null;
    return /^\s+sys\.exit\(/.test(lines[last]) ? last + 1 : null;
  }
  return null;
}

/**
 * The line after which nothing in this file can execute, or null.
 *
 * FOUR shapes end a suite program, and the first version only knew the first one (measured
 * 2026-10-01 on the real tree, 47 suite files judged of a denominator that GROWS with every added
 * suite file (65 → 66 when round 176 added `scripts/js_blank.test.cjs`, → 67 with round 177's
 * `scripts/rust_blank.test.cjs`; both are `node:test` files with no exit of their own, so the JUDGED
 * count is the stable number and the denominator is not):
 * 6 column-zero exits, 37 Python
 * `__main__` blocks that exit, 1 `__main__` block calling an always-exiting `main()` (shape 4,
 * added round 170), 2 `__main__` blocks calling `unittest.main()` (shape 5, added round 175),
 * 1 JS IIFE):
 *   1. a column-zero `process.exit(` / `sys.exit(`;
 *   2. a Python `if __name__ == "__main__":` block WHOSE BODY EXITS — requiring the exit is what
 *      makes the appended case dead (`sys.exit(main())`, the shape 37 files use);
 *   3. a column-zero IIFE whose body exits;
 *   4. a Python `if __name__ == "__main__":` block that CALLS a function whose every path exits —
 *      `integration/test_crud.py`'s `main()` ends with `sys.exit(0)` and has no `return` at all, so
 *      the block end IS a program end and code appended after it never runs. The comment here used
 *      to claim the opposite about plain `main()` ("code after it does run"), which is true only for
 *      a `main()` that can RETURN — see `defAlwaysExits`, which is conservative in that direction.
 * The files this rule does NOT judge — MEASURED 2026-10-01 (round 194 re-measured; the numbers
 * here had drifted): the OK line below is the authority, and it reads 48 of 71 judged, 23 not judged,
 * 0 of them Python, and exactly 2 containing any exit at all (both recorded in
 * `UNJUDGED_WITH_EXIT_OK`: `check_documented_metrics.test.cjs`, `check_redis_pool.test.cjs`). The
 * earlier text said "20 files … 18 JS/CJS, 2 Python … only 3 contain any exit", and the 47/2 figures
 * in the header above were stale too. Not judging them is correct — but the reason is NOT that they
 * lack a program end (the 2 recorded ones do have one, in a conditional harness branch where
 * appended code still executes); it is that this rule cannot model those shapes, which is why the
 * recorded-exception list exists.
 */
function programEnd(lines) {
  for (let i = 0; i < lines.length; i += 1) {
    if (/^(process\.exit|sys\.exit)\(/.test(lines[i])) return { line: i + 1, how: 'top-level exit' };
  }
  for (let i = 0; i < lines.length; i += 1) {
    const end = pythonMainBlock(lines, i);
    if (end === null) continue;
    if (/sys\.exit\(/.test(lines.slice(i, end + 1).join('\n'))) {
      return { line: end + 1, how: '`if __name__ == "__main__"` block that exits' };
    }
  }
  // Shape 4: the block CALLS a function that always exits (`if __name__ == "__main__": main()`).
  for (let i = 0; i < lines.length; i += 1) {
    const end = pythonMainBlock(lines, i);
    if (end === null) continue;
    for (const call of lines.slice(i + 1, end + 1)) {
      const m = /^\s+([A-Za-z_]\w*)\(\)\s*$/.exec(call);
      if (!m) continue;
      if (defAlwaysExits(lines, m[1]) !== null) {
        return {
          line: end + 1,
          how:
            `\`if __name__ == "__main__"\` block calling \`${m[1]}()\`, whose last statement is ` +
            '`sys.exit(…)` and which has no `return`',
        };
      }
    }
  }
  // Shape 5 (round 175): the `__main__` block calls `unittest.main(...)` WITHOUT `exit=False`.
  // `unittest.main` exits by default (`exit=True` ⇒ `sys.exit(not result.wasSuccessful())`), so a case
  // appended after the block never runs — and TWO real suites end exactly this way
  // (`integration/test_error_contract.py`, `tools/hydra-py/tests/test_client.py`), which the previous
  // model left unjudged because its "always exits" rule only looked at functions DEFINED in the same
  // file. `pytest.main(...)` is deliberately NOT covered: it RETURNS an exit code and does not raise
  // SystemExit unless the caller wraps it (`sys.exit(pytest.main(...))`, which shape 2 already
  // judges). `unittest.main(exit=False)` is the documented opt-out and is left alone.
  for (let i = 0; i < lines.length; i += 1) {
    const end = pythonMainBlock(lines, i);
    if (end === null) continue;
    const block = lines.slice(i, end + 1).join('\n');
    for (const m of block.matchAll(/unittest\.main\(([^)]*)\)/g)) {
      if (/exit\s*=\s*False/.test(m[1])) continue;
      return {
        line: end + 1,
        how:
          '`if __name__ == "__main__"` block calling `unittest.main()`, which exits by default ' +
          '(the documented `exit=False` opt-out is not used here)',
      };
    }
  }
  const iife = jsIifeEnd(lines);
  if (iife !== null) return { line: iife, how: 'IIFE that exits' };
  return null;
}

/** Backwards-compatible name used by the tests: the line number, or null. */
function topLevelExit(lines) {
  const end = programEnd(lines);
  return end === null ? null : end.line;
}

/**
 * Statements (and any other code) that can never run because they sit after line `from`.
 *
 * Blank lines and comments are not code; a trailing `process.exit(...)` (some scripts end with two)
 * is part of the ending, not dead work.
 */
function deadCases(lines, from) {
  const found = [];
  for (let i = from; i < lines.length; i += 1) {
    const line = lines[i];
    const t = line.trim();
    if (t === '' || t.startsWith('//') || t.startsWith('#') || t.startsWith('/*') || t.startsWith('*')) continue;
    if (/^(process\.exit|sys\.exit)\(/.test(t)) continue;
    found.push(i + 1);
  }
  return found;
}

function main() {
  const files = walk(ROOT);
  if (files.length < MIN_FILES) {
    console.error(
      `[test-tails] CANNOT VERIFY: only ${files.length} suite file(s) found (< ${MIN_FILES}); the ` +
        `walk is probably looking in the wrong place`,
    );
    return 2;
  }
  const dead = [];
  const unjudgedWithExit = [];
  let checked = 0;
  for (const f of files) {
    let text;
    try {
      text = fs.readFileSync(f, 'utf8');
    } catch {
      continue;
    }
    const lines = masked(text, f).split('\n');
    const end = programEnd(lines);
    if (end === null) {
      // An exit call the model did NOT recognise is exactly the shape of rounds 170/175, so it must be
      // named — and either modelled or recorded — rather than buried in the "not judged" count.
      if (/(^|\s)(process|sys)\.exit\(/.test(lines.join('\n'))) {
        unjudgedWithExit.push(path.relative(ROOT, f));
      }
      continue;
    }
    checked += 1;
    const after = deadCases(lines, end.line);
    if (after.length > 0) {
      dead.push({ rel: path.relative(ROOT, f), exitLine: end.line, how: end.how, after });
    }
  }
  if (dead.length > 0) {
    for (const d of dead) {
      console.error(
        // The SHAPE is named, not assumed: a Python `__main__` block or an IIFE is not a
        // "top-level exit", and calling it one would send the reader looking for a line that
        // does not exist (this message hardcoded the wording until it was caught by its own suite).
        `[test-tails] DEAD ${d.rel}:${d.after[0]} — ${d.after.length} line(s) of code sit AFTER ` +
          `the ${d.how} at line ${d.exitLine}, so nothing there ever runs (a case appended ` +
          `there is silently dead and the suite still prints PASS)`,
      );
    }
    console.error(`[test-tails] ${dead.length} file(s) with code after their program end`);
    return 1;
  }
  const problems = [];
  const { unrecorded, stale } = audit({
    records: UNJUDGED_WITH_EXIT_OK,
    needed: unjudgedWithExit,
    applies: (rel) => unjudgedWithExit.includes(rel),
  });
  for (const rel of unrecorded) {
    problems.push(
      `[test-tails] ${rel} is NOT judged by this rule but CONTAINS an exit call: either the rule must ` +
        `model its shape (rounds 170 and 175 each found a real one — an always-exiting \`main()\`, and ` +
        `\`unittest.main()\`) or the file must be recorded in UNJUDGED_WITH_EXIT_OK with the reason it ` +
        `cannot hide dead code`,
    );
  }
  for (const rel of stale) {
    problems.push(
      `[test-tails] UNJUDGED_WITH_EXIT_OK records ${rel}, but that no longer applies (the file is ` +
        `judged now, is gone, or contains no exit call): a recorded decision that cannot expire is a ` +
        `stale claim (the \`UNVERIFIED_OK\` lesson from \`check_tenant_error_codes\`)`,
    );
  }
  if (problems.length > 0) {
    for (const prob of problems) console.error(prob);
    console.error(`[test-tails] ${problems.length} unjudged-with-exit problem(s)`);
    return 1;
  }
  // The count of JUDGED files is floored: "the rule applied to almost nothing" must not print OK.
  if (checked < MIN_JUDGED) {
    console.error(
      `[test-tails] CANNOT VERIFY: only ${checked} of ${files.length} suite file(s) have a recognized ` +
        `program end (< ${MIN_JUDGED}); the rule is not reaching the files it exists for`,
    );
    return 2;
  }
  console.log(
    `[test-tails] OK  (${checked} of ${files.length} suite file(s) have a recognized program end and ` +
      `none has code after it; ${files.length - checked} were NOT judged by this rule — appended code ` +
      `in those files is not checked here; ${UNJUDGED_WITH_EXIT_OK.size} recorded exception(s) of the ` +
      `kind that hid rounds 170/175)`,
  );
  return 0;
}

if (require.main === module) process.exit(main());

module.exports = { topLevelExit, programEnd, deadCases, maskPythonLiterals, jsIifeEnd, pythonMainBlock };
