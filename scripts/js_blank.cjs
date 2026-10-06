#!/usr/bin/env node
'use strict';
/**
 * Blanking for JavaScript/TypeScript text, shared by the guards that must not be fooled by strings.
 *
 * `maskJsLiterals(text)` returns a same-length copy with comment bodies and the CONTENTS of `'…'`,
 * `"…"` and `` `…` `` blanked (the quote characters survive, so a caller can still read a quoted
 * value out of the original at the same offsets).
 *
 * Two measured reasons it exists (2026-09-30):
 *   * `check_e2e_contracts.cjs` counted braces and searched for `created_at:` with plain regexes,
 *     so a payload whose only mention of the field was a MESSAGE
 *     (`note: 'created_at: never sent }'`) passed — the field regex matched inside the string and
 *     the `}` inside it unbalanced the counter (round 123);
 *   * `check_documented_env.cjs` accepted any `"HYDRA_X"`-shaped literal as proof that a documented
 *     knob is READ, including one inside a string in a test file, so a tree where only the guard's
 *     own fixture mentioned the name was reported as "wired" (round 124).
 *
 * Both are the same lesson the Rust guards learned with `'{'` in a char literal (see
 * `rust_blank.cjs`): match structure on blanked text, keep offsets identical.
 */

/**
 * Is the `/` at `i` a REGEX LITERAL start rather than a division?
 *
 * Standard heuristic, deliberately conservative: look at the previous non-whitespace character — if it
 * cannot end an expression (`(`, `,`, `=`, `:`, `[`, `{`, `;`, `!`, `&`, `|`, `?`, operators…) a regex
 * may start there, and a preceding KEYWORD (`return`, `typeof`, `case`, `in`, `of`, `do`, `else`,
 * `yield`, `await`, …) counts the same way. Everything else (`a / b`, `x[0] / 2`, `")" / 2`) is left as
 * ordinary code, because treating a division as a regex would be the mirror-image bug.
 */
function regexCanStartHere(src, i) {
  let k = i - 1;
  while (k >= 0 && /\s/.test(src[k])) k -= 1;
  if (k < 0) return true;
  const prev = src[k];
  if (!/[\w$)\]'"`]/.test(prev)) return true;
  if (!/[\w$]/.test(prev)) return false;
  let j = k;
  while (j >= 0 && /[\w$]/.test(src[j])) j -= 1;
  const word = src.slice(j + 1, k + 1);
  return /^(return|typeof|instanceof|in|of|new|delete|void|do|else|case|yield|await)$/.test(word);
}

/** Index of the closing unescaped `/` of the regex literal starting at `i`, or -1 if there is none on
 *  this line (a regex literal cannot span lines, so a missing close means it was a division after all). */
function endOfRegex(src, i) {
  let inClass = false;
  for (let j = i + 1; j < src.length; j += 1) {
    const c = src[j];
    if (c === '\\') { j += 1; continue; }
    if (c === '\n') return -1;
    if (inClass) { if (c === ']') inClass = false; continue; }
    if (c === '[') { inClass = true; continue; }
    if (c === '/') return j;
  }
  return -1;
}

function maskJsLiteralsReport(src) {
  const out = src.split('');
  let unterminated = false;
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
      let j = i + 2;
      while (j < src.length && !(src[j] === '*' && src[j + 1] === '/')) j += 1;
      blank(i, Math.min(j + 2, src.length));
      i = Math.min(j + 2, src.length);
      continue;
    }
    // A REGEX LITERAL may contain quotes and backticks — `/\.build_pool\(` outside the owner/` in
    // `scripts/check_redis_pool.test.cjs` is the measured case. The backtick inside it used to open a
    // TEMPLATE literal that never closed, which blanked **74% of that file** (measured: 8231 → 2167
    // non-space characters), reported `unterminated: true` for a perfectly valid file, and made
    // `check_test_tails` unable to see the file's `process.exit(` at all — a guard blinded by its own
    // masker. Regex bodies are NOT blanked (callers read patterns out of them, e.g.
    // `check_e2e_contracts`); the scan only SKIPS them so their contents cannot desynchronise it.
    if (c === '/' && regexCanStartHere(src, i)) {
      const end = endOfRegex(src, i);
      if (end !== -1) {
        i = end + 1;
        continue;
      }
    }
    if (c === '"' || c === "'" || c === '`') {
      const multiline = c === '`';
      let j = i + 1;
      let closed = false;
      while (j < src.length) {
        if (src[j] === '\\') { j += 2; continue; }
        // `"`/`'` strings cannot span a newline: an unclosed one is a syntax error, and scanning on
        // to the next quote would blank everything in between (measured 2026-09-30: a stray quote
        // moved the scan's idea of where the file's code is, and later `api()` calls disappeared).
        if (!multiline && src[j] === '\n') break;
        if (src[j] === c) { closed = true; break; }
        j += 1;
      }
      // Only a TEMPLATE can swallow the rest of a file (it may legally span lines), so only that is
      // reported upward: flagging an unclosed `'`/`"` would red-flag legitimate specs whose regex
      // literals contain an apostrophe (`/don't/`), which this scanner cannot tell from a string.
      if (!closed && multiline) unterminated = true;
      blank(i + 1, j);
      i = Math.min(j + 1, src.length);
      continue;
    }
    i += 1;
  }
  return { masked: out.join(''), unterminated };
}

/** Same, for callers that do not care whether the input was well-formed. */
function maskJsLiterals(src) {
  return maskJsLiteralsReport(src).masked;
}

module.exports = { maskJsLiterals, maskJsLiteralsReport };
