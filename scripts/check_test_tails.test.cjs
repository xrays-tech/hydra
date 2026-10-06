#!/usr/bin/env node
'use strict';
/**
 * Tests for `scripts/check_test_tails.cjs` — the guard that notices cases appended after a
 * top-level `process.exit`/`sys.exit` (they never run, and the suite still prints PASS).
 *
 * The trap happened twice in this repository (rounds 132 and 137) and was caught by accident both
 * times, so both directions and the boundary are pinned here.
 */
const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const SCRIPT = path.join(__dirname, 'check_test_tails.cjs');

function fixture(files) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'test-tails-'));
  for (const [rel, body] of Object.entries(files)) {
    const full = path.join(dir, rel);
    fs.mkdirSync(path.dirname(full), { recursive: true });
    fs.writeFileSync(full, body);
  }
  return dir;
}

function run(root, env = {}) {
  const res = spawnSync(process.execPath, [SCRIPT], {
    encoding: 'utf8',
    env: {
      ...process.env,
      CTT_ROOT: root,
      // Focused fixtures have one or two files; each floor has its OWN case below (passing an
      // explicit value), so neutralising them here cannot disable them.
      CTT_MIN_FILES: '1',
      CTT_MIN_JUDGED: '0',
      // The recorded-exception list is REPLACED (not extended) by this override, so a fixture tree
      // never inherits the repository's two records — and never trips the staleness check for
      // recording files it does not contain. Cases that need a record pass their own JSON.
      CTT_UNJUDGED_WITH_EXIT_OK: '{}',
      ...env,
    },
  });
  return { status: res.status, out: `${res.stdout || ''}${res.stderr || ''}` };
}

/* Round 176: an unjudged file that CONTAINS an exit call is a finding unless it is RECORDED, and a
 * record that no longer applies is a finding too. Rounds 170/175 each found such a file BY HAND
 * (`integration/test_crud.py`'s always-exiting `main()`, the two `unittest.main()` suites) while the OK
 * line reported only a count — "19 were NOT judged" hid the ones that mattered. The list is how a real
 * exception (a CONDITIONAL harness exit) is written down; the staleness check is what keeps it honest. */
test('an unjudged file containing an exit is a finding', () => {
  const body = 'const failures = [];\nconsole.log("summary");\nif (failures.length) {\n  process.exit(1);\n}\n';
  const r = run(fixture({ 'scripts/cond.test.cjs': body }));
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /scripts\/cond\.test\.cjs is NOT judged by this rule but CONTAINS an exit call/);
});

test('CONTROL: the same file RECORDED is accepted, and the record is reported', () => {
  const body = 'const failures = [];\nconsole.log("summary");\nif (failures.length) {\n  process.exit(1);\n}\n';
  const r = run(fixture({ 'scripts/cond.test.cjs': body }), {
    CTT_UNJUDGED_WITH_EXIT_OK: JSON.stringify({ 'scripts/cond.test.cjs': 'conditional harness exit' }),
  });
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /1 recorded exception\(s\) of the kind that hid rounds 170\/175/);
});

test('a RECORD that no longer applies is a finding (a stale claim)', () => {
  const r = run(fixture({ 'scripts/a.test.cjs': CLEAN }), {
    CTT_UNJUDGED_WITH_EXIT_OK: JSON.stringify({ 'scripts/gone.test.cjs': 'was a conditional exit' }),
  });
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /UNJUDGED_WITH_EXIT_OK records scripts\/gone\.test\.cjs, but that no longer applies/);
});

test('CONTROL: an unjudged file with NO exit call is not flagged', () => {
  const r = run(fixture({ 'scripts/plain.test.cjs': 'const x = 1;\nconsole.log(x);\n' }));
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /CONTAINS an exit call/);
});

const CLEAN = 'const f = [];\nassert("a", true);\nconsole.log("ALL PASSED");\nprocess.exit(f.length ? 1 : 0);\n';

test('a clean suite passes', () => {
  const r = run(fixture({ 'scripts/a.test.cjs': CLEAN }));
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /1 of 1 suite file\(s\) have a recognized program end/);
});

