#!/usr/bin/env node
'use strict';
/**
 * Rust source blanking shared by the guards that must NOT be fooled by text.
 *
 * Single owner for two operations that were previously copy-pasted (or missing) in several
 * guards, each time producing the same class of bug — a guard that matches text it should not:
 *
 *   * `stripComments(text)` blanks comments AND string/raw-string/CHARACTER-literal contents,
 *     padding with spaces so every offset and every line number keeps its position.
 *   * `stripTestItems(code)` blanks whole `#[cfg(... test ...)]` items (both gating forms), so a
 *     guard can ask "does this exist in code we actually DEPLOY?" instead of "does this string
 *     appear anywhere in the tree, including in a test module?".
 *
 * Both were moved here in round 118 (from `check_source_purity.cjs`) when the two metric guards
 * were found to treat a `register_int_counter!` inside `#[cfg(test)]` as a live registration:
 * a documented series that only exists in tests is exactly as unresolvable as one that does not
 * exist, which is the failure those guards exist to prevent.
 *
 * The character-literal branch is load-bearing: `stripTestItems` matches braces to find the end
 * of a test item, so `const L: char = '{';` inside one used to make the depth never return to
 * zero — everything after that item was blanked and the following production code was never
 * scanned (measured 2026-09-30: a real `.unwrap()` in `lib.rs` plus that one line printed OK).
 */

const fs = require('fs');

function blankComments(text, blankLiterals) {
  const out = text.split('');
  let i = 0;
  const blank = (from, to) => {
    for (let k = from; k < to && k < out.length; k += 1) {
      if (out[k] !== '\n') out[k] = ' ';
    }
  };
  while (i < text.length) {
    const c = text[i];
    const next = text[i + 1];
    if (c === '/' && next === '/') {
      let j = i;
      while (j < text.length && text[j] !== '\n') j += 1;
      blank(i, j);
      i = j;
      continue;
    }
    if (c === '/' && next === '*') {
      let depth = 1;
      let j = i + 2;
      while (j < text.length && depth > 0) {
        if (text[j] === '/' && text[j + 1] === '*') { depth += 1; j += 2; continue; }
        if (text[j] === '*' && text[j + 1] === '/') { depth -= 1; j += 2; continue; }
        j += 1;
      }
      blank(i, j);
      i = j;
      continue;
    }
    if (blankLiterals && c === 'r' && (next === '"' || next === '#')) {
      let hashes = 0;
      let j = i + 1;
      while (text[j] === '#') { hashes += 1; j += 1; }
      if (text[j] === '"') {
        const terminator = `"${'#'.repeat(hashes)}`;
        const end = text.indexOf(terminator, j + 1);
        const stop = end === -1 ? text.length : end;
        blank(j + 1, stop);
        i = end === -1 ? text.length : end + terminator.length;
        continue;
      }
    }
    if (blankLiterals && c === '"') {
      let j = i + 1;
      while (j < text.length) {
        if (text[j] === '\\') { j += 2; continue; }
        if (text[j] === '"') break;
        j += 1;
      }
      blank(i + 1, j);
      i = Math.min(j + 1, text.length);
      continue;
    }
    // Character literals. `'{'` and `'}'` MUST be blanked: `stripTestItems` below
    // matches braces to find the end of a `#[cfg(test)]` item, so a single char
    // literal containing a brace threw the depth off by one and the item never
    // closed — every line after it was blanked and the production code there was
    // never scanned. Measured 2026-09-30: a real `.unwrap()` in a `lib.rs` plus
    // `const L: char = '{';` in the preceding test module made the guard print OK
    // (exit 0) instead of finding the violation. A lifetime (`&'a str`) is NOT a
    // char literal, which is why the closing quote is required before blanking.
    if (blankLiterals && c === "'") {
      let j = i + 1;
      if (text[j] === '\\') {
        j += 1;
        if (text[j] === 'u' && text[j + 1] === '{') {
          const close = text.indexOf('}', j + 2);
          j = close === -1 ? text.length : close + 1;
        } else if (text[j] === 'x') {
          j += 3; // `\xNN`
        } else {
          j += 1; // `\n`, `\t`, `\\`, `\'`, `\0`, …
        }
      } else if (j < text.length) {
        j += 1; // exactly one character
      }
      if (text[j] === "'") {
        blank(i + 1, j);
        i = j + 1;
        continue;
      }
      i += 1; // a lifetime or a lone quote: leave it alone
      continue;
    }
    i += 1;
  }
  return out.join('');
}

const CFG = /^\s*#\[cfg\((.*)\)\]\s*$/;

/** Split a cfg argument list on TOP-LEVEL commas only. */
function splitCfgArgs(text) {
  const out = [];
  let depth = 0;
  let cur = '';
  for (const c of text) {
    if (c === '(') depth += 1;
    if (c === ')') depth -= 1;
    if (c === ',' && depth === 0) {
      out.push(cur);
      cur = '';
    } else {
      cur += c;
    }
  }
  if (cur.trim() !== '') out.push(cur);
  return out;
}

