#!/usr/bin/env node
/* Wiring check: every test/checker artifact this repository ships must actually be
 * EXECUTED by the automation meant to run it.
 *
 * WHY: this session has twice added a checker or test file and forgotten to wire
 * it into CI, and the repository has a longer history of the same mistake —
 * `#[ignore]`d tests that no job ever ran (fixed in round 15), three SDK suites
 * with no job at all (fixed in round 15), a UI e2e suite added only after a
 * production regression. A file that exists but never runs is worse than no file:
 * it makes coverage look larger than it is.
 *
 * WHAT IT CHECKS (all against `.github/workflows/ci.yml`, the canonical runner):
 *   1. every test/checker artifact in `scripts` (`.test.cjs`/`.test.js`/`.test.sh`,
 *      `check_*.js`/`check_*.cjs`) is named
 *      by some CI step;
 *   2. every e2e spec under `tests/e2e` sits inside the Playwright `testDir` that
 *      `ui-e2e` job actually points at (a spec outside it never runs);
 *   3. every test file under `crates/<crate>/tests` is covered by a `cargo test`
 *      selects its crate (`-p <crate>`) or the whole workspace;
 *   4. every test file containing a REAL `#[ignore]` attribute (line-anchored, so
 *      a mention inside a comment or string does not count — grep -l would) is
 *      run by a step that passes `--ignored` for that test target.
 *
 * Floors: the artifact counts are asserted against minima, so a broken glob or a
 * renamed directory cannot turn this into a vacuous "OK".
 *
 * Usage: node scripts/check_ci_wiring.cjs
 * Exit: 0 = everything wired, 1 = an artifact nobody runs, 2 = usage/IO problem.
 */
"use strict";
const fs = require("fs");
const path = require("path");
const { records, audit } = require("./recorded_exceptions.cjs");

// `scripts/*.sh` that no artifact CI runs, recorded WITH A REASON (rule 1b). The list is REPLACED by
// `CIW_NOT_EXECUTED_OK` when that is set, so a fixture tree never inherits these records.
const NOT_EXECUTED_IN_CI_OK = records(process.env.CIW_NOT_EXECUTED_OK, [
  [
    "e2e-local.sh",
    "CI's `ui-e2e` job runs this same suite with its own inline steps and a RELEASE binary; this " +
      "script is the local/debug runner (it resolves `target/debug/hydra` itself) and the local " +
      "gate executes it. The duplicated recipe is a real cost, recorded here rather than hidden.",
  ],
]);

// The root is overridable so the checks themselves can be falsified against a
// throwaway skeleton (see `check_ci_wiring.test.cjs`): a guard whose predicates
// are never exercised is just an assertion about the author's confidence.
const ROOT = process.env.HYDRA_WIRING_ROOT
  ? path.resolve(process.env.HYDRA_WIRING_ROOT)
  : path.join(__dirname, "..");
const CI_PATH = path.join(ROOT, ".github", "workflows", "ci.yml");
// Close to the real counts (8 / 54 / 3): a floor an order of magnitude below
// the truth only catches "the whole directory vanished" and would happily let two
// of three specs disappear (a review measured exactly that gap). Raising a floor
// is a deliberate act — if an artifact is legitimately deleted, lower it in the
// same change.
// Floors, with an override each so the test suite can build small skeletons (the discipline every
// other guard in this repository follows). Values are set BELOW the real counts on purpose — a floor
// is there to catch a broken glob/walk, not to ratchet the repository — but not so far below that it
// catches nothing at all: `MIN_SCRIPTS` was 6 against 34 real artifacts, so deleting 28 checkers
// would not have tripped it (measured 2026-09-30), and `MIN_E2E_SPECS` was exactly the real count,
// so a legitimate two-into-one spec merge would fail. Round 133.
const MIN_SCRIPTS = Number(process.env.CI_WIRING_MIN_SCRIPTS ?? 20);
const MIN_CRATE_TESTS = Number(process.env.CI_WIRING_MIN_CRATE_TESTS ?? 45);
const MIN_E2E_SPECS = Number(process.env.CI_WIRING_MIN_E2E_SPECS ?? 2);
// Rule 5 discovers artifacts of several kinds; ONE global floor can be fed by a single category
// (81% of the real 47 are `integration/`), so each kind also has its own floor.
// Rule 1b (the rest of `scripts/*.sh`) has its own floor. Measured 2026-10-01: 3 such scripts exist
// (`ask_llm.sh`, `e2e-local.sh`, `load_test.sh`) — the floor sits below that with margin, because it
// exists to catch a broken glob, not to ratchet the directory.
const MIN_SHELL_SCRIPTS = Number(process.env.CI_WIRING_MIN_SHELL_SCRIPTS ?? 2);
const MIN_PER_CATEGORY = {
  integration: Number(process.env.CI_WIRING_MIN_INTEGRATION ?? 25),
  tools: Number(process.env.CI_WIRING_MIN_TOOLS ?? 4),
};

function readJsoncish() {
  if (!fs.existsSync(CI_PATH)) {
    console.error("error: no " + path.relative(ROOT, CI_PATH));
    process.exit(2);
  }
  return fs.readFileSync(CI_PATH, "utf8");
}

const ci = readJsoncish();

/* Round 149 removed `extractRunCommands` — a SECOND, divergent copy of the block-scalar parsing
 * (it joined the body with newlines while `extractSteps` joined with spaces at the time) that no rule
 * had called since the job-aware rewrite; every rule reads `extractSteps`. Two owners of "what is a
 * command" is how the two of them drift apart, so the dead one is gone rather than left as a trap. */

/** A literal `false` condition — the step (or job) NEVER runs. */
function isLiteralFalse(cond) {
  return cond !== null && cond !== undefined
    && /^(false|\$\{\{\s*false\s*\}\})$/.test(String(cond).trim().toLowerCase());
}

/** Steps as (working-directory, run) pairs, WITH their job.
 *
 * `working-directory:` may come before OR after `run:` inside a step, and a job-level
 * `defaults: {run: {working-directory: …}}` applies to every step that does not set its own.
 *
 * Round 149 added the JOB BOUNDARY. Before that, the parser split the file at ANY `- ` list item and
 * appended every non-item line to the block being built, which conflated two jobs (measured with a
 * fixture: job B's job-level `if: false` was appended to job A's LAST step, disabling a step that is
 * fine, while job B's own steps still counted as executed) and dropped the FIRST job's job-level keys
 * entirely (nothing precedes them to be appended to). Both directions lie: a disabled job whose steps
 * name an artifact looked WIRED, and an enabled step looked DISABLED.
 */
