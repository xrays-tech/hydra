#!/usr/bin/env node
'use strict';
/**
 * Every entry that runs a drill must be able to run it: build what it runs, and do not swallow the
 * verdict.
 *
 * ## Why the subject changed (2026-10-05)
 *
 * This guard used to read `.acceptance/round10-gate.sh` — the local gate script from round 10. That
 * file is **gitignored and was never tracked** (`/.acceptance/` is in `.gitignore`; `git log
 * --diff-filter=A` finds no commit that ever added it), so in CI — a fresh checkout — it does not
 * exist, the guard returned CANNOT VERIFY, and the `scripts` job ran RED for a reason that had
 * nothing to do with the code. Measured 2026-10-05: `CGE_ROOT=<empty dir> node
 * scripts/check_gate_entries.cjs` → exit 2.
 *
 * A guard whose subject is a local scratch file cannot hold anything. The subject is now
 * **`.github/workflows/ci.yml`**, which IS tracked, and the order rule got STRONGER on the way: an
 * entry that runs a drill starting `target/debug/hydra` must be preceded by a build **in its own
 * job**, because jobs run on separate runners — the old rule compared positions in one linear
 * script, which said nothing about that.
 *
 * The local gate script is still judged **when it is present** (a developer running it locally gets
 * the same rules, plus the two that belong to a shell script: it must RETURN its verdict, and it
 * must write a terminal marker into its log). When it is absent the guard says so in a note rather
 * than failing: its absence is the normal state of a fresh checkout.
 *
 * ## The findings it reports
 *
 *   1. a step runs a drill that documents `cargo build … --bin hydra` as a precondition, but the step
 *      does not build it — the drill would test whatever binary another step (or a parallel cargo
 *      command) last left behind;
 *   2. ...and a step that runs a drill starting the prebuilt binary has no build earlier IN THE SAME
 *      JOB (round 143: two entries inherited a binary that a parallel `cargo test --features server`
 *      had relinked WITHOUT the cluster features);
 *   3. a build terminated by `;` instead of `&&`: a failing build is ignored and the rest of the step
 *      runs against a stale artefact (measured 2026-09-30 on the SDK/CLI entries);
 *   4. a step that runs a drill or a guard SWALLOWS its verdict (`|| echo`, `|| true`, `; echo` after
 *      it) — the shape that let a red drill pass anywhere it was wired in;
 *   5. COMPLETENESS: a line that names an `integration/*.py` drill outside a parsed `run:` block is
 *      invisible to every rule above (a folded `run: >` scalar, a `uses:` step with a script, …), so
 *      it is a finding rather than a silent exemption.
 *
 * Exit: 0 clean · 1 a finding · 2 cannot verify (no workflow / no drills / a floor).
 *
 * Env: CGE_ROOT (repository root), CGE_WORKFLOW (workflow path), HYDRA_WIRING_GATE (local gate
 * script), CGE_MIN_ENTRIES / CGE_MIN_JUDGED / CGE_MIN_BINARY_DEPS (floors).
 */
const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(process.env.CGE_ROOT || path.join(__dirname, '..'));
const WORKFLOW = path.join(ROOT, process.env.CGE_WORKFLOW || '.github/workflows/ci.yml');
const GATE = path.join(ROOT, process.env.HYDRA_WIRING_GATE || path.join('.acceptance', 'round10-gate.sh'));
// The documented precondition, as the drills spell it in their headers.
const BUILD_RE = /cargo build -p hydra-server[^\n]*--bin hydra/;
// Floors, all measured 2026-10-05 against the tracked workflow and set BELOW the measurement with
// margin (a floor equal to the measurement reddens on any legitimate consolidation — the
// `MIN_E2E_SPECS` lesson). Measured: 129 steps with a `run:` block, 4 judged preconditions, 40 steps
// running a drill that starts the prebuilt binary. The local gate script used to supply the first
// number on its own; it no longer has to, which is the point of the rewrite.
const MIN_ENTRIES = Number(process.env.CGE_MIN_ENTRIES ?? 40);
const MIN_JUDGED = Number(process.env.CGE_MIN_JUDGED ?? 3);
const MIN_BINARY_DEPS = Number(process.env.CGE_MIN_BINARY_DEPS ?? 20);

