#!/usr/bin/env node
'use strict';
/**
 * Guard: a correctness-gate entry must not depend on what another entry happened to build.
 *
 * Why this exists (round 143, and it cost a RED gate): the gate script is a sequence of `gate`
 * entries run in ONE checkout, and several drills start `target/debug/hydra` through
 * `HYDRA_BIN`/`target/debug/hydra` without building it. Measured 2026-09-30: `replica fidelity` and
 * `cluster rate limits (shared+FO)` inherited the binary built by the entry above them, and a
 * `check_public_claims --measure` running in parallel — that check is
 * `cargo test -p hydra-server --features server`, which RELINKS `target/debug/hydra` WITHOUT
 * `cluster-redis`/`usage-clickhouse` — left them testing a feature-poor binary. Both reported
 * `CANNOT VERIFY … the binary lacks the cluster features` and the whole gate read RED for a reason
 * that had nothing to do with the code. The same entry order is what CI avoids: each CI step builds
 * the binary it needs (`.github/workflows/ci.yml`).
 *
 * The contract this enforces is written down by the drills THEMSELVES: a drill whose source
 * documents `cargo build -p hydra-server --features … --bin hydra` is declaring that build as a
 * precondition, and the gate entry that runs it must perform that build in the same entry.
 *
 * SCOPE, stated so this guard does not overclaim: only drills that DOCUMENT the build are judged —
 * measured 2026-09-30, 5 of the 35 drill files that read `target/debug/hydra` declare it, and the
 * other 30 say nothing about which build they need (most do not need the cluster features at all), so
 * this rule cannot and does not call those a drift. The residual — a drill that reads the binary
 * without declaring a build could still run whatever the last cargo command left behind — is
 * recorded as a queue item in the plan (§2dz) rather than silently declared fixed here.
 *
 * Reports (and only reports) two shapes:
 *   1. a judged entry runs a drill that documents the build, but does not build the binary;
 *   2. the script does not RETURN its verdict (an `echo`-terminated gate script once exited 0 while
 *      its summary said RED — a gate wired into anything would have passed forever).
 *
 * Exit: 0 clean · 1 a finding · 2 cannot verify (the gate script or its drills were not found).
 */
const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(process.env.CGE_ROOT || path.join(__dirname, '..'));
const GATE = path.join(ROOT, '.acceptance', 'round10-gate.sh');
// The documented precondition, as the drills spell it in their headers.
const BUILD_RE = /cargo build -p hydra-server[^\n]*--bin hydra/;
const MIN_ENTRIES = Number(process.env.CGE_MIN_ENTRIES ?? 40);
// Measured 2026-10-01 (round 194): 5 drills document the build precondition (and 98 entries exist). The floor
// sits BELOW the measurement with margin, so it fires when the rule stops reaching them — a floor
// equal to the measurement would redden on any legitimate consolidation (the `MIN_E2E_SPECS` lesson).
const MIN_JUDGED = Number(process.env.CGE_MIN_JUDGED ?? 4);
// Floor on how many entries run a drill that starts the PREBUILT binary. Measured 2026-09-30:
// 39 of 98 entries. The floor sits below that with margin, so it fires when the walk or the
// detection stops reaching them rather than when entries are legitimately consolidated.
const MIN_BINARY_DEPS = Number(process.env.CGE_MIN_BINARY_DEPS ?? 20);

/** The `gate "name" <command…>` entries, in order. */
function entries(text) {
  const out = [];
  const lines = text.split('\n');
  for (let i = 0; i < lines.length; i += 1) {
    const m = /^gate\s+"([^"]+)"\s+(.*)$/.exec(lines[i]);
    if (!m) continue;
    let command = m[2];
    // Entries are single-quoted `bash -c '…'` on one line; keep it simple and explicit rather than
    // clever, because a mis-parse here would silently judge the wrong text.
    while (command.endsWith('\\')) {
      command += '\n' + (lines[i + 1] ?? '');
      i += 1;
    }
    out.push({ line: i + 1, name: m[1], command });
  }
  return out;
}

/** Drill scripts under integration/ , with their source text. */
function drills() {
  const dir = path.join(ROOT, 'integration');
  const out = new Map();
  let names;
  try {
    // NOTE: this is the ONE place a directory read is enough — integration/ is flat, and a
    // recursive walk would only widen the surface (measured: the five documenting drills are all
    // directly under integration/).
    names = fs.readdirSync(dir);
  } catch {
    return out;
  }
  for (const n of names) {
    if (!n.endsWith('.py')) continue;
    out.set(n, fs.readFileSync(path.join(dir, n), 'utf8'));
  }
  return out;
}

