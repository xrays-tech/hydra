#!/usr/bin/env node
'use strict';
/**
 * The shared algebra for "RECORDED EXCEPTIONS": a named list of things a guard deliberately does not
 * judge, each with a reason, plus the two obligations that keep such a list honest.
 *
 * Why this module exists (round 183; the metric guard joined in round 184): the same pattern had been
 * written out FIVE times, each with its own subtle variant, and each round of review found a different
 * hole in it —
 *   * `check_documented_env.cjs`   `PROSE_ONLY_OK` (names documented outside the table) and
 *                                  `READ_BY_DEPENDENCY` (a read performed by a dependency);
 *   * `check_documented_defaults.cjs` `UNVERIFIED_OK` (rows whose default cannot be compared);
 *   * `check_tenant_error_codes.cjs` `UNVERIFIED_OK` (multi-status shapes recorded on purpose);
 *   * `check_test_tails.cjs`       `UNJUDGED_WITH_EXIT_OK` (unjudged files that contain an exit);
 *   * `check_documented_metrics.cjs`  `NOT_A_METRIC` (crate/tool names that are not series — a Map of
 *     reasons since round 184, audited through the STALE direction: a record applies only while the name
 *     is either mentioned by the docs or carried by a real crate/tool/package) and `ABSENT_ON_PURPOSE`
 *     (names a doc spells as deliberately absent).
 * The obligations are the same everywhere, so they live here once:
 *
 *   1. **REPLACE, never merge.** An environment override replaces the built-in list, so a fixture tree
 *      never inherits the repository's records — and never trips the staleness check for records about
 *      files it does not contain. (Getting this wrong cost every guard a round of "nine existing
 *      assertions turned red at once".)
 *   2. **AN UNRECORDED ITEM IS A FINDING.** Anything the guard decides not to judge must be recorded
 *      with a reason, or it is a silent gap.
 *   3. **A RECORD THAT NO LONGER APPLIES IS A FINDING TOO.** A recorded decision that cannot expire is
 *      a stale claim: the file is judged now, the name has a table row, the row became comparable, the
 *      exemption's subject gained a literal read site, … Each guard supplies `applies(name)`, because
 *      only it knows what "still applies" means there.
 *
 * Deliberately NOT here: the message wording. Every guard has its own voice and its own tests assert
 * its own phrases, so this module returns STRUCTURE (`{ unrecorded, stale, used }`) and the caller
 * speaks. Same for the exit codes: a guard decides whether a finding is a DRIFT (1) or CANNOT VERIFY.
 */

/** Load a record map: `envRaw` (JSON object text) REPLACES `builtin` when it is defined. */
function records(envRaw, builtin) {
  if (envRaw === undefined) {
    // Round 194: this used to be `if (builtin instanceof Map) return builtin;` — a branch no call
    // site ever reached (all 11 passed arrays) and whose semantics differed from the fallback: it
    // ALIASED the caller's Map instead of copying it. `new Map(...)` accepts any iterable of pairs,
    // so the fallback already covers Maps correctly — with a copy, which is what "replace, never
    // merge" wants. Deleted rather than kept as an unreachable special case.
    return new Map(builtin === undefined ? [] : builtin);
  }
  const parsed = JSON.parse(envRaw);
  if (parsed === null || typeof parsed !== 'object' || Array.isArray(parsed)) {
    throw new Error('a recorded-exception override must be a JSON object of name → reason');
  }
  for (const [k, v] of Object.entries(parsed)) {
    if (typeof v !== 'string' || v.trim() === '') {
      throw new Error(`recorded exception ${k} needs a non-empty reason`);
    }
  }
  return new Map(Object.entries(parsed));
}

/**
 * One audit:
 *   * `records` — the Map in force;
 *   * `needed`  — the names that currently require a record;
 *   * `applies` — `(name) => boolean`: does this record still describe the tree?
 * Returns the three lists a guard reports: what is unrecorded, what is stale, and what is in use.
 *
 * POLARITY TRAP (measured while writing this module): `applies(name)` answers "must this record be
 * KEPT?", not "is it stale?" — the refactor of `check_tenant_error_codes.cjs` first passed the
 * complement and the guard instantly reported all three of its records as stale, which is exactly the
 * kind of inversion a shared predicate invites. Keep the name `applies` and read it as a positive.
 */
function audit({ records: recordMap, needed, applies }) {
  if (!(recordMap instanceof Map)) throw new Error('audit needs a Map of records');
  const neededSet = needed instanceof Set ? needed : new Set(needed ?? []);
  const unrecorded = [...neededSet].filter((n) => !recordMap.has(n));
  const stale = [...recordMap.keys()].filter((n) => !(applies ? applies(n) : neededSet.has(n)));
  const used = [...recordMap.keys()].filter((n) => !stale.includes(n));
  return { unrecorded, stale, used };
}

module.exports = { records, audit };
