#!/usr/bin/env node
/* Tests for scripts/check_ci_wiring.cjs.
 *
 * The checker decides whether test artifacts have a runner. Its predicates are
 * regexes over YAML and Rust, which is exactly the kind of code that passes for
 * the wrong reason — so each branch is exercised against a throwaway skeleton
 * under `os.tmpdir()` (via HYDRA_WIRING_ROOT), including the false-positive case
 * that a naive `grep '#[ignore]'` gets wrong.
 *
 * Run: node scripts/check_ci_wiring.test.cjs   (CI runs it via `node --test`)
 */
"use strict";
const fs = require("fs");
const os = require("os");
const path = require("path");
const { execFileSync } = require("child_process");

const SCRIPT = path.join(__dirname, "check_ci_wiring.cjs");
let seq = 0;

/** A minimal repo skeleton: ci.yml + whatever artifacts the case needs. */
function skeleton({ ci, scripts = {}, crateTests = {}, e2e = [], playwright = null, extraFiles = {} }) {
  const root = path.join(os.tmpdir(), `wiring_${process.pid}_${seq++}`);
  fs.rmSync(root, { recursive: true, force: true });
  fs.mkdirSync(path.join(root, ".github", "workflows"), { recursive: true });
  fs.writeFileSync(path.join(root, ".github", "workflows", "ci.yml"), ci);
  if (Object.keys(scripts).length) {
    fs.mkdirSync(path.join(root, "scripts"), { recursive: true });
    for (const [name, body] of Object.entries(scripts)) fs.writeFileSync(path.join(root, "scripts", name), body);
  }
  for (const [rel, body] of Object.entries(crateTests)) {
    const p = path.join(root, rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, body);
  }
  if (e2e.length) {
    fs.mkdirSync(path.join(root, "tests", "e2e"), { recursive: true });
    for (const name of e2e) fs.writeFileSync(path.join(root, "tests", "e2e", name), "// spec\n");
  }
  if (playwright) fs.writeFileSync(path.join(root, "playwright.config.cjs"), playwright);
  for (const [rel, body] of Object.entries(extraFiles || {})) {
    const p = path.join(root, rel);
    fs.mkdirSync(path.dirname(p), { recursive: true });
    fs.writeFileSync(p, body);
  }
  return root;
}

function run(root, env = {}) {
  try {
    const out = execFileSync("node", [SCRIPT], {
      encoding: "utf8",
      stdio: ["ignore", "pipe", "pipe"],
      // Small fixtures have no test artifacts outside scripts/; the floor exists for
      // the real repository (and has its own case, with an explicit value).
      env: {
        ...process.env,
        HYDRA_WIRING_ROOT: root,
        // The floors exist for the real repository; focused skeletons are two or three directories
        // deep. Each floor has its OWN case below (passing an explicit value), so overriding them
        // here cannot disable them.
        CI_WIRING_MIN_OTHER_TESTS: "0",
        CI_WIRING_MIN_DIRS: "0",
        // Every floor is neutralised for the focused skeletons; each has its OWN case below (passing
        // an explicit value), so this cannot disable them.
        CI_WIRING_MIN_SCRIPTS: "0",
        CI_WIRING_MIN_CRATE_TESTS: "0",
        CI_WIRING_MIN_E2E_SPECS: "0",
        CI_WIRING_MIN_INTEGRATION: "0",
        CI_WIRING_MIN_TOOLS: "0",
        CI_WIRING_MIN_SHELL_SCRIPTS: "0",
        // Rule 1b's recorded exceptions describe the REAL repository (a script the local gate runs
        // and CI does not). A skeleton must not inherit them: `{}` REPLACES the built-in list, so a
        // fixture without that script never trips the staleness check for a file it does not
        // contain — and the cases that judge the record itself pass their own value below.
        CIW_NOT_EXECUTED_OK: "{}",
        ...env,
      },
    });
    return { status: 0, out: out.toString() };
  } catch (e) {
    return { status: e.status === undefined ? 1 : e.status, out: (e.stdout || "") + (e.stderr || "") };
  }
}

/** Enough artifacts to clear every floor, all wired. */
function baseCi(extra = "") {
  return [
    "jobs:",
    "  check:",
    "    steps:",
    '      - run: cargo test -p hydra-core',
    '      - run: cargo test -p hydra-server --test boot_listeners -- --ignored',
    '      - run: npx playwright test --config=playwright.config.cjs',
    '      - run: node scripts/a.test.cjs',
    '      - run: node scripts/check_a.cjs',
    '      - run: node scripts/check_b.cjs',
    '      - run: node scripts/check_c.cjs',
    '      - run: node scripts/check_d.cjs',
    '      - run: bash scripts/d.test.sh',
    extra,
  ].filter(Boolean).join("\n");
}

const wiredScripts = {
  "a.test.cjs": "// t\n",
  "check_a.cjs": "// c\n",
  "check_b.cjs": "// c\n",
  "check_c.cjs": "// c\n",
  "check_d.cjs": "// c\n",
  "d.test.sh": "#!/bin/sh\n",
};
const crateTests = Object.fromEntries(
  Array.from({ length: 45 }, (_, i) => [`crates/hydra-server/tests/t${i}.rs`, "#[test]\nfn t() {}\n"]),
);
crateTests["crates/hydra-server/tests/boot_listeners.rs"] = '#[test]\n#[ignore]\nfn slow() {}\n';

let failures = 0;
function assert(name, cond, detail) {
  if (cond) console.log("PASS  " + name);
  else {
    failures++;
    console.error("FAIL  " + name + (detail ? "  -> " + detail : ""));
  }
}

/* 1. A fully wired skeleton passes (otherwise every later case proves nothing). */
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert("a fully wired skeleton passes", r.status === 0, "status=" + r.status + " out=" + r.out.trim().slice(0, 200));
  assert("it reports how many artifacts it examined", /script artifact\(s\).*crate test file\(s\)/s.test(r.out), r.out.trim().slice(0, 200));
}