test('a case appended after the top-level exit is reported (the real trap shape)', () => {
  // Indented inside a block, exactly how it happened: the first version of this guard only looked
  // for case-shaped lines at column zero and did not see it.
  const r = run(fixture({
    'scripts/a.test.cjs': `${CLEAN}{\n  assert("appended later", true);\n}\n`,
  }));
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /DEAD scripts\/a\.test\.cjs:5/);
  assert.match(r.out, /never runs|nothing there ever runs/);
});

test('a trailing comment after the exit is NOT dead code', () => {
  const r = run(fixture({ 'scripts/a.test.cjs': `${CLEAN}// that is all\n\n# not code either\n` }));
  assert.equal(r.status, 0, r.out);
});

test('CONTROL: an INDENTED conditional exit is normal and is not reported', () => {
  const body = 'const f = [];\nif (process.env.CI) process.exit(1);\nassert("a", true);\nconsole.log("ok");\n';
  // RECORDED, because round 176 makes an unjudged file WITH an exit a finding: that is the same
  // statement this control makes ("the exit is conditional, so appended code still runs"), now written
  // in a form the guard can check instead of in prose.
  const r = run(fixture({ 'scripts/a.test.cjs': body }), {
    CTT_UNJUDGED_WITH_EXIT_OK: JSON.stringify({ 'scripts/a.test.cjs': 'conditional exit' }),
  });
  assert.equal(r.status, 0, r.out);
  assert.match(r.out, /0 of 1 suite file\(s\) have a recognized program end/);
});

test('the same rule applies to Python suites (`sys.exit` at column zero)', () => {
  const dead = 'import sys\nprint("ok")\nsys.exit(0)\nprint("never")\n';
  const r = run(fixture({ 'tests/test_x.py': dead }));
  assert.equal(r.status, 1, r.out);
  const ok = 'import sys\nprint("ok")\nsys.exit(0)\n';
  assert.equal(run(fixture({ 'tests/test_y.py': ok })).status, 0);
});

test('a tree with too few suite files is CANNOT VERIFY, never a pass', () => {
  const r = run(fixture({ 'notes.txt': 'nothing here\n' }));
  assert.equal(r.status, 2, r.out);
  assert.match(r.out, /CANNOT VERIFY/);
});

test('FALSE POSITIVE guard: an exit spelled inside a template literal is not an exit', () => {
  // Measured before the maskers: a fixture like this made the REAL cases after it look like dead code.
  const body = 'const f = [];\nfunction assert(n, c) { if (!c) f.push(n); }\nconst FIX = `\nprocess.exit(0);\n`;\nassert("real", true);\nconsole.log("ok");\n';
  const r = run(fixture({ 'scripts/t.test.cjs': body }));
  assert.equal(r.status, 0, r.out);
  // The point of this case is that the REAL `assert("real", true)` after the fixture is not
  // reported as dead, so assert that directly rather than only on the OK wording.
  assert.doesNotMatch(r.out, /DEAD/);
  assert.match(r.out, /0 of 1 suite file\(s\) have a recognized program end/);
});

test('FALSE POSITIVE guard: an exit spelled inside a Python docstring is not an exit', () => {
  const body = 'import sys\nDOC = """\nsys.exit(0)\n"""\nassert True\n';
  const r = run(fixture({ 'tests/test_d.py': body }));
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /DEAD/);
});

/* Round 141: the first version only knew ONE way a suite program ends (a column-zero exit), and
 * judged 6 of the real 64 suite files while its OK line read as if everything had been checked.
 * Measured: 35 Python drills end with `if __name__ == "__main__": sys.exit(main())` and one JS suite
 * ends inside an IIFE that exits — appending a case after either never runs. */
