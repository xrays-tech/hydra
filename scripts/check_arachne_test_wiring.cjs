#!/usr/bin/env node
'use strict';
/**
 * Every `crates/hydra-server/tests/arachne_*.rs` integration test MUST be named in the arachne
 * `--test` lists of BOTH the tracked CI workflow and (when present) the local gate script.
 *
 * ## Why this exists (2026-10-10, oracle F1)
 *
 * Both arachne in-process test entries enumerate `--test` targets EXPLICITLY (the list pins the
 * port-binding order). When `tests/arachne_adoption.rs` was added (plan 2026-10-10, T3 / B1–B4)
 * neither list was updated, so the four adoption tests were compiled by `--all-targets` clippy
 * but NEVER EXECUTED by any automated entry — and a GREEN gate was therefore not evidence that
 * T3's B group passed. The gap is silent by construction: a missing name is not an error for
 * `cargo test`, it is simply absence. This guard makes the absence a red finding.
 *
 * ## The rules
 *
 *   1. every file matching `crates/hydra-server/tests/arachne_*.rs` appears as a `--test <name>`
 *      token (file basename without `.rs`) in `.github/workflows/ci.yml`;
 *   2. the same holds for `.acceptance/round10-gate.sh` WHEN that file is present (it is
 *      gitignored local scratch — absent on a fresh checkout, same convention as
 *      `check_gate_entries.cjs` — so its absence is a note, never a finding);
 *   3. reverse (stale name): every `--test arachne_*` token in the workflow (and the gate script
 *      when present) names a file that exists — a renamed or deleted test file must not leave a
 *      cargo error (or worse, a silently-passing `--test` of nothing) behind.
 *
 * Exit: 0 clean · 1 a finding · 2 cannot verify (no workflow, no arachne test files).
 *
 * Env: ATW_ROOT (repository root), ATW_WORKFLOW, ATW_GATE (paths, defaults as above).
 */
const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(process.env.ATW_ROOT || path.join(__dirname, '..'));
const WORKFLOW = path.join(ROOT, process.env.ATW_WORKFLOW || path.join('.github', 'workflows', 'ci.yml'));
const GATE = path.join(ROOT, process.env.ATW_GATE || path.join('.acceptance', 'round10-gate.sh'));
const TESTS_DIR = path.join(ROOT, 'crates', 'hydra-server', 'tests');

/** Basenames (without `.rs`) of every `arachne_*.rs` integration test in the tree. */
function arachneTestFiles() {
  if (!fs.existsSync(TESTS_DIR)) return [];
  return fs
    .readdirSync(TESTS_DIR)
    .filter((f) => /^arachne_[a-z0-9_]+\.rs$/.test(f))
    .map((f) => f.replace(/\.rs$/, ''))
    .sort();
}

/** Every `--test arachne_*` token in `text`, as a Set of basenames. */
function namedTargets(text) {
  const out = new Set();
  for (const m of text.matchAll(/--test\s+(arachne_[a-z0-9_]+)/g)) out.add(m[1]);
  return out;
}

function main() {
  const expected = arachneTestFiles();
  if (expected.length === 0) {
    console.error('ATW CANNOT VERIFY: no crates/hydra-server/tests/arachne_*.rs files found');
    return 2;
  }
  if (!fs.existsSync(WORKFLOW)) {
    console.error(`ATW CANNOT VERIFY: workflow not found at ${WORKFLOW}`);
    return 2;
  }

  const findings = [];
  const ciNamed = namedTargets(fs.readFileSync(WORKFLOW, 'utf8'));
  for (const name of expected) {
    if (!ciNamed.has(name)) {
      findings.push(
        `crates/hydra-server/tests/${name}.rs is NOT named in any \`--test\` list of ${path.relative(ROOT, WORKFLOW)} `
        + `(the arachne in-process step enumerates targets explicitly — an unlisted test file is never executed)`,
      );
    }
  }
  for (const name of [...ciNamed].sort()) {
    if (!expected.includes(name)) {
      findings.push(
        `${path.relative(ROOT, WORKFLOW)} names \`--test ${name}\` but crates/hydra-server/tests/${name}.rs does not exist `
        + `(stale name: cargo errors on a missing --test target, or the file was renamed without updating the list)`,
      );
    }
  }

  if (fs.existsSync(GATE)) {
    const gateNamed = namedTargets(fs.readFileSync(GATE, 'utf8'));
    for (const name of expected) {
      if (!gateNamed.has(name)) {
        findings.push(
          `crates/hydra-server/tests/${name}.rs is NOT named in the arachne entry of ${path.relative(ROOT, GATE)} `
          + `(local gate runs the same explicit list — keep the two in step)`,
        );
      }
    }
    for (const name of [...gateNamed].sort()) {
      if (!expected.includes(name)) {
        findings.push(
          `${path.relative(ROOT, GATE)} names \`--test ${name}\` but crates/hydra-server/tests/${name}.rs does not exist`,
        );
      }
    }
  } else {
    console.log(`ATW note: ${path.relative(ROOT, GATE)} absent (normal on a fresh checkout) — judged the workflow only`);
  }

  if (findings.length > 0) {
    for (const f of findings) console.error(`ATW FINDING: ${f}`);
    return 1;
  }
  console.log(
    `ATW ok: ${expected.length} arachne test file(s) all named in the workflow's --test lists`
    + `${fs.existsSync(GATE) ? ' and the local gate script' : ''}: ${expected.join(', ')}`,
  );
  return 0;
}

process.exit(main());