/**
 * Does this cfg PREDICATE select test code?
 *
 * This used to be `/\btest\b/.test(expr)` — a substring search, not a predicate evaluation — and it
 * got the direction backwards for the one gate that says "this is NOT test code":
 * `#[cfg(not(test))]` compiles only in production builds, yet it matched, so the whole item was
 * blanked and the code inside was never scanned. Measured 2026-09-30: the same `.unwrap()` was
 * reported in a plain function (exit 1) and silently accepted inside `not(test)` (exit 0).
 *
 * Verified while writing this (and worth stating precisely, because the obvious guess is wrong):
 * a feature whose NAME contains `test` (`#[cfg(feature = "test-helpers")]`) did NOT suffer from the
 * substring test in the real pipeline — `stripComments` blanks string contents first, so the
 * predicate saw `#[cfg(feature = "            ")]` (measured). The string-stripping here therefore
 * keeps that property even if a caller ever passes raw text; it is defence in depth, not the fix.
 * The fix is the evaluation below: `all`/`any`/`not` are resolved, a bare `test` is the only truth,
 * and anything this evaluator does not understand counts as PRODUCTION (scanned, never hidden).
 */
function cfgIsTest(expr) {
  const e = expr.replace(/"(?:[^"\\]|\\.)*"/g, '""').trim();
  const m = /^(all|any|not)\s*\(([\s\S]*)\)$/.exec(e);
  if (m) {
    const args = splitCfgArgs(m[2]);
    // The question is "does this predicate HOLD ONLY when cfg(test) is set?" (i.e. does it imply
    // `test`), not "do all of its parts mention test":
    //   * `all(test, feature = "x")`  -> the item exists only under cfg(test)  => test code
    //     (this tree has four of those, so getting it wrong would start reporting them);
    //   * `any(test, feature = "x")`  -> it can also ship with the feature on => PRODUCTION
    //     (that direction is the safe one: it gets scanned);
    //   * `not(...)`                  -> never implies test (`not(test)` is production-only).
    // Ambiguity always resolves to "production", so a predicate this evaluator does not understand
    // is scanned rather than hidden.
    if (m[1] === 'not') return false;
    if (m[1] === 'all') return args.some(cfgIsTest);
    return args.length > 0 && args.every(cfgIsTest);
  }
  return /^test$/.test(e);
}

function isTestGate(line) {
  const m = CFG.exec(line);
  return m !== null && cfgIsTest(m[1]);
}

/**
 * Blanks every `#[cfg(... test ...)]`-gated item. Consumes trailing attributes,
 * then either a `;`-terminated item (`mod tests;`) or a brace-matched block.
 */
function stripTestItems(code) {
  const lines = code.split('\n');
  const out = lines.slice();
  const blankLine = (n) => { if (n < out.length) out[n] = ''; };
  let i = 0;
  while (i < lines.length) {
    if (!isTestGate(lines[i])) { i += 1; continue; }
    let j = i;
    while (j + 1 < lines.length && /^\s*(#\[|#!\[)/.test(lines[j + 1])) j += 1;
    let k = j + 1;
    let depth = 0;
    let started = false;
    while (k < lines.length) {
      const line = lines[k];
      depth += (line.match(/\{/g) || []).length - (line.match(/\}/g) || []).length;
      if (line.includes('{')) started = true;
      if (started && depth <= 0) break;
      if (!started && line.trimEnd().endsWith(';')) break;
      k += 1;
    }
    for (let n = i; n <= Math.min(k, lines.length - 1); n += 1) blankLine(n);
    i = k + 1;
  }
  return out.join('\n');
}


/** Comments AND string/character-literal contents blanked (offsets and line numbers kept). */
function stripComments(text) {
  return blankComments(text, true);
}

/** Comments blanked, literal CONTENTS kept — for guards that read names out of string literals
 *  (`register_int_counter!("hydra_x", …)`), where blanking the string would erase the evidence. */
function stripCommentsOnly(text) {
  return blankComments(text, false);
}

/**
 * The combination the metric guards need: comments gone, `#[cfg(test)]` items gone, but string
 * literals kept.
 *
 * `stripTestItems` must run on the fully blanked text (it matches braces, so a `{` inside a
 * string or a char literal would derail it), yet the RESULT must not be handed to a name scan:
 * its string contents are blanked too. So the item boundaries are computed on the blanked text
 * and then applied line-by-line to the comment-only text — a line is either inside a test item
 * (blank) or it is kept with its literals intact.
 */
function stripCommentsAndTestItems(text) {
  const codeOnly = stripCommentsOnly(text);
  const masked = stripTestItems(stripComments(text)).split('\n');
  const kept = codeOnly.split('\n');
  for (let i = 0; i < kept.length; i += 1) if ((masked[i] || '').trim() === '') kept[i] = '';
  return kept.join('\n');
}

module.exports = {
  blankCommentsAndStrings: stripComments,
  stripComments,
  stripCommentsOnly,
  stripCommentsAndTestItems,
  stripTestItems,
  isTestGate,
};