test('a Python `__main__` block that exits is an end: a case after it is dead', () => {
  const body = 'import sys\ndef main():\n    return 0\nif __name__ == "__main__":\n    sys.exit(main())\n\ntest_appended()\n';
  const r = run(fixture({ 'integration/test_x.py': body }));
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /DEAD integration\/test_x\.py:7/);
  assert.match(r.out, /if __name__ == "__main__"` block that exits/);
});

test('CONTROL: a `__main__` block that calls a main() which RETURNS does not end the program', () => {
  // The justification here used to be "`test_crud.py` is written this way, and code after the block
  // really does run" — measured 2026-10-01, that is FALSE about that file: its `main()` (lines
  // 567-595) has no `return` at all and ends with `sys.exit(0)`, so code after its block is DEAD and
  // the file was simply unjudged (round 170 added shape 4 for it). The CONTROL itself is still the
  // right control: a `main()` that can RETURN leaves the caller's next line live, and a rule that
  // ignored that would report live code as dead.
  const body = 'def main():\n    return 0\nif __name__ == "__main__":\n    main()\n\ntest_appended()\n';
  const r = run(fixture({ 'integration/test_y.py': body }));
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /DEAD/);
});

/* Round 170: shape 4 — `if __name__ == "__main__": main()` where `main()` exits on EVERY path. The
 * model required the `sys.exit(` to sit inside the `__main__` block, so `integration/test_crud.py`
 * (main() ends with `sys.exit(0)`, no `return` anywhere) was left unjudged: a case appended after its
 * block would never run and nothing would say so. The three controls below are the false-positive
 * boundary of the new shape, and each one is a real way to write `main()`. */
test('shape 4: a `__main__` block calling a main() that always exits IS a program end', () => {
  const body = 'import sys\ndef main():\n    print("run")\n    if 1:\n        sys.exit(1)\n    sys.exit(0)\n\nif __name__ == "__main__":\n    main()\n\ntest_appended()\n';
  const r = run(fixture({ 'integration/test_crud_like.py': body }));
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /DEAD integration\/test_crud_like\.py:11/);
  assert.match(r.out, /block calling `main\(\)`, whose last statement is `sys\.exit/);
});

test('CONTROL (shape 4): a `return` ANYWHERE in main() makes the block end live again', () => {
  // The `return` is on a branch that is never taken at runtime — the rule cannot know that, so it
  // must stay silent. This is the case that keeps the new shape from reporting live code as dead.
  const body = 'import sys\ndef main():\n    if 0:\n        return\n    sys.exit(0)\n\nif __name__ == "__main__":\n    main()\n\ntest_appended()\n';
  // RECORDED: the `return` disqualifies shape 4, so the file is unjudged AND contains `sys.exit(` —
  // exactly the combination round 176 refuses to leave unnamed.
  const r = run(fixture({ 'integration/test_returns.py': body }), {
    CTT_UNJUDGED_WITH_EXIT_OK: JSON.stringify({ 'integration/test_returns.py': 'a reachable return' }),
  });
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /DEAD/);
});

test('CONTROL (shape 4): a main() that merely ENDS (no sys.exit) is not a program end', () => {
  const body = 'def main():\n    print("done")\n\nif __name__ == "__main__":\n    main()\n\ntest_appended()\n';
  const r = run(fixture({ 'integration/test_noexit.py': body }));
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /DEAD/);
});

test('CONTROL (shape 4): an EMPTY main() is not a program end', () => {
  const body = 'def main():\n    pass\n\nif __name__ == "__main__":\n    main()\n\ntest_appended()\n';
  const r = run(fixture({ 'integration/test_empty.py': body }));
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /DEAD/);
});

test('FALSE POSITIVE guard: a `sys.exit` inside an indented body (no `__main__`) is not an end', () => {
  // Measured 2026-09-30: the block-end scan works on indentation, so without the shape check ANY
  // line opening an indented block that mentions `sys.exit(` became a "program end" — and this file,
  // where nothing calls `main`, had its real appended case reported as dead code.
  const body = 'import sys\ndef main():\n    sys.exit(1)\n\ntest_appended()\n';
  // RECORDED: nothing calls `main`, so its `sys.exit` is unreachable and the appended case DOES run —
  // the file is unjudged, and the exit inside it is why round 176 demands a record.
  const r = run(fixture({ 'integration/test_z.py': body }), {
    CTT_UNJUDGED_WITH_EXIT_OK: JSON.stringify({ 'integration/test_z.py': 'an uncalled main()' }),
  });
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /DEAD/);
});