/**
 * The LAST line index of the block scalar opened at `lines[i]`, or null when that line opens none.
 *
 * ONE owner for "where does a `run: |` body end": both the step parser and the step-shape checker need
 * it, and they used to compute it separately — the shape checker cut blocks at ANY `- ` line, including
 * one INSIDE a body, which could hide a duplicate `run:` key from it.
 */
function blockScalarEnd(lines, i) {
  const m = lines[i].match(/^\s*-?\s*(?:run|[A-Za-z0-9_.-]+):\s*[|>][-+]?\d*\s*$/);
  if (!m) return null;
  const indent = lines[i].length - lines[i].replace(/^\s*/, "").length;
  let end = i;
  for (let j = i + 1; j < lines.length; j += 1) {
    const l = lines[j];
    if (l.trim() === "") continue;
    if (l.length - l.replace(/^\s*/, "").length <= indent) break;
    end = j;
  }
  return end;
}

function extractSteps(text) {
  const out = [];
  for (const { lines: block, job, jobDisabled, defaultDir } of stepBlocks(text)) {
    let dir = null;
    let run = null;
    let cond = null;
    let lenient = false;
    for (let i = 0; i < block.length; i += 1) {
      const c = block[i].match(/^\s*if:\s*(.+?)\s*$/);
      if (c) cond = c[1].replace(/^["']|["']$/g, "");
      const l = block[i].match(/^\s*continue-on-error:\s*(\S+)\s*$/);
      if (l) lenient = l[1].replace(/^["']|["']$/g, "") === "true";
      const d = block[i].match(/^\s*working-directory:\s*(\S+)\s*$/);
      if (d) dir = d[1].replace(/^["']|["']$/g, "");
      const m = block[i].match(/^\s*-?\s*run:\s*(.*)$/);
      if (!m) continue;
      const rest = m[1].trim().replace(/^["']|["']$/g, "");
      if (/^[|>][-+]?\d*$/.test(rest)) {
        // LINE-JOINED, not space-joined: a block scalar is a SHELL SCRIPT, so a `#` line comments out
        // only the rest of ITS OWN line (see `logicalCommands`). The body's EXTENT is owned by
        // `blockScalarEnd`/`stepBlocks`, not recomputed here.
        const end = blockScalarEnd(block, i);
        const body = end === null
          ? []
          : block.slice(i + 1, end + 1).filter((l) => l.trim() !== "").map((l) => l.trim());
        run = body.join("\n");
        i = end === null ? i : end;
      } else {
        run = rest;
      }
    }
    // A job-level default applies only when the step does not set its own directory.
    if (dir === null && defaultDir) dir = defaultDir;
    if (run !== null) out.push({ job, jobDisabled, dir, run, cond, lenient });
  }
  return out;
}
/**
 * The COMMANDS inside one `run:` body: one per line, with `\`-continuations merged first.
 *
 * A rule like "#[ignore]d target X is run" must be satisfied by ONE command — a step where
 * `cargo test --test usage_query` and a DIFFERENT `cargo test … -- --ignored` both appear would
 * otherwise read as "X runs with --ignored" (measured 2026-09-30: with a space-joined block both
 * substrings land in the same string, so the rule was satisfied across commands). Merging
 * continuations keeps the legitimate multi-line shape
 * (`cargo test … \` / `  --test boot_listeners -- --ignored`) working.
 */
function logicalCommands(run) {
  const out = [];
  let cur = null;
  for (const line of String(run).split("\n")) {
    const t = line.trim();
    if (cur === null) {
      if (t === "") continue;
      cur = t;
    } else {
      cur += " " + t;
    }
    if (/\\$/.test(cur)) {
      cur = cur.replace(/\\$/, "").trimEnd();
      continue;
    }
    out.push(cur);
    cur = null;
  }
  if (cur !== null) out.push(cur);
  return out;
}

/**
 * A step body that lost its `- name:`/list marker is not a YAML error a human notices: it
 * MERGES into the previous step, so the mapping ends up with two `run:` (or two `env:`) keys.
 * Measured 2026-09-30: that is exactly how the `clickhouse_sink/usage_query --ignored` step sat
 * in this workflow — GitHub rejects a workflow with duplicate keys, and a tolerant parser lets
 * the LATER `run:` win, so BOTH the auth-cache drill and those ignored suites silently stopped
 * running while every count in this file still looked right. The coverage predicate above then
 * reported "everything is executed" because the merged block's last `run:` was the cargo one.
 *
 * Block scalars (`run: |`) are consumed as a unit, so a command that merely PRINTS the text
 * `run: x` cannot be mistaken for a second key.
 */
function stepShapeProblems(text) {
  const problems = [];
  for (const { lines: block, startLine, inSteps } of stepBlocks(text)) {
    const counts = { run: 0, env: 0 };
    for (let i = 0; i < block.length; i++) {
      const m = block[i].match(/^\s*-?\s*(run|env):\s*(.*)$/);
      if (!m) continue;
      counts[m[1]] += 1;
      // Skip the block scalar's body through the SHARED helper: a `- ` line inside that body is text,
      // and cutting the block there used to hide a duplicate key from this very check.
      const end = blockScalarEnd(block, i);
      if (end !== null) i = end;
    }
    for (const key of ["run", "env"]) {
      if (counts[key] > 1) {
        problems.push(
          `the step starting at line ${startLine} has ${counts[key]} \`${key}:\` keys — a step ` +
            `body is missing its \`- name:\`/list marker (it merged into the step above; GitHub ` +
            `rejects duplicate keys, and a tolerant parser would let the last one win, silently ` +
            `dropping the other step)`
        );
      }
    }
    // A step with NEITHER `run:` nor `uses:` executes nothing. GitHub's schema requires one of them,
    // so such a file is not even parsed — the whole workflow is rejected, and nothing locally noticed
    // (round 161: `ci.yml` carried a leftover `- name: "The ClickHouse end‑to‑end tests …"` step whose
    // work is done by a later step, so the workflow over-claimed AND was structurally invalid).
    if (inSteps && counts.run === 0 && !block.some((l) => /^\s*-?\s*uses:\s*\S/.test(l))) {
      problems.push(
        `the step starting at line ${startLine} has neither \`run:\` nor \`uses:\` — it executes ` +
          `nothing, and GitHub's workflow schema requires one of the two (the whole file is rejected)`,
      );
    }
  }
  return problems;
}

/**
 * The raw step blocks of the workflow: one per list item, with `run: |` bodies kept inside the step
 * they belong to. THE single owner of "what is a block" — `extractSteps` parses these, and
 * `stepShapeProblems` looks for duplicate keys in them.
 */
function stepBlocks(text) {
  const lines = text.split(String.fromCharCode(10));
  const blocks = [];
  let cur = null;
  let blockIndent = null;
  let startLine = 1;
  let inJobs = false;
  let job = null;         // { name, disabled, defaultDir }
  let inDefaults = false; // inside a job-level `defaults:` mapping
  // A `- ` line is only a STEP inside a job's `steps:` section: `services: {…: ports: [ - 8123:8123 ]}`
  // and any other list item at depth look identical to this scanner (measured: the run/uses rule below
  // fired on `- 8123:8123` until this flag existed, while a YAML parser found no such step).
  let inSteps = false;
  const flush = () => {
    if (cur) {
      blocks.push({
        lines: cur,
        startLine,
        inSteps,
        job: job ? job.name : null,
        jobDisabled: !!(job && job.disabled),
        defaultDir: job ? job.defaultDir : null,
      });
    }
    cur = null;
    blockIndent = null;
  };
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (/^jobs:\s*$/.test(line)) {
      inJobs = true;
      continue;
    }
    if (!inJobs) continue;
    // A block-scalar BODY first: every more-indented line is shell text, including one that starts
    // with `- ` (a heredoc, a `printf '…- …'`). This must come before the job/step key handling,
    // because a body line can look like a key (`    foo: bar`).
    if (blockIndent !== null) {
      const indent = line.length - line.replace(/^\s*/, "").length;
      if (line.trim() === "" || indent > blockIndent) {
        if (cur) cur.push(line);
        continue;
      }
      blockIndent = null;
    }
    const jobKey = /^ {2}([A-Za-z0-9_.-]+):\s*$/.exec(line);
    if (jobKey) {
      flush();
      job = { name: jobKey[1], disabled: false, defaultDir: null };
      inDefaults = false;
      inSteps = false;
      continue;
    }
    const jobLevel = /^ {4}([A-Za-z0-9_.-]+):\s*(.*)$/.exec(line);
    if (jobLevel) {
      flush();
      inDefaults = jobLevel[1] === "defaults";
      inSteps = jobLevel[1] === "steps";
      // NOTE the key is captured WITHOUT its colon (`if`, not `if:`): comparing to `"if:"` here made
      // this assignment dead code and the guard still printed "everything is executed".
      if (job && jobLevel[1] === "if") job.disabled = isLiteralFalse(jobLevel[2]);
      continue;
    }
    if (inDefaults) {
      const dirLine = /^\s+working-directory:\s*(\S+)\s*$/.exec(line);
      if (dirLine && job) job.defaultDir = dirLine[1].replace(/^["']|["']$/g, "");
      continue;
    }
    if (/^\s*-\s/.test(line)) {
      flush();
      cur = [line];
      startLine = i + 1;
      const end = blockScalarEnd(lines, i);
      if (end !== null) blockIndent = line.length - line.replace(/^\s*/, "").length;
      continue;
    }
    if (cur) {
      cur.push(line);
      const end = blockScalarEnd(lines, i);
      if (end !== null) blockIndent = line.length - line.replace(/^\s*/, "").length;
    }
  }
  flush();
  return blocks;
}

const allSteps = extractSteps(ci);

let checked = 0; // artifacts examined — PRINTED in the OK line below, so it is not dead weight
const problems = stepShapeProblems(ci);
const notes = [];

/**
 * A step whose `if:` is a literal false NEVER runs, and neither does any step of a job whose JOB-level
 * `if:` is a literal false. Their artifacts are therefore not executed, and the guard must say so
 * instead of taking the step's `run:` text as proof (round 130 for the step case; round 149 added the
 * job case, which used to be attributed to the PREVIOUS job's last step — see `extractSteps`).
 */
const disabledSteps = allSteps.filter((st) => isLiteralFalse(st.cond));
for (const st of disabledSteps) {
  problems.push(`the step \`${st.run.slice(0, 70)}\` is disabled by \`if: ${st.cond}\` — nothing it names is executed`);
}
const disabledJobs = [...new Set(allSteps.filter((st) => st.jobDisabled).map((st) => st.job))];
for (const name of disabledJobs) {
  const n = allSteps.filter((st) => st.job === name && st.jobDisabled).length;
  problems.push(
    `every step of job \`${name}\` is disabled by a JOB-level \`if: false\` (${n} step(s)) — ` +
      `nothing those steps name is executed`,
  );
}
const steps = allSteps.filter((st) => !isLiteralFalse(st.cond) && !st.jobDisabled);

// COMPLETENESS (round 180): every `run:` line a human would call a step must have been parsed into one.
// A `run:` the parser never sees is a COMMAND that no rule in this file checks (crate selection,
// `--ignored`, artifact wiring) — the same "the parser silently ate the text" hole that was closed for
// the gate script in round 178, where a single-quoted entry ran while being exempt from every rule.
// Measured on the real workflow (2026-10-01): 115 `run:` lines, 115 parsed steps. The two numbers are
// COMPARED, not assumed, so a step shape this parser does not model fails loudly instead of silently.
// A `run:` key has exactly TWO homes in a workflow: a step, and the JOB-level `defaults.run`
// (`working-directory`/`shell`). The latter is not a command and must not be counted — measured by the
// round-150 fixture (`a JOB-level \`defaults.run.working-directory\` covers the tests in that
// directory`), which my first version of this check turned RED for exactly that reason.
const defaultsLines = new Set();
{
  const lines = ci.split('\n');
  for (let i = 0; i < lines.length; i += 1) {
    if (!/^\s*defaults:\s*$/.test(lines[i])) continue;
    const indent = lines[i].match(/^\s*/)[0].length;
    for (let j = i + 1; j < lines.length; j += 1) {
      if (lines[j].trim() === '') continue;
      if (lines[j].match(/^\s*/)[0].length <= indent) break;
      defaultsLines.add(j);
    }
  }
}
const runLines = ci
  .split('\n')
  .filter((l, i) => /^\s*-?\s*run:/.test(l) && !defaultsLines.has(i)).length;
if (allSteps.length !== runLines) {
  problems.push(
    `the workflow has ${runLines} \`run:\` line(s) but the parser produced ${allSteps.length} step(s): ` +
      `the difference is invisible to EVERY rule in this file (crate selection, \`--ignored\`, artifact ` +
      `wiring). Either the step's shape is not modelled by \`stepBlocks\` or the line is inside a block ` +
      `scalar — name it rather than skipping it`,
  );
}

// Commands come from the ENABLED steps (round 130) — a step disabled by `if: false`, or any step of a
// job disabled by a JOB-level `if: false` (round 149), declares no execution; using the raw text here
// let a disabled step satisfy the `scripts/` rule ("invoked") as well as rules 4/5. `extractSteps`
// keeps a block scalar as its LINES (round 145: joining them with spaces let one shell comment
// swallow every command after it), which is what the path/marker matching below expects.
const commands = steps.map((st) => stripComments(st.run));

// NOTE: the `continue-on-error` rule lives AFTER `TEST_RUNNER` is defined (below) — it used to sit
// here and threw a TDZ ReferenceError on every run with such a step (caught by the tests).

/* 1. scripts/ artifacts ---------------------------------------------------- */
const scriptsDir = path.join(ROOT, "scripts");
// A missing directory is 0 artifacts (and therefore a floor failure with a clear
// message), not an ENOENT stack trace — the falsification skeleton has no
// `scripts/` at all.
const scriptArtifacts = (fs.existsSync(scriptsDir) ? fs.readdirSync(scriptsDir) : [])
  .filter((f) => /\.test\.(cjs|js|sh)$/.test(f) || /^check_.*\.(js|cjs)$/.test(f))
  .sort();
if (scriptArtifacts.length < MIN_SCRIPTS) {
  problems.push(`only ${scriptArtifacts.length} script artifact(s) found (< ${MIN_SCRIPTS}); the glob is probably wrong`);
}
/** Drop a YAML inline comment from a command if its `#` is not inside quotes:
 *  `run: echo ok  # see scripts/x.test.cjs` must not count as executing x. */
function stripInlineComment(cmd) {
  let q = null;
  for (let i = 0; i < cmd.length; i++) {
    const c = cmd[i];
    if (q) {
      if (c === q) q = null;
      continue;
    }
    if (c === '"' || c === "'") {
      q = c;
      continue;
    }
    if (c === "#" && (i === 0 || /\s/.test(cmd[i - 1]))) return cmd.slice(0, i);
  }
  return cmd;
}

/**
 * The same, applied to EVERY line of a (possibly multi-line) `run:` body.
 *
 * Round 104 fixed rule 1 (scripts/) with `stripInlineComment`, but rules 4 and 5 kept
 * matching against the raw text, and `extractSteps` preserves YAML
 * comments and shell comments inside block scalars. Measured 2026-09-30: a brand-new
 * `integration/test_zzz_probe.py` named ONLY in a comment
 * (`# integration/test_zzz_probe.py is not wired yet`) was accepted as wired, and
 * `cargo test -p hydra-server --test usage_query   # TODO re-enable --ignored` counted as
 * running the `#[ignore]`d target — i.e. all three `#[ignore]` suites could be unwired
 * again while this guard printed OK. A comment is a statement of intent, never evidence
 * of execution.
 */
function stripComments(text) {
  return text.split("\n").map(stripInlineComment).join("\n");
}

/** Is `file` INVOKED by this command (as an argument of a runner), rather than
 *  merely mentioned? `node --test scripts/x.test.cjs` yes; `grep -q foo
 *  scripts/x.test.cjs` no — reading a file is not running it, and a review showed
 *  that a plain substring test let EXACTLY that shape (and even an inline
 *  comment) satisfy this guard. */
function invokes(command, file) {
  const esc = file.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const runner = "(?:node|nodejs|npx|bash|sh|python3|python|deno|bun|tsx)";
  if (new RegExp(`(^|[\\s;&|])${runner}([\\s]+--?[\\w.=/-]+)*[\\s]+\\S*${esc}([\\s;&|]|$)`).test(command)) {
    return true;
  }
  // A GLOB in the argument list runs every file the pattern matches, so a file matched by such an
  // argument IS invoked. Without this, consolidating thirty per-file steps into one
  // `node --test scripts/*.test.cjs` (a natural CI cleanup) would produce thirty false UNWIREDs —
  // measured limit: the real workflow names each file today, so this gap is latent. The runner
  // requirement is kept: `grep -l x scripts/*.test.cjs` READS files and still does not count.
  const globRe = new RegExp(`(^|[\\s;&|])${runner}([\\s]+--?[\\w.=/-]+)*[\\s]+([^\\s;&|]*[*?][^\\s;&|]*)`, "g");
  for (const g of command.matchAll(globRe)) {
    if (globMatchesBasename(g[3], file)) return true;
  }
  return false;
}

/** Does a shell glob's last path segment match this basename? (`*` does not cross `/`.) */
function globMatchesBasename(pattern, file) {
  const last = pattern.split("/").pop();
  const rx = last
    .split("")
    .map((ch) => {
      if (ch === "*") return "[^/]*";
      if (ch === "?") return "[^/]";
      return ch.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    })
    .join("");
  return new RegExp(`^${rx}$`).test(file);
}

for (const f of scriptArtifacts) {
  // ONLY an invocation counts: a step `name:`, a standalone comment, an inline
  // comment inside a command, or a command that merely reads the file (grep/ls)
  // all fail to execute it.
  const wired = commands.some((c) => invokes(stripInlineComment(c), f));
  if (!wired) problems.push(`scripts/${f} is never executed by ci.yml`);
}

/* 1b. the rest of`scripts/*.sh`: a script NOTHING runs ---------------------- */
/**
 * Rule 1 owns `*.test.*` and `check_*.cjs`; the remaining shell scripts under `scripts/` had NO rule
 * at all, and measured 2026-10-01 two of them were executed by nothing: `scripts/e2e-local.sh` (a
 * working browser suite — 20 passed — that only a human ever ran) and `scripts/load_test.sh` (whose
 * four judge self-checks had only ever been verified by hand). A script that exists but never runs
 * makes coverage look larger than it is, which is this guard's whole reason for existing, so the gap
 * was in the guard, not only in the tree.
 *
 * "Executed" means INVOKED (a runner's argument — `invokes()`), by EITHER automation: `ci.yml` or the
 * local gate script, because the gate is what the plan tells an operator to run and several scripts
 * are local-only by design (the gate is untracked — decision item D-5). A helper is exempt only if a
 * file that IS wired calls it (`ask_llm.sh` <- `ask_llm.test.sh`, which CI runs); the naming file must
 * itself be wired, so a mention in an orphan cannot launder another orphan.
 */
const GATE_SCRIPT = path.resolve(
  ROOT, process.env.HYDRA_WIRING_GATE ?? path.join(".acceptance", "round10-gate.sh"));
// Quotes become spaces: the gate writes `bash -c 'E2E_SKIP_BUILD=1 … bash scripts/x.sh'`, and without
// this the trailing `'` sits where `invokes()` expects a word boundary — the real entry would have
// been reported as "executed by NOTHING" (measured: the first version of this rule did exactly that).
const gateText = fs.existsSync(GATE_SCRIPT)
  ? stripComments(fs.readFileSync(GATE_SCRIPT, "utf8")).replace(/['"]/g, " ")
  : "";
const shellScripts = (fs.existsSync(scriptsDir) ? fs.readdirSync(scriptsDir) : [])
  .filter((f) => f.endsWith(".sh") && !/\.test\.sh$/.test(f))
  .sort();
function invokedByAutomation(base) {
  if (commands.some((c) => invokes(stripInlineComment(c).replace(/['"]/g, " "), base))) return true;
  return invokes(gateText, base);
}
/**
 * Does **ci.yml** invoke this script? Deliberately narrower than {@link invokedByAutomation}, which
 * also counts the local gate: rule 1b's records are about the CANONICAL runner, so a record must
 * survive a developer machine (where the gate runs the script and the rule needs no record) exactly
 * as it survives a runner (where there is no gate file and the rule would otherwise red).
 */
function invokedByCiYml(base) {
  return commands.some((c) => invokes(stripInlineComment(c).replace(/['"]/g, " "), base));
}
/**
 * Comment stripping that knows the file type. A mention inside a COMMENT is not a call — and this
 * rule's first version proved how sharp that edge is: `scripts/e2e-local.sh` was reported as wired
 * because THIS FILE's own explanatory comment names it, so the guard documented its own hole as the
 * evidence that the hole was closed (measured: with the gate script pointed at a nonexistent path,
 * the rule still passed).
 */
function stripFileComments(file, text) {
  if (/\.(js|cjs|mjs|ts)$/.test(file)) {
    return text
      .replace(/\/\*[\s\S]*?\*\//g, " ")
      .split("\n")
      .map((l) => l.replace(/(^|[^:])\/\/.*$/, "$1"))
      .join("\n");
  }
  return stripComments(text);
}

/** A wired file that NAMES `base` (the helper case). */
function helperCaller(base) {
  const files = [];
  const skip = /^(node_modules|target|dist|\.git|pw-browsers|tmp-.*)$/;
  (function walk(dir, depth) {
    if (depth > 3 || !fs.existsSync(dir)) return;
    for (const ent of fs.readdirSync(dir, { withFileTypes: true })) {
      if (skip.test(ent.name)) continue;
      const p = path.join(dir, ent.name);
      if (ent.isDirectory()) walk(p, depth + 1);
      else if (/\.(sh|cjs|js|mjs|py|ts|yml|yaml)$/.test(ent.name) && ent.name !== base) files.push(p);
    }
  })(path.join(ROOT, "scripts"), 0);
  for (const dir of ["integration", "tools", ".github"]) {
    (function walk(d, depth) {
      if (depth > 3 || !fs.existsSync(d)) return;
      for (const ent of fs.readdirSync(d, { withFileTypes: true })) {
        if (skip.test(ent.name)) continue;
        const p = path.join(d, ent.name);
        if (ent.isDirectory()) walk(p, depth + 1);
        else if (/\.(sh|cjs|js|mjs|py|ts|yml|yaml)$/.test(ent.name) && ent.name !== base) files.push(p);
      }
    })(path.join(ROOT, dir), 0);
  }
  for (const file of files) {
    // Comments stripped, and the mention must be a CALL, not a bare name in prose: either an
    // invocation (`invokes`: a runner's argument) or a `scripts/<name>` path — the shape
    // `ask_llm.test.sh` uses (`SCRIPT="scripts/ask_llm.sh"`, then `bash "$SCRIPT"`), which no
    // invocation-shaped scan can see through without a mini dataflow analysis.
    const text = stripFileComments(file, fs.readFileSync(file, "utf8"));
    if (!invokes(text.replace(/['"]/g, " "), base) && !text.includes(`scripts/${base}`)) continue;
    const own = path.basename(file);
    if (invokedByAutomation(own) || scriptArtifacts.includes(own)) return file;
  }
  return null;
}
const helperReached = [];
const orphanScripts = [];
for (const f of shellScripts) {
  if (invokedByAutomation(f)) continue;
  const caller = helperCaller(f);
  if (caller) {
    helperReached.push(`${f} <- ${path.relative(ROOT, caller)}`);
    continue;
  }
  orphanScripts.push(f);
}
// RULE 1b's recorded exceptions, through the shared algebra (`recorded_exceptions.cjs`): a record is
// a CLAIM WITH A REASON and it EXPIRES — the moment a wired artifact invokes the script, keeping the
// record would be the stale claim the algebra exists to catch.
//
// WHY A RECORD IS NEEDED AT ALL (measured 2026-10-07, the first CI run ever to reach this rule):
// the local gate executes `scripts/e2e-local.sh`, so on a developer machine the rule sees it wired
// and says nothing. On a runner `.acceptance/round10-gate.sh` does not exist — it is gitignored and
// has never been tracked — so ci.yml alone decides, and ci.yml does not call it. One tree, two
// verdicts, one per machine. The record states the CI verdict instead of leaving it to whichever
// files happen to be on the box.
const orphanAudit = audit({
  records: NOT_EXECUTED_IN_CI_OK,
  needed: orphanScripts,
  // Positive polarity ("keep this record"): it still describes the tree while **ci.yml** does not
  // invoke the script. Judged against ci.yml alone, so hiding the local gate file (a runner) or
  // having it (a developer) cannot flip the verdict for the same tree.
  applies: (name) => !invokedByCiYml(name),
});
for (const f of orphanAudit.unrecorded) {
  problems.push(
    `scripts/${f} is executed by NOTHING — not by ci.yml, not by ` +
      `${path.relative(ROOT, GATE_SCRIPT)} — and no wired file calls it as a helper`,
  );
}
for (const f of orphanAudit.stale) {
  problems.push(
    `scripts/${f} is RECORDED as not-executed-in-CI, but ci.yml invokes it now — remove the record ` +
      `(NOT_EXECUTED_IN_CI_OK) instead of leaving a claim that no longer applies`,
  );
}
for (const f of orphanAudit.used) {
  console.log(`note  scripts/${f} is not executed by CI, recorded on purpose: ${NOT_EXECUTED_IN_CI_OK.get(f)}`);
}
if (shellScripts.length < MIN_SHELL_SCRIPTS) {
  problems.push(
    `only ${shellScripts.length} \`scripts/*.sh\` file(s) examined (< ${MIN_SHELL_SCRIPTS}); the glob ` +
      `is probably wrong`,
  );
}

/* 2. e2e specs vs the Playwright testDir ----------------------------------- */
const specDir = path.join(ROOT, "tests", "e2e");
const specs = fs.existsSync(specDir) ? fs.readdirSync(specDir).filter((f) => f.endsWith(".spec.cjs")).sort() : [];
if (specs.length < MIN_E2E_SPECS) problems.push(`no e2e specs found under ${path.relative(ROOT, specDir)}`);
const cfgPath = path.join(ROOT, "playwright.config.cjs");
let testDir = null;
if (fs.existsSync(cfgPath)) {
  const m = fs.readFileSync(cfgPath, "utf8").match(/testDir:\s*['"]([^'"]+)['"]/);
  if (m) testDir = path.normalize(m[1]);
}
if (!testDir) problems.push("playwright.config.cjs declares no testDir, so what the ui-e2e job runs is undefined");
else if (path.normalize(path.relative(ROOT, specDir)) !== testDir) {
  problems.push(`e2e specs live in ${path.relative(ROOT, specDir)} but playwright testDir is ${testDir}`);
}
if (!commands.some((c) => c.includes("playwright test"))) problems.push("ci.yml never runs `playwright test`");

/* 3+4. crate test files, and the `#[ignore]` ones --------------------------- */
const cratesDir = path.join(ROOT, "crates");
const crateTests = [];
for (const crate of fs.existsSync(cratesDir) ? fs.readdirSync(cratesDir) : []) {
  const dir = path.join(cratesDir, crate, "tests");
  if (!fs.existsSync(dir)) continue;
  for (const f of fs.readdirSync(dir)) {
    if (!f.endsWith(".rs")) continue;
    const rel = `crates/${crate}/tests/${f}`;
    const src = fs.readFileSync(path.join(dir, f), "utf8");
    // Line-anchored: an attribute is the first thing on its line. A mention in a
    // comment or string (test_attribute_integrity.rs documents `#[ignore]`) must
    // NOT count — `grep -l '#[ignore]'` gets that wrong.
    // Any ATTRIBUTE line mentioning `ignore`, so `#[ignore]`,
    // `#[ignore = "reason"]`, `#[should_panic] #[ignore]` on one line and
    // `#[cfg_attr(feature = "x", ignore)]` are all caught. A `//` comment is not
    // an attribute line, which is what keeps discussions OF the attribute from
    // counting — `grep '#[ignore]'` gets both directions wrong.
    const ignored = src.split("\n").some((l) => /^\s*#\[.*\bignore\b/.test(l));
    crateTests.push({ crate, target: f.replace(/\.rs$/, ""), rel, ignored });
  }
}
if (crateTests.length < MIN_CRATE_TESTS) {
  problems.push(`only ${crateTests.length} crate test file(s) found (< ${MIN_CRATE_TESTS}); the glob is probably wrong`);
}

// 4b. `#[ignore]`d tests that live INSIDE a crate's `src/` — outside every rule above.
//
// The rules in this file enumerate `crates/*/tests/*.rs`, so a `#[ignore]` test written in a `src/`
// module is invisible to the floor: nothing requires a CI step to run it, and nothing says so. That
// was measured on 2026-10-07, when the TDengine backend's live test (which needs a container) landed
// in `src/usage/backends/tdengine/mod.rs` and the guard stayed green with nothing running it.
//
// Making it a FAILURE would demand a service container for every such test, which is a real cost this
// guard is not entitled to decide. Making it a NOTE is not decoration either: it is printed on every
// run, so "this ignore test is not covered" cannot be an accident of nobody looking.
const libIgnored = [];
for (const crate of fs.existsSync(cratesDir) ? fs.readdirSync(cratesDir) : []) {
  const srcDir = path.join(cratesDir, crate, "src");
  const walk = (dir) => {
    if (!fs.existsSync(dir)) return;
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, e.name);
      if (e.isDirectory()) walk(full);
      else if (e.name.endsWith(".rs")) {
        const text = fs.readFileSync(full, "utf8");
        if (text.split("\n").some((l) => /^\s*#\[.*\bignore\b/.test(l))) {
          libIgnored.push(path.relative(ROOT, full).split(path.sep).join("/"));
        }
      }
    }
  };
  walk(srcDir);
}
if (libIgnored.length) {
  notes.push(
    `${libIgnored.length} crate-internal test file(s) contain a real #[ignore] and are OUTSIDE every rule here: ` +
      libIgnored.join(", ") +
      " — no step is required to run them (they are noted, not judged: requiring a service container for them is a cost this guard does not decide)",
  );
}
const cargoTestCmds = commands.filter((c) => /\bcargo test\b/.test(c));
if (!cargoTestCmds.length) problems.push("ci.yml never runs `cargo test`");
// Rules 3 and 4 are answered per COMMAND, not per step: selecting a crate and running a target with
// `--ignored` must happen in the same invocation, otherwise a step that merely mentions both (two
// different commands) counts as wiring the target up.
const cargoTestInvocations = cargoTestCmds.flatMap((c) => logicalCommands(c)).filter((c) => /\bcargo test\b/.test(c));

/**
 * Does this command SELECT this crate? Whole words only.
 *
 * The old `command.includes("-p " + crate)` was satisfied by `-p hydra-server-extra` — and by
 * `--features hydra-core-x` for `hydra-core` — so a CI edit that stopped testing a crate (or a typo in
 * the package name) still read as "this crate is selected" (round 150). `--package <crate>` and
 * `-p=<crate>` are the same flag and are accepted; `--workspace` selects everything.
 */
function selectsCrate(command, crate) {
  const esc = crate.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  if (/(^|\s)--workspace(\s|$)/.test(command)) return true;
  return new RegExp(`(^|\\s)(?:-p|--package)(?:=|\\s+)${esc}(?![A-Za-z0-9_-])`).test(command);
}

for (const t of crateTests) {
  const covered = cargoTestInvocations.some((c) => selectsCrate(c, t.crate));
  if (!covered) problems.push(`${t.rel} belongs to crate ${t.crate}, which no \`cargo test\` command selects`);
}
const ignoredTargets = crateTests.filter((t) => t.ignored);
for (const t of ignoredTargets) {
  // Whole-word, like `selectsCrate` above: `c.includes("--test " + target)` was satisfied by a
  // LONGER target name (`--test usage_query` matches `--test usage_query_wire`), so a renamed
  // target would keep its `#[ignore]`d tests "wired" forever (round 156).
  const targetFlag = new RegExp(`(^|\\s)--test(?:=|\\s+)${t.target.replace(/[.*+?^${}()|[\\]\\]/g, "\\$&")}(?![A-Za-z0-9_-])`);
  const wired = cargoTestInvocations.some((c) => targetFlag.test(c) && /(^|\s)--ignored(\s|$)/.test(c));
  if (!wired) problems.push(`${t.rel} contains #[ignore] tests but no step runs \`--test ${t.target} … --ignored\``);
}

/* 5. test artifacts EVERYWHERE else ----------------------------------------
 * The first version of this guard only knew `scripts/` and `crates/<crate>/tests`,
 * which is why three real suites went unnoticed: `integration/test_crud.py` (116
 * assertions across the whole admin REST surface), `integration/e2e_proxy_test.py`
 * (mock auth + mock LLM + the proxy path) and `tools/hydra-cli/test/client.test.ts`
 * (a SHIPPED npm package's own suite). All three were green when first run — nobody
 * had ever run them. */
// Floor on how many directories the discovery walk VISITS (the real tree visits 47): with no
// lower bound, moving the tree or breaking the walk would report "everything is executed" over an
// empty set.
const MIN_VISITED_DIRS = Number(process.env.CI_WIRING_MIN_DIRS ?? 10);

const TEST_GLOBS = [/\.test\.(ts|js|cjs|mjs)$/, /^(test_.*|.*_test)\.py$/, /^check_.*\.py$/, /_test\.go$/, /\.spec\.(ts|js|cjs)$/];
/**
 * Directories skipped at ANY depth: build caches and VCS metadata.
 *
 * The previous single `SKIP_DIRS` was matched by NAME at every depth, so a directory called
 * `scripts` anywhere (`.github/scripts/`, `tools/x/scripts/`) was skipped along with the top-level
 * `scripts/` that rule 1 owns — measured 2026-09-30 with `.github/scripts/probe.test.cjs`, which the
 * walk never reached. Name-based skipping is the wrong tool for a directory that has a rule BECAUSE
 * OF ITS LOCATION.
 */
const SKIP_ANYWHERE = new Set(["target", ".git", "node_modules", ".acceptance", ".cargo-cache", "dist", "dist-test"]);
// Skipped only at the TOP level: `scripts/` is rule 1's own directory, and the other two must not be
// read (secrets) or are prose.
const SKIP_TOP_LEVEL = new Set(["scripts", "secure", "dev-docs"]);
const discovered = [];
const visitedDirs = new Set();
(function walk(dir) {
  visitedDirs.add(path.relative(ROOT, dir) || ".");
  for (const ent of fs.readdirSync(dir, { withFileTypes: true })) {
    if (ent.isDirectory()) {
      if (SKIP_ANYWHERE.has(ent.name)) continue;
      // Dot-directories are skipped EXCEPT `.github`, which holds helper scripts that must be
      // executed like any other artifact (round 133; `.git` is in SKIP_ANYWHERE).
      if (ent.name.startsWith(".") && ent.name !== ".github") continue;
      const rel = path.relative(ROOT, path.join(dir, ent.name));
      if (!rel.includes(path.sep) && SKIP_TOP_LEVEL.has(ent.name)) continue;
      // Directories with their own rule for SOME artifact kinds: `tests/e2e` (playwright),
      // `scripts` (rule 1). `crates` and `.github` are walked as well — rules 3/4 only cover
      // `crates/<crate>/tests/*.rs` and only ci.yml is read from `.github`, so a `*.test.cjs` under
      // `crates/` or a helper under `.github/scripts/` used to be completely invisible (latent: the
      // tree has none of those today). `.rs` files never match TEST_GLOBS, so crate targets are
      // still owned by rules 3/4.
      if (rel === "tests/e2e" || rel === "scripts") continue;
      walk(path.join(dir, ent.name));
      continue;
    }
    if (TEST_GLOBS.some((re) => re.test(ent.name))) discovered.push(path.join(dir, ent.name));
  }
})(ROOT);
if (visitedDirs.size < MIN_VISITED_DIRS) {
  problems.push(
    `the discovery walk visited only ${visitedDirs.size} director(ies) (< ${MIN_VISITED_DIRS}); ` +
      `a renamed/moved tree would make this guard silently discover nothing`,
  );
}
const TEST_RUNNER = /(npm ci|npm test|go test|python3? -m unittest|pytest|node --test|playwright test|run-crud-local|cargo test)/;

/**
 * A step that runs a test artifact with `continue-on-error: true` can fail without failing the job,
 * so it is not a gate. Reported as a problem: the artifact is "run", its failure blocks nothing.
 * (Round 130; this block must stay after `TEST_RUNNER`.)
 */
for (const st of steps.filter((x) => x.lenient)) {
  if (TEST_RUNNER.test(st.run)) {
    problems.push(`the step \`${st.run.slice(0, 70)}\` runs tests but sets \`continue-on-error: true\` — its failure cannot block anything, so it is not a gate`);
  } else {
    notes.push(`the step \`${st.run.slice(0, 60)}\` sets \`continue-on-error: true\` (no test runner detected in it)`);
  }
}

/**
 * Files a CI step names outright, plus the SCRIPTS those steps execute.
 *
 * The previous predicate accepted `st.run.includes(dir + "/")` — "its directory, via a
 * script" — which made this check **vacuous for every file under `integration/`**: any step
 * containing the string `integration/` (and there are dozens) marked every integration file as
 * wired, so a brand-new drill dropped in there was reported as "everything is executed"
 * without being wired at all. Measured 2026-09-30 with `integration/test_zzz_dummy_probe.py`:
 * the guard printed OK. The rule is now a real chain — a file is wired only if CI names it, a
 * script CI runs names it, or a wired Python file in the same directory imports it.
 */
function namedInSteps(base) {
  return steps.some((st) => stripComments(st.run).includes(base));
}

/** Paths of scripts (`.sh`/`.py`) that a CI step executes and that exist on disk. */
function executedScripts() {
  const found = new Set();
  for (const st of steps) {
    for (const m of stripComments(st.run).matchAll(/[\w./-]+\.(?:sh|py)\b/g)) {
      const rel = m[0].replace(/^\.\//, "");
      const full = path.join(ROOT, rel);
      if (fs.existsSync(full)) found.add(full);
    }
  }
  return [...found];
}

/**
 * Wired artifacts, computed to a FIXED POINT because the relations chain:
 * CI → `integration/run-crud-local.sh` → `check_error_contract.py` → `check_api_docs.py`,
 * and `e2e_proxy_test.py` → `mock_auth.py` / `mock_llm.py`.
 */
function wiredSet(discovered) {
  const wired = new Set(discovered.filter((f) => namedInSteps(path.basename(f))));
  const scripts = executedScripts();
  // COMMENTS STRIPPED, like every other piece of text this guard reads: this file's own rule is
  // "a comment is a statement of intent, never evidence of execution" (see `stripComments`), and the
  // script chain was the one place it was not applied. Measured 2026-09-30 with a runner script whose
  // only mention of `integration/test_new.py` was `# TODO: … is still unwired`: the artifact counted
  // as EXECUTED (the TODO saying it is not wired became the proof that it is), and deleting that
  // comment — changing nothing else — turned the guard red.
  const scriptText = new Map(scripts.map((f) => [f, stripComments(fs.readFileSync(f, "utf8"))]));
  let changed = true;
  while (changed) {
    changed = false;
    for (const file of discovered) {
      if (wired.has(file)) continue;
      const base = path.basename(file);
      const module = base.replace(/\.py$/, "");
      // (a) executed by a script that CI runs
      for (const [, text] of scriptText) {
        if (text.includes(base)) {
          wired.add(file);
          changed = true;
          break;
        }
      }
      if (wired.has(file)) continue;
      // (b) imported by a wired Python file in the same directory (one hop per pass)
      for (const other of discovered) {
        if (!wired.has(other) || path.dirname(other) !== path.dirname(file)) continue;
        const text = fs.readFileSync(other, "utf8");
        if (new RegExp(`(^|\\n)\\s*(import\\s+${module}\\b|from\\s+${module}\\s+import)`).test(text)) {
          wired.add(file);
          changed = true;
          break;
        }
      }
    }
  }
  return wired;
}

const coveredBy = (file) => {
  const rel = path.relative(ROOT, file);
  const dir = path.dirname(rel);
  return (
    wiredSet(discovered).has(file) ||
    // A step may also run a runner from a working-directory that owns the file.
    steps.some((st) => st.dir && (st.dir === dir || dir.startsWith(st.dir + "/")) && TEST_RUNNER.test(st.run))
  );
};
const MIN_OTHER_TESTS = Number(process.env.CI_WIRING_MIN_OTHER_TESTS ?? 4);
// Per-category floors: a single global count cannot notice one KIND of artifact disappearing.
const categoryCounts = new Map();
for (const f of discovered) {
  const rel = path.relative(ROOT, f);
  const category = rel.split(path.sep)[0];
  categoryCounts.set(category, (categoryCounts.get(category) ?? 0) + 1);
}
for (const [category, floor] of Object.entries(MIN_PER_CATEGORY)) {
  const count = categoryCounts.get(category) ?? 0;
  if (count < floor) {
    problems.push(
      `only ${count} test artifact(s) under \`${category}/\` (< ${floor}); either that whole ` +
        `category was moved/renamed, or the discovery walk stopped reaching it`,
    );
  }
}
if (discovered.length < MIN_OTHER_TESTS) {
  problems.push(`only ${discovered.length} test artifact(s) outside scripts/ and crates/<crate>/tests (< ${MIN_OTHER_TESTS}); the globs are probably wrong`);
}
for (const f of discovered) {
  checked += 1;
  if (!coveredBy(f)) {
    problems.push(
      `${path.relative(ROOT, f)} is never executed by ci.yml ` +
        `(no step names it, no script a step runs names it, and no wired file in its ` +
        `directory imports it)`
    );
  }
}
if (discovered.length >= MIN_OTHER_TESTS) {
  notes.push(`${discovered.length} test artifact(s) outside scripts/ and crates/<crate>/tests: ${discovered.map((f) => path.relative(ROOT, f)).sort().join(", ")}`);
}
/* Report ------------------------------------------------------------------- */
notes.push(`${scriptArtifacts.length} script artifact(s): ${scriptArtifacts.join(", ")}`);
notes.push(
  `${shellScripts.length} \`scripts/*.sh\` file(s) beyond rule 1, ` +
    `${helperReached.length} reached only as a helper of a wired file` +
    (helperReached.length ? `: ${helperReached.join(", ")}` : ""),
);
notes.push(`${specs.length} e2e spec(s) under testDir ${testDir || "(none)"}`);
notes.push(`${crateTests.length} crate test file(s) in ${new Set(crateTests.map((t) => t.crate)).size} crate(s)`);
notes.push(`${ignoredTargets.length} file(s) with real #[ignore] tests: ${ignoredTargets.map((t) => t.target).join(", ") || "(none)"}`);

if (problems.length) {
  for (const p of problems) console.error("UNWIRED  " + p);
  console.error(`${problems.length} wiring issue(s)`);
  process.exit(1);
}
// The examined count is PRINTED, not just accumulated: it used to be a dead variable whose
// comment claimed it was the protection against a broken glob passing silently (round 156).
// Round 194 (adversarial review): this line said `${checked} artifact(s) examined`, and `checked`
// only counts `discovered` — the test artifacts OUTSIDE `scripts/` and `crates/<crate>/tests`. Read
// after "everything is executed" it looked like the total, while the same run's notes accounted for
// five more classes (script artifacts, `scripts/*.sh`, e2e specs, crate test files, `#[ignore]`
// targets) — measured then: 48 vs 40 + 3 + 3 + 54 + 3. The number is now named for what it counts,
// and the total is printed next to it.
const judgedTotal = checked + scriptArtifacts.length + shellScripts.length + helperReached.length
  + specs.length + crateTests.length;
console.log(`OK  (everything is executed — ${checked} test artifact(s) outside scripts/ and crates/<crate>/tests, ${judgedTotal} artifact(s) judged in total across ${5 + 1} classes)`);
for (const n of notes) console.log("    " + n);