/**
 * The steps of the tracked workflow, in file order, each `{ line, name, job, run }`.
 *
 * Deliberately a small explicit reader rather than a YAML dependency: the shapes this repository
 * uses are `- name: …` (or `- uses: …`) with an indented `run: |` block or a one-line `run: …`, and
 * a reader that silently mis-parses would judge the wrong text. Anything it cannot attribute is
 * caught by the completeness rule below, which is why that rule exists.
 */
function workflowSteps(text) {
  const lines = text.split('\n');
  const out = [];
  let job = '?';
  let inJobs = false;
  let current = null;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (/^jobs:\s*$/.test(line)) {
      inJobs = true;
      continue;
    }
    if (!inJobs) continue;
    const jm = /^  ([a-zA-Z][a-zA-Z0-9_-]*):\s*$/.exec(line);
    if (jm) {
      job = jm[1];
      current = null;
      continue;
    }
    const sm = /^      - (?:name:\s*(.*)|uses:\s*.*)$/.exec(line);
    if (sm) {
      current = { line: i + 1, name: (sm[1] || '').trim() || '(unnamed step)', job, run: [] };
      out.push(current);
      continue;
    }
    const rm = /^        run:\s*(.*)$/.exec(line);
    if (rm) {
      if (current === null) {
        current = { line: i + 1, name: '(step without a name)', job, run: [] };
        out.push(current);
      }
      const rest = rm[1].trim();
      if (rest !== '' && rest !== '|' && rest !== '>' && rest !== '|-') current.run.push(rest);
      continue;
    }
    // A continuation line of a block scalar: deeper than the `run:` key itself.
    if (current !== null && /^          \S/.test(line)) current.run.push(line.trim());
  }
  return out.filter((s) => s.run.length > 0);
}

/** The `gate "name" command` entries of the LOCAL gate script, in order. */
function gateEntries(text) {
  const out = [];
  const lines = text.split('\n');
  for (let i = 0; i < lines.length; i += 1) {
    const m = /^gate\s+"([^"]+)"\s+(.*)$/.exec(lines[i]);
    if (!m) continue;
    let command = m[2];
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

/** Does this drill start the PREBUILT binary (`target/debug/hydra`, or `HYDRA_BIN`'s default)? */
function readsPrebuiltBinary(text) {
  return text.includes('target", "debug", "hydra') || text.includes('target/debug/hydra');
}

/** The rules every entry source shares: build-what-you-run, no `;`-build, no swallowed verdict. */
function judgeEntries(entries, src, problems, counters) {
  for (const e of entries) {
    for (const n of src.keys()) {
      if (!e.command.includes(n)) continue;
      const drill = src.get(n);
      if (!BUILD_RE.test(drill)) continue;
      counters.judged += 1;
      if (BUILD_RE.test(e.command)) continue;
      problems.push(
        `entry "${e.name}" (line ${e.line}) runs integration/${n}, which documents ` +
          '`cargo build -p hydra-server --features … --bin hydra` as a precondition, but the entry ' +
          'does not build it: the drill would test whatever binary another entry (or a parallel ' +
          'cargo command) last left in target/debug',
      );
    }
    const buildAt = e.command.search(/(?:cargo\s+build\b[^\n]*?--bin\s+hydra|npm\s+run\s+build\b)/);
    if (buildAt === -1) continue;
    const rest = e.command.slice(buildAt);
    const semi = rest.indexOf(';');
    const andand = rest.indexOf('&&');
    if (semi !== -1 && (andand === -1 || semi < andand) && /\S/.test(rest.slice(semi + 1))) {
      problems.push(
        `entry "${e.name}" (line ${e.line}) runs a BUILD terminated by \`;\` instead of \`&&\`: ` +
          'a failing build is ignored and the rest of the entry runs against a stale artefact ' +
          `(\`${rest.slice(0, 60).trim()}\`)`,
      );
    }
  }
}

/**
 * Rule 2, grouped by `job`: within one job the build must come first; across jobs it cannot help,
 * because each job starts on its own runner with an empty `target/`.
 *
 * The grouping is what makes this rule work on BOTH subjects: the workflow has real jobs, and the
 * local gate script is one implicit job (`group: 'gate'`). Sharing the function is not tidiness — the
 * rule was MEASURED on the gate script (round 143: two entries inherited a binary a parallel
 * `cargo test --features server` had relinked feature-poor), so dropping it for that subject while
 * adding it for the workflow would have silently lost the case it was written for.
 */