/* Round 175: shape 5 — `if __name__ == "__main__": unittest.main()`. `unittest.main` exits by
 * default (`exit=True` ⇒ `sys.exit(...)`), so a case appended after the block never runs; TWO real
 * suites end this way (`integration/test_error_contract.py`, `tools/hydra-py/tests/test_client.py`)
 * and the previous model left them unjudged because its "always exits" rule only looked at functions
 * DEFINED in the same file. The two controls are the boundary: `exit=False` is the documented opt-out
 * (appended code DOES run), and `pytest.main(...)` only RETURNS an exit code. */
test('shape 5: a `__main__` block calling `unittest.main()` IS a program end', () => {
  const body = 'import unittest\nclass T(unittest.TestCase):\n    def test_ok(self):\n        pass\n\nif __name__ == "__main__":\n    unittest.main(verbosity=2)\n\ntest_appended()\n';
  const r = run(fixture({ 'integration/test_unittest.py': body }));
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /DEAD integration\/test_unittest\.py:9/);
  assert.match(r.out, /calling `unittest\.main\(\)`, which exits by default/);
});

test('CONTROL (shape 5): `unittest.main(exit=False)` does NOT end the program', () => {
  const body = 'import unittest\nclass T(unittest.TestCase):\n    def test_ok(self):\n        pass\n\nif __name__ == "__main__":\n    unittest.main(exit=False)\n\ntest_appended()\n';
  const r = run(fixture({ 'integration/test_unittest_noexit.py': body }));
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /DEAD/);
});

test('CONTROL (shape 5): `pytest.main(...)` does NOT end the program (it only returns a code)', () => {
  const body = 'import pytest\n\nif __name__ == "__main__":\n    pytest.main(["-q"])\n\ntest_appended()\n';
  const r = run(fixture({ 'integration/test_pytest.py': body }));
  assert.equal(r.status, 0, r.out);
  assert.doesNotMatch(r.out, /DEAD/);
});

/* Round 155: an IIFE that does NOT exit must not end the SCAN. Measured: with two column-zero
 * IIFEs (the first without an exit, the second exiting) the file was left unjudged — and the OK line
 * called its shape one "this rule does not model", while the shape IS modelled. The appended case
 * after the second IIFE was silently missed. */
test('a file with TWO IIFEs is judged on the one that exits', () => {
  const body = 'const failures = [];\n(async () => {\n  failures.push("x");\n})();\n'
    + '(async () => {\n  process.exit(failures ? 1 : 0);\n})();\n\ncheck("appended", true);\n';
  const r = run(fixture({ 'scripts/two-iife.test.cjs': body }));
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /DEAD scripts\/two-iife\.test\.cjs:9/);
  assert.match(r.out, /IIFE that exits/);
});

test('an IIFE that exits is an end: a case after it is dead', () => {  const body = 'const failures = [];\n(async () => {\n  failures.push("x");\n  process.exit(failures ? 1 : 0);\n})();\n\ncheck("appended", true);\n';
  const r = run(fixture({ 'scripts/iife.test.cjs': body }));
  assert.equal(r.status, 1, r.out);
  assert.match(r.out, /IIFE that exits/);
});

test('CONTROL: an IIFE that does NOT exit does not end the program', () => {
  const body = 'const failures = [];\n(async () => {\n  failures.push("x");\n})();\n\ncheck("appended", true);\n';
  const r = run(fixture({ 'scripts/iife2.test.cjs': body }));
  assert.equal(r.status, 0, r.out);
});

test('the JUDGED floor refuses to print OK when the rule reaches almost nothing', () => {
  const r = run(fixture({ 'scripts/a.test.cjs': CLEAN }), { CTT_MIN_JUDGED: '99' });
  assert.equal(r.status, 2, r.out);
  assert.match(r.out, /CANNOT VERIFY/);
});