/**
 * Does this drill start the PREBUILT binary (`target/debug/hydra`, or `HYDRA_BIN`'s default)?
 *
 * Measured 2026-10-01: 39 of the 98 gate entries run such a drill; only 9 build the binary in the
 * same entry, and before round 154 the rest ran BEFORE the gate's first build — i.e. they inherited
 * whatever the last cargo command had left behind (feature-poor in the round-143 incident, absent on
 * a fresh checkout). The gate therefore opens with the same build CI's `integration` job runs, and
 * the rule below keeps that true if the entries are ever reordered (re-measured after that change:
 * `inheritingWithNoEarlierBuild = 0`, first build entry = "build the binary under test", line 60).
 *
 * The 9/38 split is PRINTED in the OK line (round 168): the old wording ("each either building it in
 * its own entry or preceded by one that does") was true but hid the number that matters — 29 of the
 * 38 entries trust an earlier entry. There is deliberately NO floor on the split: the invariant is
 * "no reader trusts a binary this run did not build" (rule 3 below), while the split is a property of
 * how the entries are laid out, so a floor would redden on a legitimate consolidation (the
 * `MIN_E2E_SPECS` lesson). Re-measure with `node scripts/check_gate_entries.cjs` — the OK line is the
 * measurement.
 */
function readsPrebuiltBinary(text) {
  return text.includes('target", "debug", "hydra') || text.includes('target/debug/hydra');
}

