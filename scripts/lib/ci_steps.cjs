'use strict';
/**
 * Single owner of "what is a workflow step" for the CI guards.
 *
 * `check_ci_wiring.cjs` and `check_gate_entries.cjs` both read `.github/workflows/ci.yml`
 * and both used to carry their own reader. The two readers disagreed — the wiring guard's
 * `stepBlocks` models job boundaries, `defaults.run`, `if:`/`continue-on-error`, and block
 * scalars, while the gate-entries reader only recognised six-space `- name:` steps and
 * eight-space `run:` keys. Two owners of the same question is how one of them silently
 * stops seeing a shape the other already handles, so the enumeration lives here.
 *
 * Deliberately dependency-free (no npm YAML parser): the shapes this repository uses are
 * `- name:`/`- uses:` list items with an indented `run: |` block or a one-line `run: …`.
 * Anything the parser cannot attribute is caught by each guard's completeness rule, which
 * is why those rules exist.
 *
 * The parser is the one that used to live in `check_ci_wiring.cjs`; it is the more
 * complete of the two, and `check_gate_entries.cjs` consumes the same `extractSteps` output.
 */

/** A literal `false` condition — the step (or job) NEVER runs. */
function isLiteralFalse(cond) {
  return cond !== null && cond !== undefined
    && /^(false|\$\{\{\s*false\s*\}\})$/.test(String(cond).trim().toLowerCase());
}

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
  const indent = lines[i].length - lines[i].replace(/^\s*/, '').length;
  let end = i;
  for (let j = i + 1; j < lines.length; j += 1) {
    const l = lines[j];
    if (l.trim() === '') continue;
    if (l.length - l.replace(/^\s*/, '').length <= indent) break;
    end = j;
  }
  return end;
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
      const indent = line.length - line.replace(/^\s*/, '').length;
      if (line.trim() === '' || indent > blockIndent) {
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
      inDefaults = jobLevel[1] === 'defaults';
      inSteps = jobLevel[1] === 'steps';
      // NOTE the key is captured WITHOUT its colon (`if`, not `if:`): comparing to `"if:"` here made
      // this assignment dead code and the guard still printed "everything is executed".
      if (job && jobLevel[1] === 'if') job.disabled = isLiteralFalse(jobLevel[2]);
      continue;
    }
    if (inDefaults) {
      const dirLine = /^\s+working-directory:\s*(\S+)\s*$/.exec(line);
      if (dirLine && job) job.defaultDir = dirLine[1].replace(/^["']|["']$/g, '');
      continue;
    }
    if (/^\s*-\s/.test(line)) {
      flush();
      cur = [line];
      startLine = i + 1;
      const end = blockScalarEnd(lines, i);
      if (end !== null) blockIndent = line.length - line.replace(/^\s*/, '').length;
      continue;
    }
    if (cur) {
      cur.push(line);
      const end = blockScalarEnd(lines, i);
      if (end !== null) blockIndent = line.length - line.replace(/^\s*/, '').length;
    }
  }
  flush();
  return blocks;
}

/** The step's display name: its `- name:` value, or `(unnamed step)` for `- uses:`/anonymous steps. */
function stepName(block) {
  const first = block.length > 0 ? block[0] : '';
  const m = /^\s*-\s*name:\s*(.*)$/.exec(first);
  return m ? m[1].trim() || '(unnamed step)' : '(unnamed step)';
}

/**
 * Steps that actually carry a `run:` body, in file order.
 *
 * Each entry: `{ line, name, job, jobDisabled, dir, run, cond, lenient }`, where `run` is the
 * block scalar's lines joined with `\n` (a block scalar is a SHELL SCRIPT, so a `#` line
 * comments out only its own line — see `logicalCommands` in the wiring guard).
 *
 * `working-directory:` may come before OR after `run:` inside a step, and a job-level
 * `defaults: {run: {working-directory: …}}` applies to every step that does not set its own.
 *
 * The JOB BOUNDARY matters: a parser that split the file at ANY `- ` list item appended every
 * non-item line to the block being built, which conflated two jobs (measured with a fixture: job B's
 * job-level `if: false` was appended to job A's LAST step, disabling a step that is fine, while job
 * B's own steps still counted as executed) and dropped the FIRST job's job-level keys entirely
 * (nothing precedes them to be appended to). Both directions lie: a disabled job whose steps name an
 * artifact looked WIRED, and an enabled step looked DISABLED.
 */
function extractSteps(text) {
  const out = [];
  for (const { lines: block, startLine, job, jobDisabled, defaultDir } of stepBlocks(text)) {
    let dir = null;
    let run = null;
    let cond = null;
    let lenient = false;
    for (let i = 0; i < block.length; i += 1) {
      const c = block[i].match(/^\s*if:\s*(.+?)\s*$/);
      if (c) cond = c[1].replace(/^["']|["']$/g, '');
      const l = block[i].match(/^\s*continue-on-error:\s*(\S+)\s*$/);
      if (l) lenient = l[1].replace(/^["']|["']$/g, '') === 'true';
      const d = block[i].match(/^\s*working-directory:\s*(\S+)\s*$/);
      if (d) dir = d[1].replace(/^["']|["']$/g, '');
      const m = block[i].match(/^\s*-?\s*run:\s*(.*)$/);
      if (!m) continue;
      const rest = m[1].trim().replace(/^["']|["']$/g, '');
      if (/^[|>][-+]?\d*$/.test(rest)) {
        // LINE-JOINED, not space-joined: a block scalar is a SHELL SCRIPT, so a `#` line comments out
        // only the rest of ITS OWN line (see `logicalCommands`). The body's EXTENT is owned by
        // `blockScalarEnd`/`stepBlocks`, not recomputed here.
        const end = blockScalarEnd(block, i);
        const body = end === null
          ? []
          : block.slice(i + 1, end + 1).filter((l) => l.trim() !== '').map((l) => l.trim());
        run = body.join('\n');
        i = end === null ? i : end;
      } else {
        run = rest;
      }
    }
    // A job-level default applies only when the step does not set its own directory.
    if (dir === null && defaultDir) dir = defaultDir;
    if (run !== null) {
      out.push({ line: startLine, name: stepName(block), job, jobDisabled, dir, run, cond, lenient });
    }
  }
  return out;
}

module.exports = { isLiteralFalse, blockScalarEnd, stepBlocks, stepName, extractSteps };