function judgeBinaryPredecessor(entries, src, problems, counters) {
  const builtGroups = new Set();
  for (const e of entries) {
    const reads = [...src.keys()].filter((n) => e.command.includes(n) && readsPrebuiltBinary(src.get(n)));
    const selfBuilds = BUILD_RE.test(e.command);
    if (reads.length > 0) {
      counters.binaryDeps += 1;
      if (selfBuilds) counters.binarySelfBuilds += 1;
      if (!selfBuilds && !builtGroups.has(e.group)) {
        problems.push(
          `entry "${e.name}" (line ${e.line}) runs integration/${reads[0]}, which starts ` +
            '`target/debug/hydra`, but no entry before it in its ' +
            `${e.group === 'gate' ? 'gate run' : `job (\`${e.group}\`)`} builds that binary and it ` +
            'does not either: the drill would test whatever the last cargo command left — ' +
            'feature-poor, or missing entirely on a fresh checkout',
        );
      }
    }
    if (selfBuilds) builtGroups.add(e.group);
  }
}

/** Rule 4, over the shapes a step can use to swallow a failure it should report. */
function judgeVerdictSwallowing(entries, problems) {
  for (const e of entries) {
    const runsSomething = /(integration\/\w+\.py|scripts\/\w+\.c?js)/.test(e.command);
    if (!runsSomething) continue;
    const swallowed = /\|\|\s*(echo|true|:|printf)\b/.exec(e.command)
      ?? /;\s*(echo|printf)\b/.exec(e.command);
    if (swallowed === null) continue;
    problems.push(
      `entry "${e.name}" (line ${e.line}) runs a drill or a guard but SWALLOWS its verdict ` +
        `(\`${swallowed[0].trim()}\`): a red drill would pass wherever this entry is wired in`,
    );
  }
}

function main() {
  const src = drills();
  if (src.size === 0) {
    console.error('[gate-entries] CANNOT VERIFY: no integration/*.py drill found');
    return 2;
  }
  let wfText;
  try {
    wfText = fs.readFileSync(WORKFLOW, 'utf8');
  } catch (e) {
    console.error(
      `[gate-entries] CANNOT VERIFY: cannot read ${path.relative(ROOT, WORKFLOW)} (${e.message}) — ` +
        'the tracked workflow is the subject of these rules',
    );
    return 2;
  }

  const problems = [];
  const counters = { judged: 0, binaryDeps: 0, binarySelfBuilds: 0 };
  const steps = workflowSteps(wfText);
  const wfEntries = steps.map((s) => ({
    line: s.line,
    name: s.name,
    group: s.job,
    command: s.run.join('\n'),
  }));
  judgeEntries(wfEntries, src, problems, counters);
  judgeVerdictSwallowing(wfEntries, problems);

  judgeBinaryPredecessor(wfEntries, src, problems, counters);

  // Rule 5, COMPLETENESS: every drill NAMED in the workflow must be inside a parsed `run:` block.
  // A folded scalar (`run: >`) or a step using `uses:` with an inline script would otherwise run
  // while every rule above ignores it. Comment lines are documentation, not entries.
  const parsedRanges = steps.map((s) => s.line);
  const wfLines = wfText.split('\n');
  for (let i = 0; i < wfLines.length; i += 1) {
    const line = wfLines[i];
    if (/^\s*#/.test(line)) continue;
    const m = /integration\/(\w+\.py)/.exec(line);
    if (m === null || !src.has(m[1])) continue;
    const covered = parsedRanges.some((l) => i + 1 >= l && i + 1 <= l + 60);
    if (!covered) {
      problems.push(
        `line ${i + 1} names integration/${m[1]} but is not inside a parsed \`run:\` block of any ` +
          'step, so no rule here judges it (a folded `run: >` scalar, or a `uses:` step with an ' +
          'inline script, would run unjudged)',
      );
    }
  }

  // The LOCAL gate script: judged when it is present, noted when it is not. It is gitignored, so its
  // absence is the normal state of a fresh checkout — and it was pretending to be the subject of
  // this guard until 2026-10-05, which is how the guard came to FAIL IN CI while passing locally.
  let localNote = 'the local gate script is not present (gitignored): only the workflow was judged';
  let localEntries = 0;
  try {
    const gateText = fs.readFileSync(GATE, 'utf8');
    const entries = gateEntries(gateText);
    localEntries = entries.length;
    localNote = `the local gate script was judged too (${entries.length} entr(ies))`;
    for (const e of entries) e.group = 'gate';
    judgeEntries(entries, src, problems, counters);
    judgeBinaryPredecessor(entries, src, problems, counters);
    // COMPLETENESS for this subject: a line that calls `gate` in a shape the parser does not accept
    // (a single-quoted name, a leading space) still RUNS in bash while every rule here ignores it.
    const parsedLines = new Set(entries.map((e) => e.line));
    const gateLines = gateText.split('\n');
    for (let i = 0; i < gateLines.length; i += 1) {
      if (!/^\s*gate\s+['"]/.test(gateLines[i])) continue;
      if (parsedLines.has(i + 1)) continue;
      problems.push(
        `line ${i + 1} of the local gate script looks like an entry ` +
          `(\`${gateLines[i].trim().slice(0, 70)}\`) but the parser did not accept it, so it would ` +
          'RUN while every rule in this guard ignores it. Write entries as `gate "name" command` ' +
          'starting at column zero.',
      );
    }
    // The two rules that belong to a SHELL SCRIPT: it must return its verdict, and it must write a
    // terminal marker into its log (a truncated log otherwise looks like a finished green run).
    if (!/^exit\s+"?\$overall"?/m.test(gateText)) {
      problems.push(
        'the local gate script does not end by RETURNING its verdict (no `exit "$overall"`): a gate ' +
          'that prints RED but exits 0 passes anywhere it is wired in',
      );
    }
    if (!/GATE COMPLETE/.test(gateText) || !/>\s*"\$LOG"|>>\s*"\$LOG"/.test(gateText)) {
      problems.push(
        'the local gate script does not write a terminal marker into its LOG (`GATE COMPLETE`): a ' +
          'truncated log then looks like a finished run, and the verdict cannot be audited after ' +
          'the fact',
      );
    }
  } catch (e) {
    if (e.code !== 'ENOENT') throw e;
  }

  const floors = [];
  if (wfEntries.length < MIN_ENTRIES) {
    floors.push(
      `only ${wfEntries.length} workflow step(s) with a \`run:\` block parsed (< ${MIN_ENTRIES}); ` +
        'the parser is probably not reading the workflow',
    );
  }
  if (counters.binaryDeps < MIN_BINARY_DEPS) {
    floors.push(
      `only ${counters.binaryDeps} step(s) run a drill that starts the prebuilt binary ` +
        `(< ${MIN_BINARY_DEPS}); the rule is not reaching the steps it exists for`,
    );
  }
  if (counters.judged < MIN_JUDGED) {
    floors.push(
      `only ${counters.judged} documented-build precondition(s) judged (< ${MIN_JUDGED}); the rule ` +
        'is not reaching the entries it exists for',
    );
  }
  if (problems.length > 0 || floors.length > 0) {
    for (const p of problems) console.error(`[gate-entries] DRIFT ${p}`);
    for (const f of floors) console.error(`[gate-entries] CANNOT VERIFY ${f}`);
    console.error(`[gate-entries] ${problems.length} problem(s), ${floors.length} floor(s)`);
    return floors.length > 0 ? 2 : 1;
  }
  console.log(
    `[gate-entries] OK  (${wfEntries.length} workflow step(s) judged, ` +
      `${counters.judged} drill(s) DOCUMENT a build and the step that runs each builds it; ` +
      `${counters.binaryDeps} step(s) run a drill that starts the prebuilt binary — ` +
      `SELF-BUILD ${counters.binarySelfBuilds} / INHERITED-IN-JOB ` +
      `${counters.binaryDeps - counters.binarySelfBuilds}; ${localNote})`,
  );
  return 0;
}

if (require.main === module) process.exit(main());

module.exports = { gateEntries, workflowSteps, BUILD_RE };