function main() {
  let text;
  try {
    text = fs.readFileSync(GATE, 'utf8');
  } catch (e) {
    console.error(`[gate-entries] CANNOT VERIFY: cannot read ${path.relative(ROOT, GATE)} (${e.message})`);
    return 2;
  }
  const all = entries(text);
  if (all.length < MIN_ENTRIES) {
    console.error(
      `[gate-entries] CANNOT VERIFY: only ${all.length} gate entr(ies) parsed (< ${MIN_ENTRIES}); ` +
        `the parser is probably not reading the gate script`,
    );
    return 2;
  }
  const src = drills();
  if (src.size === 0) {
    console.error('[gate-entries] CANNOT VERIFY: no integration/*.py drill found');
    return 2;
  }

  const problems = [];
  const floors = [];
  // COMPLETENESS (round 178): a line that CALLS `gate` but did not parse as an entry is invisible to
  // every rule in this file — including the "an entry must build the binary it runs" rule that exists
  // because of the round-143 RED gate. Bash accepts shapes the parser does not (`gate 'x' …` with
  // single quotes, a leading space before `gate`), so an entry written that way would run while being
  // exempt from every rule here. Measured 2026-10-01 (round 194): the real script has 98 entries and 0 such lines,
  // i.e. this is a LATENT hole — which is exactly why it is asserted rather than assumed.
  const parsedLines = new Set(all.map((e) => e.line));
  const gateLines = text.split('\n');
  for (let i = 0; i < gateLines.length; i += 1) {
    if (!/^\s*gate\s+['"]/.test(gateLines[i])) continue;
    if (parsedLines.has(i + 1)) continue;
    problems.push(
      `line ${i + 1} looks like a gate entry (\`${gateLines[i].trim().slice(0, 70)}\`) but the parser ` +
        `did not accept it, so it would RUN while every rule in this guard ignores it (the ` +
        `build-predecessor and prebuilt-binary rules included). Write entries as ` +
        `\`gate "name" command\` starting at column zero.`,
    );
  }
  let judged = 0;
  let binaryDeps = 0;
  // Split of `binaryDeps`: how many of those entries build the binary themselves, and how many
  // inherit it from an earlier one. Reported, not floored — see the header note.
  let binarySelfBuilds = 0;
  let built = false; // has an entry built the prebuilt binary so far?
  for (const e of all) {
    for (const n of src.keys()) {
      if (!e.command.includes(n)) continue;
      const drill = src.get(n);
      if (!BUILD_RE.test(drill)) continue; // this drill does not declare a build precondition
      judged += 1;
      if (BUILD_RE.test(e.command)) continue;
      problems.push(
        `entry "${e.name}" (line ${e.line}) runs integration/${n}, which documents ` +
          `\`cargo build -p hydra-server --features … --bin hydra\` as a precondition, but the entry ` +
          `does not build it: the drill would test whatever binary another entry (or a parallel ` +
          `cargo command) last left in target/debug`,
      );
    }
    // Rule 3: a drill that starts `target/debug/hydra` must run against a binary THIS RUN built —
    // either in its own entry, or in an earlier one.
    const drillsReadBinary = [...src.keys()].filter(
      (n) => e.command.includes(n) && readsPrebuiltBinary(src.get(n)),
    );
    if (drillsReadBinary.length > 0) {
      binaryDeps += 1;
      const selfBuilds = BUILD_RE.test(e.command);
      if (selfBuilds) binarySelfBuilds += 1;
      if (!selfBuilds && !built) {
        problems.push(
          `entry "${e.name}" (line ${e.line}) runs integration/${drillsReadBinary[0]}, which starts ` +
            `\`target/debug/hydra\`, but NO entry before it builds that binary (and this one does not ` +
            `either): the drill would test whatever the last cargo command left — feature-poor, or ` +
            `missing entirely on a fresh checkout`,
        );
      }
    }
    if (BUILD_RE.test(e.command)) built = true;
  }
  if (binaryDeps < MIN_BINARY_DEPS) {
    floors.push(
      `only ${binaryDeps} entry(ies) run a drill that starts the prebuilt binary (< ${MIN_BINARY_DEPS}); ` +
        `the rule is not reaching the entries it exists for`,
    );
  }
  if (judged < MIN_JUDGED) {
    floors.push(
      `only ${judged} documented-build precondition(s) judged (< ${MIN_JUDGED}); the rule is not ` +
        `reaching the entries it exists for`,
    );
  }
  // A BUILD whose failure is swallowed: `npm run build >/dev/null 2>&1;` (or any build terminated by
  // `;` rather than `&&`) lets the rest of the entry run against a STALE artefact — measured 2026-09-30
  // on the four SDK/CLI entries, where a failing `npm run build` still let the drill run and pass
  // against `tools/*/dist` from a previous commit. The reviewer proved it with a failing `npm` shim:
  // the `;` chain continued (exit=2), the `&&` chain stopped.
  for (const e of all) {
    // NOTE the scan, not a single regex: the text between a build and its terminator contains `2>&1`,
    // whose `&` broke a character-class version of this rule (measured: the rule silently matched
    // nothing, and the probe that re-introduced a `;` stayed green).
    const buildAt = e.command.search(/(?:cargo\s+build\b[^\n]*?--bin\s+hydra|npm\s+run\s+build\b)/);
    if (buildAt !== -1) {
      const rest = e.command.slice(buildAt);
      const semi = rest.indexOf(";");
      const andand = rest.indexOf("&&");
      if (semi !== -1 && (andand === -1 || semi < andand) && /\S/.test(rest.slice(semi + 1))) {
        problems.push(
          `entry "${e.name}" (line ${e.line}) runs a BUILD terminated by \`;\` instead of \`&&\`: ` +
            `a failing build is ignored and the rest of the entry runs against a stale artefact ` +
            `(\`${rest.slice(0, 60).trim()}\`)`,
        );
      }
    }
  }
  if (!/^exit\s+"?\$overall"?/m.test(text)) {
    problems.push(
      'the gate script does not end by RETURNING its verdict (no `exit "$overall"`): a gate that ' +
        'prints RED but exits 0 passes anywhere it is wired in',
    );
  }
  // The LOG must carry a terminal marker as well: it is truncated at the start of every run, so a log
  // that merely STOPS (killed, terminal gone) used to be indistinguishable from one that finished
  // GREEN — the evidence chain could not be audited afterwards (round 163).
  if (!/GATE COMPLETE/.test(text) || !/>\s*"\$LOG"|>>\s*"\$LOG"/.test(text)) {
    problems.push(
      'the gate script does not write a terminal marker into its LOG (`GATE COMPLETE`): a truncated ' +
        'log then looks like a finished run, and the verdict cannot be audited after the fact',
    );
  }
  if (problems.length > 0 || floors.length > 0) {
    for (const p of problems) console.error(`[gate-entries] DRIFT ${p}`);
    // The floor is printed WITH the drift (round 136: a floor that exits before the findings hides
    // them), but it keeps the stronger exit code: "the rule did not run" is not "the rule found
    // something" and must not be mistaken for ordinary drift.
    for (const f of floors) console.error(`[gate-entries] CANNOT VERIFY ${f}`);
    console.error(`[gate-entries] ${problems.length} problem(s), ${floors.length} floor(s)`);
    return floors.length > 0 ? 2 : 1;
  }
  console.log(
    `[gate-entries] OK  (${all.length} entries; ${judged} drill(s) DOCUMENT a build and the entry that ` +
      `runs each builds it; ${binaryDeps} entry(ies) run a drill that starts the prebuilt binary — ` +
      `SELF-BUILD ${binarySelfBuilds} / INHERITED ${binaryDeps - binarySelfBuilds} (an inherited entry ` +
      `tests whatever an EARLIER entry of this same run built: that is the trust this rule exists to ` +
      `keep honest); the script returns its verdict)`,
  );
  return 0;
}

if (require.main === module) process.exit(main());

module.exports = { entries, BUILD_RE };