/* 2. An artifact nobody runs is caught (this is the mistake the checker exists
 *    for: this session forgot it twice). */
{
  const root = skeleton({
    ci: baseCi(),
    scripts: { ...wiredScripts, "check_orphan.cjs": "// c\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert("an unwired script exits non-zero", r.status !== 0, "status=" + r.status);
  assert("...and is named", r.out.includes("check_orphan.cjs"), r.out.trim().slice(0, 160));
}

/* 3. `#[ignore]`d tests with no `--ignored` step are caught... */
{
  const ci = baseCi().replace(" --test boot_listeners -- --ignored", "");
  const root = skeleton({ ci, scripts: wiredScripts, crateTests, e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"], playwright: "module.exports = { testDir: './tests/e2e' };\n" });
  const r = run(root);
  assert("an #[ignore] test with no --ignored step is caught", r.status !== 0 && /boot_listeners/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 160));
}

/* 4. ...but a #[ignore] MENTION IN A COMMENT is not an ignored test.
 *    (`grep -l '#[ignore]'` gets this wrong — it flagged the repo's own
 *    test_attribute_integrity.rs, which only documents the attribute.) */
{
  const ct = { ...crateTests };
  ct["crates/hydra-server/tests/mentions_only.rs"] = "#[test]\nfn t() {\n    // Another attribute in the same block (`#[ignore]`, `#[should_panic]`).\n}\n";
  const root = skeleton({ ci: baseCi(), scripts: wiredScripts, crateTests: ct, e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"], playwright: "module.exports = { testDir: './tests/e2e' };\n" });
  const r = run(root);
  assert("a comment mentioning #[ignore] is not treated as an ignored test", r.status === 0, "status=" + r.status + " out=" + r.out.trim().slice(0, 200));
}

/* 5. `#[ignore = "reason"]` IS an ignored test (the form `grep '#[ignore]'`
 *    misses, and which `usage_query.rs` really uses). */
{
  const ct = { ...crateTests };
  ct["crates/hydra-server/tests/with_reason.rs"] = '#[test]\n#[ignore = "needs a live service"]\nfn slow() {}\n';
  const root = skeleton({ ci: baseCi(), scripts: wiredScripts, crateTests: ct, e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"], playwright: "module.exports = { testDir: './tests/e2e' };\n" });
  const r = run(root);
  assert('an #[ignore = "reason"] test is caught when unwired', r.status !== 0 && /with_reason/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 160));
}

/* 6. A spec outside the configured Playwright testDir never runs. */
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/elsewhere' };\n",
  });
  const r = run(root);
  assert("a testDir mismatch is caught", r.status !== 0 && /testDir/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 200));
}

/* 7. Floors: an empty skeleton must not pass vacuously. */
{
  const root = skeleton({ ci: "jobs:\n  check:\n    steps:\n      - run: echo hi\n" });
  // Explicit floors: the harness zeroes them for focused skeletons, and THIS case is about them.
  const r = run(root, { CI_WIRING_MIN_SCRIPTS: "1" });
  // NAME FIXED (round 158): this case asserts "an empty skeleton is REFUSED, not a silent pass" —
  // it happens to be a floor that fires today, but another rule can fire first (measured by the
  // round-155 reviewer: with the script floor disabled this case still reddens, via
  // `playwright.config.cjs declares no testDir`). The floor-specific cases are the ones that
  // name their floor (e.g. "the script-artifact floor catches a shrunken scripts/ directory").
  assert("an empty skeleton is refused (not a silent pass)", r.status !== 0 && /probably wrong/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 200));
}

/* 8. An artifact executed ONLY inside a YAML block scalar (`run: |`) is wired.
 *    A line-wise `run:` filter sees just `run: |` and would call this unwired —
 *    a false positive that would push someone to add a redundant step. */
{
  const ci = baseCi(
    [
      "      - name: a multi-line step",
      "        run: |",
      "          set -euo pipefail",
      "          node scripts/check_blocky.cjs",
    ].join("\n"),
  );
  const root = skeleton({
    ci,
    scripts: { ...wiredScripts, "check_blocky.cjs": "// c\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert("an artifact run inside a block scalar counts as wired", r.status === 0, "status=" + r.status + " out=" + r.out.trim().slice(0, 200));
}

/* 9. ...but a MENTION does not. A step `name:` is not a command, and an earlier
 *    version of the checker accepted any occurrence in the file. */
{
  const ci = baseCi("      - name: mentions node scripts/check_mentioned.cjs");
  const root = skeleton({
    ci,
    scripts: { ...wiredScripts, "check_mentioned.cjs": "// c\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert("a mention in a step name is not execution", r.status !== 0 && /check_mentioned/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 160));
}

/* 10. An INLINE COMMENT mentioning the file inside a `run:` line is not execution.
 *     A review demonstrated this exact probe against the previous version: the
 *     real step replaced by `echo a ok  # scripts/a.test.cjs …` still gave a green
 *     guard. */
{
  const ci = baseCi().replace(
    "      - run: node scripts/a.test.cjs",
    "      - run: echo a ok   # scripts/a.test.cjs is mentioned here only",
  );
  const root = skeleton({ ci, scripts: wiredScripts, crateTests, e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"], playwright: "module.exports = { testDir: './tests/e2e' };\n" });
  const r = run(root);
  assert("a filename inside an inline comment is not execution", r.status !== 0 && /a\.test\.cjs/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 160));
}

/* 11. Reading a file is not running it: `grep`/`ls`/`cat` must not satisfy the
 *     guard either. */
{
  const ci = baseCi().replace(
    "      - run: node scripts/a.test.cjs",
    "      - run: grep -q something scripts/a.test.cjs",
  );
  const root = skeleton({ ci, scripts: wiredScripts, crateTests, e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"], playwright: "module.exports = { testDir: './tests/e2e' };\n" });
  const r = run(root);
  assert("a command that merely reads the file is not execution", r.status !== 0 && /a\.test\.cjs/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 160));
}

/* 12. `#[should_panic] #[ignore]` on ONE line is still an ignored test (the old
 *     line-anchored regex missed it). */
{
  const ct = { ...crateTests };
  ct["crates/hydra-server/tests/inline_attrs.rs"] = '#[test]\n#[should_panic] #[ignore]\nfn t() {}\n';
  const root = skeleton({ ci: baseCi(), scripts: wiredScripts, crateTests: ct, e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"], playwright: "module.exports = { testDir: './tests/e2e' };\n" });
  const r = run(root);
  assert("an inline second attribute is detected", r.status !== 0 && /inline_attrs/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 160));
}

/* 13. Test artifacts OUTSIDE `scripts/` and `crates/<crate>/tests` must also have a
 *     runner. This is the rule that was missing when three real suites
 *     (`integration/test_crud.py`, `integration/e2e_proxy_test.py`,
 *     `tools/hydra-cli/test/client.test.ts`) sat unrun — all three green when finally
 *     executed, which is exactly the point. */
const otherTests = {
  "integration/test_crud.py": "# test\n",
  "integration/e2e_proxy_test.py": "# test\n",
  "tools/cli/test/client.test.ts": "// test\n",
  "tools/sdk/client_test.go": "package main\n",
};
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: otherTests,
  });
  const r = run(root);
  assert("an unrun suite outside scripts/ is caught", r.status !== 0, "status=" + r.status);
  assert("...and every one of them is named", /test_crud\.py/.test(r.out) && /e2e_proxy_test\.py/.test(r.out) && /client\.test\.ts/.test(r.out) && /client_test\.go/.test(r.out), r.out.trim().slice(0, 240));
}

/* 14. ...and each of the ways to cover one counts: a step naming the file, a step naming a
 *     SCRIPT that runs it, a bare import from a wired sibling, or a test runner started in the
 *     file's working-directory.
 *
 *     The fixture used to rely on "naming its directory via a script" — `./integration/run-crud-local.sh`
 *     covering `integration/test_crud.py` because the run string contained `integration/`. That is
 *     the vacuous rule case 16 removed; the script here therefore NAMES the file it runs, which is
 *     what the real `integration/run-crud-local.sh` does too. */
{
  const ci = baseCi(
    [
      "      - run: python3 integration/e2e_proxy_test.py",
      "      - run: ./integration/run-crud-local.sh",
      "      - run: go test ./...",
      "        working-directory: tools/sdk",
      "      - run: npm test",
      "        working-directory: tools/cli",
    ].join("\n"),
  );
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      ...otherTests,
      // The harness CI executes, naming the file it runs (as the real one does).
      "integration/run-crud-local.sh": "#!/bin/sh\npython3 integration/test_crud.py\n",
    },
  });
  const r = run(root);
  assert("every coverage shape is accepted", r.status === 0, "status=" + r.status + " out=" + r.out.trim().slice(0, 240));
}

/* 15. The discovery floor itself: a skeleton with NO artifacts outside scripts/
 *     must not pass when the floor is on (an over-broad `SKIP_DIRS` could otherwise
 *     turn the whole rule into a no-op). */
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  let r;
  try {
    r = { status: 0, out: execFileSync("node", [SCRIPT], {
      encoding: "utf8", stdio: ["ignore", "pipe", "pipe"],
      env: { ...process.env, HYDRA_WIRING_ROOT: root, CI_WIRING_MIN_OTHER_TESTS: "4" },
    }).toString() };
  } catch (e) {
    r = { status: e.status === undefined ? 1 : e.status, out: (e.stdout || "") + (e.stderr || "") };
  }
  assert("the discovery floor trips when nothing is found", r.status !== 0 && /probably wrong/.test(r.out), "status=" + r.status + " out=" + r.out.trim().slice(0, 160));
}

/* 16. THE REGRESSION THIS GUARD SHIPPED WITH: `integration/**` files used to be covered by
 *     "its directory appears in some run command", which made the whole rule vacuous there —
 *     a brand-new drill nobody wired was reported as "everything is executed" (measured
 *     2026-09-30 with `integration/test_zzz_dummy_probe.py`). A file in `integration/` that no
 *     step names must now be UNWIRED, even though other steps DO contain `integration/`. */
{
  const ci = baseCi('      - run: python3 integration/test_wired_one.py');
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      "integration/test_wired_one.py": "#!/usr/bin/env python3\n",
      "integration/test_nobody_runs.py": "#!/usr/bin/env python3\n",
    },
  });
  const r = run(root);
  assert(
    "a new file in integration/ is UNWIRED unless a step names it",
    r.status !== 0 && /integration\/test_nobody_runs\.py is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 17. ...and the positive directions of the new chain: a file executed by a script CI runs,
 *     and a file imported by a wired file (one hop and two hops). */
{
  const ci = baseCi('      - run: ./integration/harness.sh');
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      "integration/harness.sh": "#!/bin/sh\npython3 integration/run_by_script.py\n",
      "integration/run_by_script.py": "#!/usr/bin/env python3\n",
      "integration/entry.py": "#!/usr/bin/env python3\nimport helper\n",
      "integration/helper.py": "def f():\n    return 1\n",
    },
  });
  const r = run(root);
  assert(
    "a file a CI-executed script runs is wired, and imports chain (entry.py -> helper.py)",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 18. The malformed-step shape: a step body missing its `- name:`/list marker merges into the
 *     step above and produces duplicate `env:`/`run:` keys (how the live-deps ClickHouse step
 *     sat for several rounds). The guard must name it rather than pick the last `run:` and
 *     call the other step wired. */
{
  const ci = [
    "jobs:",
    "  check:",
    "    steps:",
    '      - run: cargo test -p hydra-core',
    '      - run: cargo test -p hydra-server --test boot_listeners -- --ignored',
    '      - run: npx playwright test --config=playwright.config.cjs',
    '      - run: node scripts/a.test.cjs',
    '      - run: node scripts/check_a.cjs',
    '      - run: node scripts/check_b.cjs',
    '      - run: node scripts/check_c.cjs',
    '      - run: node scripts/check_d.cjs',
    '      - run: bash scripts/d.test.sh',
    '      - name: a step whose body lost its marker',
    '        env:',
    '          X: 1',
    '        run: python3 integration/test_wired_one.py',
    '        env:',
    '          Y: 2',
    '        run: python3 integration/other.py',
  ].join("\n");
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      "integration/test_wired_one.py": "#!/usr/bin/env python3\n",
      "integration/other.py": "#!/usr/bin/env python3\n",
    },
  });
  const r = run(root);
  assert(
    "a merged step body (duplicate run:/env: keys) is reported",
    r.status !== 0 && /has 2 `run:` keys/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 19. A COMMENT is not evidence of execution — the round-104 predicate fix covered rule 1
 *     (scripts/) with `stripInlineComment`, but rules 4 and 5 kept matching the raw `run:`
 *     text, which preserves YAML and shell comments. Measured 2026-09-30: naming a brand-new
 *     drill ONLY in a comment marked it wired, and a `# TODO re-enable --ignored` on the
 *     `usage_query` line marked the `#[ignore]`d target as run. Both directions are pinned
 *     here, with the positive control (the same text, without the `#`) in each case. */
{
  const ci = baseCi([
    '      - run: python3 integration/test_wired_one.py',
    '      - run: echo ok  # TODO also run integration/test_zzz_probe.py',
  ].join("\n"));
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      "integration/test_wired_one.py": "#!/usr/bin/env python3\n",
      "integration/test_zzz_probe.py": "#!/usr/bin/env python3\n",
    },
  });
  const r = run(root);
  assert(
    "a drill named only inside a CI comment is still UNWIRED",
    r.status !== 0 && /integration\/test_zzz_probe\.py is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 20. ...and the `#[ignore]` rule: `--ignored` inside a comment does not run anything.
 *     NOTE the fixture: `baseCi()` already runs `boot_listeners --ignored` for real, so this
 *     case needs an ignored target that NO genuine step runs and whose only `--ignored`
 *     mention sits in a comment — otherwise the base line would satisfy rule 4 and the case
 *     would pass for the wrong reason (the first version of this case did exactly that and
 *     reported green; the control in case 21 is what exposed it). */
const ignoredExtra = { "crates/hydra-server/tests/usage_query.rs": "#[test]\n#[ignore]\nfn slow() {}\n" };
{
  const ci = baseCi('      - run: cargo test -p hydra-server --test usage_query   # TODO re-enable --ignored');
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests: { ...crateTests, ...ignoredExtra },
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "an `#[ignore]` target whose `--ignored` appears only in a comment is reported UNWIRED",
    r.status !== 0 && /usage_query\.rs contains #\[ignore\] tests but no step runs/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 21. CONTROL for case 20: the same line WITHOUT the comment is genuinely wired, so case 20
 *     proves the comment is what made the difference rather than something else in the fixture. */
{
  const ci = baseCi('      - run: cargo test -p hydra-server --test usage_query -- --ignored');
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests: { ...crateTests, ...ignoredExtra },
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: the same step without the comment passes",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 22. A checker named `check_*.py` must be DISCOVERED (round 118). `TEST_GLOBS` matched
 *     `test_*.py`/`*_test.py` but not `check_*.py`, so `integration/check_api_docs.py` and
 *     `integration/check_error_contract.py` — both real checkers in this repository — were
 *     never discovered and therefore never checked for a runner; a new one would silently join
 *     them. The positive control (the same file named by a step) is in the untouched chain
 *     cases, and the real tree proves the shipped pair still resolves through
 *     `run-crud-local.sh` → `check_error_contract.py` → `check_api_docs.py`. */
{
  const ci = baseCi('      - run: python3 integration/test_wired_one.py');
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      "integration/test_wired_one.py": "#!/usr/bin/env python3\n",
      "integration/check_orphan.py": "#!/usr/bin/env python3\n",
    },
  });
  const r = run(root);
  assert(
    "an unwired `integration/check_*.py` checker is reported (it used to be invisible)",
    r.status !== 0 && /integration\/check_orphan\.py is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 23. CONTROL for 22: the same checker, named by a step, passes. */
{
  const ci = baseCi('      - run: python3 integration/check_wired.py');
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: { "integration/check_wired.py": "#!/usr/bin/env python3\n" },
  });
  const r = run(root);
  assert(
    "CONTROL: a `check_*.py` named by a step passes",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 24. Round 126: an artifact in a directory the walk used to SKIP was invisible — neither
 *     discovered nor counted — so `admin-ui/`, `bin/`, `docs/`, `environment/` and `tests/` (outside
 *     `tests/e2e`) were holes in "every artifact is executed". Both cases below are new coverage of
 *     exactly those holes, with a control for each. */
{
  const root = skeleton({
    ci: baseCi('      - run: python3 integration/test_wired_one.py'),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      "integration/test_wired_one.py": "#!/usr/bin/env python3\n",
      "admin-ui/panel.test.cjs": "// never run\n",
    },
  });
  const r = run(root);
  assert(
    "an artifact under `admin-ui/` (a previously SKIPPED dir) is discovered and reported unwired",
    r.status !== 0 && /admin-ui\/panel\.test\.cjs is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}
{
  const root = skeleton({
    ci: baseCi('      - run: node --test admin-ui/panel.test.cjs'),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: { "admin-ui/panel.test.cjs": "// run by the step above\n" },
  });
  const r = run(root);
  assert(
    "CONTROL: the same artifact passes once a step runs it",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}
{
  const root = skeleton({
    ci: baseCi('      - run: npx playwright test --config=playwright.config.cjs'),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: { "tests/stray.spec.cjs": "// outside testDir\n" },
  });
  const r = run(root);
  assert(
    "a spec OUTSIDE tests/e2e is discovered and reported unwired (the walk used to skip `tests/`)",
    r.status !== 0 && /tests\/stray\.spec\.cjs is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}
{
  // The walk floor itself, with an explicit value.
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root, { CI_WIRING_MIN_DIRS: "500" });
  assert(
    "the directory-walk floor catches a walk that visited almost nothing",
    r.status !== 0 && /director\(ies\)/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 25. Round 130: a step that CANNOT run, or whose failure cannot block anything, is not evidence
 *     that an artifact is executed/gated. Before this, `if: false` and `continue-on-error: true`
 *     were simply not read, so a disabled step's `run:` counted as execution. */
{
  const root = skeleton({
    ci: baseCi(['      - run: node --test scripts/extra.test.cjs', '        if: false'].join("\n")),
    scripts: { ...wiredScripts, "extra.test.cjs": "// disabled step only\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "a step disabled by `if: false` does not count as executing its artifact",
    r.status !== 0 && /disabled by `if: false`/.test(r.out) && /extra\.test\.cjs is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}
{
  const root = skeleton({
    ci: baseCi(['      - run: node --test scripts/extra.test.cjs', '        continue-on-error: true'].join("\n")),
    scripts: { ...wiredScripts, "extra.test.cjs": "// lenient step\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "a test step with `continue-on-error: true` is reported as not a gate",
    r.status !== 0 && /not a gate/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}
{
  // CONTROL: the same wiring without those keys passes — so the cases above fail because of the
  // keys, not because of something else in the fixture.
  const root = skeleton({
    ci: baseCi('      - run: node --test scripts/extra.test.cjs'),
    scripts: { ...wiredScripts, "extra.test.cjs": "// plain step\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: the same step without `if:`/`continue-on-error:` passes",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}

/* 26. Round 133: floors that can actually catch something, and per-category floors.
 *     `MIN_SCRIPTS` was 6 against 30 real checkers (deleting 24 would not have tripped it) and one
 *     global artifact count can be fed by a single category (81% of the real ones are integration/). */
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root, { CI_WIRING_MIN_SCRIPTS: "99" });
  assert(
    "the script-artifact floor catches a shrunken scripts/ directory",
    r.status !== 0 && /script artifact\(s\) found/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root, { CI_WIRING_MIN_INTEGRATION: "5" });
  assert(
    "the per-category floor notices a whole category disappearing (integration/)",
    r.status !== 0 && /under `integration\/`/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 27. Round 133: `crates/` and `.github/` were skipped by the walk, so a test artifact there was
 *     invisible (rules 3/4 only cover `crates/<crate>/tests/*.rs`; only ci.yml is read from
 *     `.github`). Both are walked now, with a control for each. */
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      "crates/hydra-server/tests/check_probe.py": "#!/usr/bin/env python3\n",
      ".github/scripts/probe.test.cjs": "// never run\n",
    },
  });
  const r = run(root);
  assert(
    "an artifact under `crates/` is discovered and reported unwired",
    r.status !== 0 && /crates\/hydra-server\/tests\/check_probe\.py is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
  assert(
    "an artifact under `.github/` is discovered and reported unwired",
    r.status !== 0 && /\.github\/scripts\/probe\.test\.cjs is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}
{
  const root = skeleton({
    ci: baseCi([
      "      - run: python3 crates/hydra-server/tests/check_probe.py",
      "      - run: node --test .github/scripts/probe.test.cjs",
    ].join("\n")),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      "crates/hydra-server/tests/check_probe.py": "#!/usr/bin/env python3\n",
      ".github/scripts/probe.test.cjs": "// run by the step above\n",
    },
  });
  const r = run(root);
  assert(
    "CONTROL: both pass once a step names them",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}

/* 30-33 (round 145). Inside a `run: |` block scalar the commands were joined with SPACES, which
 * turns a shell comment into a terminator for every line after it and merges separate commands into
 * one string. Measured on the real workflow: 5 of the 19 block scalars contain such a `#` line (up
 * to 18 commands follow it). Both directions are covered here, each with its control. */
{
  // (a) a comment line must not hide the commands BELOW it: only `check_d.cjs` is inside the block,
  //     and only after the comment.
  const ci = [
    "jobs:",
    "  check:",
    "    steps:",
    "      - run: cargo test -p hydra-core",
    "      - run: cargo test -p hydra-server --test boot_listeners -- --ignored",
    "      - run: npx playwright test --config=playwright.config.cjs",
    "      - run: node scripts/a.test.cjs",
    "      - run: node scripts/check_a.cjs",
    "      - run: node scripts/check_b.cjs",
    "      - run: node scripts/check_c.cjs",
    "      - run: |",
    "          # a shell comment ends at ITS OWN line, not at the end of the block",
    "          node scripts/check_d.cjs",
    "      - run: bash scripts/d.test.sh",
  ].join("\n");
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "a comment line inside a `run: |` block does not hide the commands after it",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}
{
  // (b) `--ignored` in a DIFFERENT command of the same step must not wire the target up.
  const ci = baseCi([
    "      - run: |",
    "          cargo test -p hydra-server --test usage_query",
    "          cargo test -p hydra-server -- --ignored",
  ].join("\n"));
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests: { ...crateTests, ...ignoredExtra },
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "`--ignored` from ANOTHER command in the same step does not count as running the target",
    r.status !== 0 && /usage_query\.rs contains #\[ignore\] tests but no step runs/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}
{
  // (c) CONTROL for (b): one command carrying both satisfies the rule.
  const ci = baseCi('      - run: cargo test -p hydra-server --test usage_query -- --ignored');
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests: { ...crateTests, ...ignoredExtra },
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: one command with `--test usage_query … --ignored` is wired",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}
{
  // (d) CONTROL: a `\`-continued command is ONE command — the legitimate multi-line shape must keep
  //     working after the rules became line-aware.
  const ci = baseCi([
    "      - run: |",
    "          cargo test -p hydra-server --features server,cluster-redis \\",
    "            --test usage_query -- --ignored",
  ].join("\n"));
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests: { ...crateTests, ...ignoredExtra },
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: a `\\`-continued invocation still counts as one command",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}

/* 40-43 (round 149): the JOB boundary. The parser used to split the file at ANY `- ` list item and
 * append every other line to the block being read, which (a) appended the NEXT job's job-level
 * `if: false` to the PREVIOUS job's last step — disabling a step that is fine — and (b) dropped the
 * FIRST job's job-level keys entirely. Both directions lie: a disabled job's artifacts looked WIRED,
 * and an enabled step looked DISABLED. Measured with these fixtures before the fix: `jobDisabled`
 * was false for a job whose `if: false` was never even read (`if` vs `if:`), and the guard printed
 * `OK (everything is executed)` for a job that never runs. */
const jobBody = () => ([
  "      - run: cargo test -p hydra-core",
  "      - run: cargo test -p hydra-server --test boot_listeners -- --ignored",
  "      - run: npx playwright test --config=playwright.config.cjs",
  "      - run: node scripts/a.test.cjs",
  "      - run: node scripts/check_a.cjs",
  "      - run: node scripts/check_b.cjs",
  "      - run: node scripts/check_c.cjs",
  "      - run: bash scripts/d.test.sh",
]);
const jobFixture = (lines) => skeleton({
  ci: lines.join("\n"),
  scripts: { ...wiredScripts, "check_d.cjs": "// c\n" },
  crateTests,
  e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
  playwright: "module.exports = { testDir: './tests/e2e' };\n",
});
{
  // check_d.cjs is named ONLY by the FIRST job, whose job-level `if:` is false.
  const root = jobFixture([
    "jobs:",
    "  a:",
    "    if: false",
    "    runs-on: ubuntu-latest",
    "    steps:",
    "      - run: node scripts/check_d.cjs",
    "  b:",
    "    runs-on: ubuntu-latest",
    "    steps:",
    ...jobBody(),
  ]);
  const r = run(root);
  assert(
    "a step of a job disabled at the JOB level is not execution (it used to count as wired)",
    r.status !== 0 && /every step of job `a` is disabled by a JOB-level `if: false`/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
  assert(
    "...and the artifact it named is reported as never executed",
    /scripts\/check_d\.cjs is never executed/.test(r.out),
    "out=" + r.out.trim().slice(0, 240),
  );
}
{
  // The same, with the disabled job SECOND: its `if: false` used to be attributed to the PREVIOUS
  // job's last step (`bash scripts/d.test.sh`), which is a false accusation against a fine step.
  const root = jobFixture([
    "jobs:",
    "  a:",
    "    runs-on: ubuntu-latest",
    "    steps:",
    ...jobBody(),
    "  b:",
    "    runs-on: ubuntu-latest",
    "    if: false",
    "    steps:",
    "      - run: node scripts/check_d.cjs",
  ]);
  const r = run(root);
  assert(
    "the disabled job is named, and its artifact is NOT counted as wired",
    r.status !== 0 && /every step of job `b` is disabled/.test(r.out) && /scripts\/check_d\.cjs is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
  assert(
    "...and the PREVIOUS job's last step is not accused of being disabled",
    !/the step `bash scripts\/d\.test\.sh` is disabled/.test(r.out),
    "out=" + r.out.trim().slice(0, 240),
  );
}
{
  // CONTROL: the identical shape with the third job ENABLED is fine.
  const root = jobFixture([
    "jobs:",
    "  a:",
    "    runs-on: ubuntu-latest",
    "    steps:",
    ...jobBody(),
    "  c:",
    "    runs-on: ubuntu-latest",
    "    steps:",
    "      - run: node scripts/check_d.cjs",
  ]);
  const r = run(root);
  assert(
    "CONTROL: an enabled job naming the artifact passes",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}
{
  // A job-level `defaults: {run: {working-directory: …}}` applies to steps that do not set their own
  // — that is what makes the "covered by a runner started in the file's directory" rule work.
  // `tools/sdk/client_test.go` is NOT named by any step: it is covered only because `go test ./...`
  // runs in `tools/sdk`, and that directory arrives through the JOB default.
  const root = skeleton({
    ci: [
      "jobs:",
      "  a:",
      "    runs-on: ubuntu-latest",
      "    defaults:",
      "      run:",
      "        working-directory: tools/sdk",
      "    steps:",
      ...jobBody(),
      "      - run: node scripts/check_d.cjs",
      "      - run: python3 integration/e2e_proxy_test.py",
      "      - run: ./integration/run-crud-local.sh",
      "      - run: go test ./...",
      "      - run: npm test",
      "        working-directory: tools/cli",
    ].join("\n"),
    scripts: { ...wiredScripts, "check_d.cjs": "// c\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      ...otherTests,
      "integration/run-crud-local.sh": "#!/bin/sh\npython3 integration/test_crud.py\n",
    },
  });
  const r = run(root);
  assert(
    "a JOB-level `defaults.run.working-directory` covers the tests in that directory",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}

/* 44-46 (round 150): SELECTING a crate is a whole-word question. `command.includes("-p " + crate)`
 * was satisfied by `-p hydra-server-extra` (and `--features hydra-core-x` for `hydra-core`), so a CI
 * edit that stopped testing a crate — or a typo in the package name — still read as "covered". */
{
  // Only the WRONG package name is selected: `crates/hydra-server/tests/*.rs` must be reported.
  // NOTE: BOTH cargo lines have to be re-pointed — the fixture's `--test boot_listeners … --ignored`
  // line selects `hydra-server` correctly and would silently satisfy the rule on its own (measured:
  // the first version of this case passed for exactly that reason).
  const root = skeleton({
    ci: baseCi().replace(
      "      - run: cargo test -p hydra-core",
      "      - run: cargo test -p hydra-core-extra",
    ).replace(
      "      - run: cargo test -p hydra-server --test boot_listeners -- --ignored",
      "      - run: cargo test -p hydra-server-extra --test boot_listeners -- --ignored",
    ),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "`-p hydra-server-extra` does NOT select crate `hydra-server`",
    r.status !== 0 && /belongs to crate hydra-server, which no `cargo test` command selects/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}
{
  // CONTROL: the long form `--package hydra-core` selects it.
  const root = skeleton({
    ci: baseCi().replace(
      "      - run: cargo test -p hydra-core",
      "      - run: cargo test --package hydra-core\n      - run: cargo test --package hydra-server",
    ),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: `--package <crate>` selects the crate",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}
{
  // CONTROL: `--workspace` still covers every crate.
  const root = skeleton({
    ci: baseCi().replace(
      "      - run: cargo test -p hydra-core",
      "      - run: cargo test --workspace",
    ),
    scripts: wiredScripts,
    crateTests: { ...crateTests },
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: `--workspace` still selects every crate",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}

/* 47-49 (round 151): a GLOB argument runs every file it matches, so it must count as invocation —
 * while a READER that happens to take the same glob must still not count. Measured limit: the real
 * workflow names each test file individually, so the gap was latent; consolidating the steps into one
 * `node --test scripts/*.test.cjs` would have produced ~30 false UNWIREDs. */
{
  const root = skeleton({
    ci: [
      "jobs:",
      "  check:",
      "    steps:",
      "      - run: cargo test --workspace --test boot_listeners -- --ignored",
      "      - run: npx playwright test --config=playwright.config.cjs",
      "      - run: node --test scripts/*.test.cjs",
      "      - run: node scripts/check_a.cjs",
      "      - run: node scripts/check_b.cjs",
      "      - run: node scripts/check_c.cjs",
      "      - run: node scripts/check_d.cjs",
      "      - run: bash scripts/d.test.sh",
    ].join("\n"),
    scripts: { ...wiredScripts, "check_d.cjs": "// c\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "`node --test scripts/*.test.cjs` counts as invoking the files it matches",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}
{
  // CONTROL: a glob handed to a READER (`grep -l`) is not execution.
  const root = skeleton({
    ci: baseCi().replace(
      "      - run: node scripts/a.test.cjs",
      "      - run: grep -l TODO scripts/*.test.cjs || true",
    ),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: `grep -l … scripts/*.test.cjs` still does NOT count (reading is not running)",
    r.status !== 0 && /scripts\/a\.test\.cjs is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}
{
  // CONTROL: a glob that matches nothing of interest does not wire anything up.
  const root = skeleton({
    ci: baseCi().replace(
      "      - run: node scripts/a.test.cjs",
      "      - run: node --test tools/*.spec.mjs",
    ),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: an unrelated glob does not cover the artifact",
    r.status !== 0 && /scripts\/a\.test\.cjs is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}

/* 50-52 (round 153): EVERY floor gets its own case that makes it fail — the discipline from round
 * 133, applied to the three that were still only neutralised by this suite's harness. A floor with no
 * failing case is a floor nobody has ever seen fire, which is how `MIN_E2E_SPECS` came to equal the
 * real count without anyone noticing. */
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root, { CI_WIRING_MIN_CRATE_TESTS: "500" });
  assert(
    "the crate-test floor fires (CI_WIRING_MIN_CRATE_TESTS above the fixture's count)",
    r.status !== 0 && /crate test file\(s\) found \(< 500\)/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root, { CI_WIRING_MIN_E2E_SPECS: "500" });
  assert(
    "the e2e-spec floor fires (CI_WIRING_MIN_E2E_SPECS above the fixture's count)",
    r.status !== 0 && /no e2e specs found under/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: { ...otherTests },
  });
  const r = run(root, { CI_WIRING_MIN_TOOLS: "500" });
  assert(
    "the per-category `tools/` floor fires (a whole category cannot disappear unnoticed)",
    r.status !== 0 && /test artifact\(s\) under `tools\/` \(< 500\)/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}

/* 53 (round 155): the SCRIPT CHAIN reads text, so it must strip comments like every other text this
 * guard reads. Measured: with `integration/run.sh` containing `# TODO: integration/test_new.py is
 * still unwired`, the artifact counted as EXECUTED — the comment saying it is NOT wired became the
 * proof that it is — and deleting that one comment turned the guard red. */
{
  const root = skeleton({
    ci: baseCi('      - run: ./integration/run.sh'),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: {
      ...otherTests,
      "integration/run.sh": "#!/bin/sh\n# TODO: integration/test_new.py is still unwired\npython3 integration/test_crud.py\n",
      "integration/test_new.py": "# not wired yet\n",
      // test_crud.py stays wired through the OTHER shapes so only test_new.py is at stake.
      "integration/run-crud-local.sh": "#!/bin/sh\npython3 integration/test_crud.py\n",
    },
  });
  const r = run(root);
  assert(
    "an artifact named only in a COMMENT inside a CI runner script is NOT executed",
    r.status !== 0 && /integration\/test_new\.py is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}

/* 54 (round 156): `--test <target>` must be a WHOLE WORD, like `selectsCrate` for `-p`. Measured:
 * `c.includes("--test " + target)` was satisfied by a LONGER target name, so `usage_query` counted as
 * run by a step that runs `usage_query_wire` — the `#[ignore]`d suite would stay "wired" forever after
 * a rename while never being executed. */
{
  const ci = [
    "jobs:",
    "  check:",
    "    steps:",
    "      - run: cargo test -p hydra-server --test boot_listeners -- --ignored",
    "      - run: cargo test -p hydra-server --test usage_query_wire -- --ignored",
    "      - run: npx playwright test --config=playwright.config.cjs",
    "      - run: node scripts/a.test.cjs",
    "      - run: node scripts/check_a.cjs",
    "      - run: node scripts/check_b.cjs",
    "      - run: node scripts/check_c.cjs",
    "      - run: node scripts/check_d.cjs",
    "      - run: bash scripts/d.test.sh",
  ].join("\n");
  const root = skeleton({
    ci,
    scripts: { ...wiredScripts, "check_d.cjs": "// c\n" },
    crateTests: {
      ...crateTests,
      "crates/hydra-server/tests/usage_query.rs": "#[test]\n#[ignore]\nfn slow() {}\n",
      "crates/hydra-server/tests/usage_query_wire.rs": "#[test]\n#[ignore]\nfn slow() {}\n",
    },
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "`--test usage_query_wire` does NOT count as running the `usage_query` target",
    r.status !== 0 && /usage_query\.rs contains #\[ignore\] tests but no step runs/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}
{
  // CONTROL: the exact target name DOES count.
  const root = skeleton({
    ci: [
      "jobs:",
      "  check:",
      "    steps:",
      "      - run: cargo test -p hydra-server --test boot_listeners -- --ignored",
    "      - run: cargo test -p hydra-server --test usage_query -- --ignored",
      "      - run: npx playwright test --config=playwright.config.cjs",
      "      - run: node scripts/a.test.cjs",
      "      - run: node scripts/check_a.cjs",
      "      - run: node scripts/check_b.cjs",
      "      - run: node scripts/check_c.cjs",
      "      - run: node scripts/check_d.cjs",
      "      - run: bash scripts/d.test.sh",
    ].join("\n"),
    scripts: { ...wiredScripts, "check_d.cjs": "// c\n" },
    crateTests: {
      ...crateTests,
      "crates/hydra-server/tests/usage_query.rs": "#[test]\n#[ignore]\nfn slow() {}\n",
    },
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: the exact `--test usage_query` name counts",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}

/* 55 (round 156): the EXAMINED COUNT is printed. It used to be a dead variable whose comment
 * claimed it was the protection against a broken glob passing silently — a comment describing a
 * mechanism that did not exist. */
{
  const root = skeleton({
    ci: baseCi(),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  // Round 194: the line used to say "N artifact(s) examined" where N was only the test artifacts
  // OUTSIDE scripts/ and crates/<crate>/tests — read after "everything is executed" it looked like
  // the total (measured then: 48 vs the 149 the same run actually judged). The number is now named
  // for its class AND the total is printed, so the assertion checks both halves of the sentence.
  const judged = r.out.match(/OK\s+\(everything is executed — (\d+) test artifact\(s\) outside [^,]+,\s+(\d+) artifact\(s\) judged in total across (\d+) classes\)/);
  assert(
    "the OK line names the class it counts and reports the judged total (not a dead counter)",
    r.status === 0 && judged !== null && Number(judged[2]) >= Number(judged[1]),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 56-57 (round 158): a line starting with `- ` INSIDE a `run: |` body is SHELL TEXT, not a new step.
 * The old parser split the step there and dropped the step it started (a step with no `run:` is never
 * emitted), so every command after such a line was LOST: measured with `- printf 1` before the real
 * command, the artifact was reported UNWIRED — a false red pointing at a correct file. */
const BLOCK_WITH_DASH = (realCommand) => [
  "jobs:",
  "  check:",
  "    steps:",
  "      - run: cargo test --workspace",
  "      - run: cargo test -p hydra-server --test boot_listeners -- --ignored",
  "      - run: npx playwright test --config=playwright.config.cjs",
  "      - run: node scripts/a.test.cjs",
  "      - run: node scripts/check_a.cjs",
  "      - run: node scripts/check_b.cjs",
  "      - run: node scripts/check_c.cjs",
  "      - run: node scripts/check_d.cjs",
  "      - run: bash scripts/d.test.sh",
  "      - run: |",
  '          echo "starting"',
  "          - printf 1",
  realCommand ? "          python3 integration/test_old.py" : "          echo nothing-to-run",
].join("\n");
{
  const root = skeleton({
    ci: BLOCK_WITH_DASH(true),
    scripts: { ...wiredScripts, "check_d.cjs": "// c\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: { "integration/test_old.py": "# old\n" },
  });
  const r = run(root);
  assert(
    "a `- ` line inside a `run: |` body does not hide the commands after it",
    r.status === 0,
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}
{
  // CONTROL: with the command actually absent the same block shape is still reported unwired, so the
  // case above passes because the command is seen — not because the artifact stopped being required.
  const root = skeleton({
    ci: BLOCK_WITH_DASH(false),
    scripts: { ...wiredScripts, "check_d.cjs": "// c\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    extraFiles: { "integration/test_old.py": "# old\n" },
  });
  const r = run(root);
  assert(
    "CONTROL: the same block shape without the command is still UNWIRED",
    r.status !== 0 && /integration\/test_old\.py is never executed/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 200),
  );
}

/* 58 (round 159): ONE owner for "what is a step block". `extractSteps` and `stepShapeProblems` each
 * had their own scanner, and the shape checker cut blocks at ANY `- ` line — including one inside a
 * `run: |` body. Measured with a step whose duplicate `run:` key sits AFTER such a line: the duplicate
 * landed in another block and the checker stayed silent, so a step that merged into the one above
 * (which GitHub REJECTS) went unnoticed. */
{
  const ci = [
    "jobs:",
    "  check:",
    "    steps:",
    "      - run: cargo test --workspace",
    // The merged-step shape: the second `run:` belongs to the SAME step (no `- ` marker).
    "      - name: first",
    "        run: |",
    '          echo "starting"',
    "          - printf 1",
    "        run: node scripts/a.test.cjs",
    "      - run: npx playwright test --config=playwright.config.cjs",
  ].join("\n");
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "a duplicate `run:` key AFTER a `- ` line inside a block scalar is still detected",
    r.status !== 0 && /has 2 `run:` keys/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}

{
  // ...and the body-skip must not go the other way either: a block scalar that WRITES YAML from shell
  // (a heredoc containing a `run:` line) is not a duplicate key. Without skipping the body when
  // counting keys this is a FALSE POSITIVE ("this step has 2 `run:` keys") against a correct file.
  const ci = [
    "jobs:",
    "  check:",
    "    steps:",
    "      - run: cargo test --workspace",
    "      - name: writes yaml",
    "        run: |",
    "          cat <<'YAML' > /tmp/step.yml",
    "          run: something",
    "          env: OTHER",
    "          YAML",
    "      - run: npx playwright test --config=playwright.config.cjs",
  ].join("\n");
  const root = skeleton({
    ci,
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "a `run:` line INSIDE a block scalar body is not a duplicate key (false positive guard)",
    !/has \d+ `run:` keys/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 240),
  );
}

/* 59-60 (round 161): a step with NEITHER `run:` nor `uses:` executes nothing, and GitHub's workflow
 * schema requires one of the two — so the file is not merely useless, it is rejected as a whole.
 * Measured on the real `ci.yml`: a leftover `- name: "The ClickHouse end-to-end tests …"` step (its
 * work is done by a later step) sat there while every local guard stayed quiet, because the shape
 * checker only looked for DUPLICATE keys. */
{
  const root = skeleton({
    ci: [
      "jobs:",
      "  check:",
      "    steps:",
      "      - run: cargo test --workspace",
      '      - name: "a step with no run"',
      "      - run: npx playwright test --config=playwright.config.cjs",
    ].join("\n"),
    scripts: wiredScripts,
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "a step with neither `run:` nor `uses:` is reported (GitHub rejects the whole file)",
    r.status !== 0 && /has neither `run:` nor `uses:`/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}
{
  // CONTROL: a list item OUTSIDE `steps:` (a service's `ports:` entry) is not a step. Without the
  // `inSteps` scoping this fired on the real file's `- 8123:8123` (measured).
  const root = skeleton({
    ci: [
      "jobs:",
      "  check:",
      "    runs-on: ubuntu-latest",
      "    services:",
      "      clickhouse:",
      "        image: clickhouse/clickhouse-server:24.3",
      "        ports:",
      "          - 8123:8123",
      "    steps:",
      "      - run: cargo test --workspace",
      "      - run: cargo test -p hydra-server --test boot_listeners -- --ignored",
      "      - run: npx playwright test --config=playwright.config.cjs",
      "      - run: node scripts/a.test.cjs",
      "      - run: node scripts/check_a.cjs",
      "      - run: node scripts/check_b.cjs",
      "      - run: node scripts/check_c.cjs",
      "      - run: node scripts/check_d.cjs",
      "      - run: bash scripts/d.test.sh",
    ].join("\n"),
    scripts: { ...wiredScripts, "check_d.cjs": "// c\n" },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
  });
  const r = run(root);
  assert(
    "CONTROL: a list item outside `steps:` (a service port) is not judged as a step",
    r.status === 0 && !/has neither `run:` nor `uses:`/.test(r.out),
    "status=" + r.status + " out=" + r.out.trim().slice(0, 220),
  );
}

/* 44-49 (round 168): the REST of `scripts/*.sh`. Rule 1 owns `*.test.*` and `check_*.cjs`, and
 * measured 2026-10-01 two working scripts fell outside every rule: `scripts/e2e-local.sh` (a browser
 * suite only a human ever ran) and `scripts/load_test.sh` (its judge self-checks had only ever been
 * verified by hand). The new rule requires an INVOCATION by ci.yml OR by the local gate script,
 * unless a WIRED file calls the script as a helper.
 *
 * NOTE on the fixtures: each case carries ONLY the extra scripts it is about — a leftover
 * `helper.sh` in the gate case is itself an orphan and reddens the case for an unrelated reason
 * (measured: the first version of case 45 did exactly that and "failed" while proving nothing). */
{
  const skel = (scripts, extra = {}) => skeleton({
    ci: baseCi().replace("bash scripts/d.test.sh", "bash scripts/d.test.sh\n      - run: bash scripts/c.test.sh"),
    scripts: { ...wiredScripts, "c.test.sh": "#!/bin/sh\necho ok\n", ...scripts },
    crateTests,
    e2e: ["a.spec.cjs", "b.spec.cjs", "c.spec.cjs"],
    playwright: "module.exports = { testDir: './tests/e2e' };\n",
    ...extra,
  });

  // 44: nothing runs it. (`c.test.sh` is wired but never mentions it.)
  const r44 = run(skel({ "orphan.sh": "#!/bin/sh\n" }));
  assert(
    "44: a `scripts/*.sh` executed by nothing is UNWIRED",
    r44.status === 1 && /scripts\/orphan\.sh is executed by NOTHING/.test(r44.out),
    "status=" + r44.status + " out=" + r44.out.trim().slice(0, 220),
  );

  // 45: CONTROL — the local gate is a valid runner (several scripts are local-only by design).
  const r45 = run(skel({ "orphan.sh": "#!/bin/sh\n" }, {
    extraFiles: { ".acceptance/round10-gate.sh": "#!/usr/bin/env bash\ngate \"x\" bash -c 'bash scripts/orphan.sh'\n" },
  }));
  assert(
    "45: CONTROL — the same script invoked by the local gate passes",
    r45.status === 0 && !/orphan\.sh is executed by NOTHING/.test(r45.out),
    "status=" + r45.status + " out=" + r45.out.trim().slice(0, 300),
  );

  // 45b: RECORDED — a script CI does not run can be exempted on purpose, and the reason is printed
  //      every run. This is the case the REAL tree needs: the local gate runs `e2e-local.sh` and a
  //      runner has no gate file, so without a record the same tree is green on one machine and red
  //      on the other (measured 2026-10-07, the first CI run to reach rule 1b).
  const RECORD_SKELETON = { "orphan.sh": "#!/bin/sh\n" };
  const r45b = run(skel(RECORD_SKELETON), {
    CIW_NOT_EXECUTED_OK: JSON.stringify({ "orphan.sh": "local-only runner; CI inlines the same suite" }),
  });
  assert(
    "45b: a RECORDED not-executed-in-CI script passes",
    r45b.status === 0 && !/orphan\.sh is executed by NOTHING/.test(r45b.out),
    "status=" + r45b.status + " out=" + r45b.out.trim().slice(0, 300),
  );
  assert(
    "...and the record's reason is printed, so the exemption is visible rather than silent",
    /note {2}scripts\/orphan\.sh is not executed by CI, recorded on purpose: local-only runner/.test(r45b.out),
    r45b.out.trim().slice(0, 300),
  );

  // 45c: FALSIFY — once ci.yml invokes it, the record is STALE. A record that cannot expire is a
  //      claim about the past wearing the present tense. (The ci below keeps `c.test.sh` wired the
  //      way `skel` wires it, so this case isolates the record's staleness.)
  const r45c = run(skel(RECORD_SKELETON, {
    ci: baseCi().replace(
      "bash scripts/d.test.sh",
      "bash scripts/d.test.sh\n      - run: bash scripts/c.test.sh\n      - run: bash scripts/orphan.sh",
    ),
  }), { CIW_NOT_EXECUTED_OK: JSON.stringify({ "orphan.sh": "local-only runner; CI inlines the same suite" }) });
  assert(
    "45c: FALSIFY — a record for a script that ci.yml DOES run is reported as stale",
    r45c.status === 1 && /orphan\.sh is RECORDED as not-executed-in-CI, but ci.yml invokes it now/.test(r45c.out),
    "status=" + r45c.status + " out=" + r45c.out.trim().slice(0, 300),
  );

  // 46: CONTROL — a helper a WIRED script calls is exempt, and the exemption is reported.
  const r46 = run(skel({
    "helper.sh": "#!/bin/sh\n",
    "c.test.sh": '#!/bin/sh\nSCRIPT="scripts/helper.sh"\nbash "$SCRIPT"\n',
  }));
  assert(
    "46: CONTROL — a helper called by a wired script passes, and is reported as such",
    r46.status === 0 && /1 reached only as a helper of a wired file: helper\.sh <- scripts\/c\.test\.sh/.test(r46.out),
    "status=" + r46.status + " out=" + r46.out.trim().slice(0, 300),
  );

  // 47: a SHELL COMMENT is not a call. This is the shape that made the first version of the rule
  // pass on the real tree: the guard's OWN comment named the script it was supposed to flag.
  const r47 = run(skel({
    "orphan.sh": "#!/bin/sh\n",
    "c.test.sh": "#!/bin/sh\n# TODO: scripts/orphan.sh is still unwired\necho ok\n",
  }));
  assert(
    "47: a mention inside a shell comment does not count as calling it",
    r47.status === 1 && /scripts\/orphan\.sh is executed by NOTHING/.test(r47.out),
    "status=" + r47.status + " out=" + r47.out.trim().slice(0, 220),
  );

  // 48: neither does a JS BLOCK comment (this guard's own header does exactly that).
  const r48 = run(skel({
    "orphan.sh": "#!/bin/sh\n",
    "check_c.cjs": "/* scripts/orphan.sh is not wired */\n// c\n",
  }));
  assert(
    "48: a mention inside a JS block comment does not count either",
    r48.status === 1 && /scripts\/orphan\.sh is executed by NOTHING/.test(r48.out),
    "status=" + r48.status + " out=" + r48.out.trim().slice(0, 220),
  );

  // 49: the floor, with its own explicit value.
  const r49 = run(skel({ "orphan.sh": "#!/bin/sh\n" }), { CI_WIRING_MIN_SHELL_SCRIPTS: "9" });
  assert(
    "49: the shell-script floor refuses to pass when the glob reaches almost nothing",
    r49.status === 1 && /only \d+ `scripts\/\*\.sh` file\(s\) examined \(< 9\)/.test(r49.out),
    "status=" + r49.status + " out=" + r49.out.trim().slice(0, 220),
  );
}

console.log(failures === 0 ? "\nALL CI WIRING TESTS PASSED" : "\n" + failures + " CI WIRING TEST(S) FAILED");
process.exit(failures ? 1 : 0);
