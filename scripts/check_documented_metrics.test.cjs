#!/usr/bin/env node
'use strict';
/**
 * Tests for `check_documented_metrics.cjs` — both directions, the three registration shapes, and
 * the honest "cannot verify" exits.
 *
 * Case 2 is the one that matters most: it is the FALSE NEGATIVE this guard was written after. A
 * hand audit of the real docs flagged `hydra_sni_host_mismatch_total` as "documented but not
 * registered" because the counter is registered from a CONSTANT
 * (`register_int_counter!(MISMATCH_METRIC, …)`), and the audit only understood the literal form.
 * A guard with that hole would have produced a false alarm on a correct document.
 */
const { execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const SCRIPT = path.join(__dirname, 'check_documented_metrics.cjs');
const failures = [];
// Counted so the summary cannot overstate how much was checked (it used to be a hardcoded number).
let checks = 0;

function check(label, ok, detail = '') {
  checks++;
  console.log(`   ${ok ? 'PASS' : 'FAIL'}  ${label}${detail ? '  — ' + detail : ''}`);
  if (!ok) failures.push(label);
}

function tree(files) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'cdm-'));
  for (const [rel, body] of Object.entries(files)) {
    const full = path.join(root, rel);
    fs.mkdirSync(path.dirname(full), { recursive: true });
    fs.writeFileSync(full, body);
  }
  return root;
}

function run(files, extraEnv = {}) {
  const root = tree(files);
  try {
    const out = execFileSync('node', [SCRIPT], {
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'pipe'],
      env: {
        ...process.env,
        CDM_DOCS_ROOT: root,
        CDM_DOCS: 'dev-docs/ops.md',
        CDM_CRATES: 'crates',
        CDM_MIN_CHECKED: '1',
        // The NOT_A_METRIC list is REPLACED (not extended) by this override (a JSON object of
        // name → reason since round 184): a fixture tree is a few
        // files under a temp root, so the repository's crate-name exclusions would look unjustified
        // there (and the rule that checks them would fire for the wrong reason). Cases that need an
        // exclusion pass their own JSON.
        CDM_NOT_A_METRIC: '{}',
        // Same reason, same rule, for the OTHER recorded list: it is REPLACED here so a fixture
        // never inherits this repository's "deliberately absent" names (see the guard's note — the
        // merge that used to happen here turned eleven cases red the moment the built-in list
        // stopped being empty).
        CDM_ABSENT_ON_PURPOSE: '{}',
        ...extraEnv,
      },
    });
    return { status: 0, out };
  } catch (e) {
    return { status: e.status === undefined ? 1 : e.status, out: (e.stdout || '') + (e.stderr || '') };
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

const doc = (body) => ({ 'dev-docs/ops.md': body + '\n' });

// 1. a literal registration
let r = run({
  ...doc('Key series: `hydra_requests_total`.'),
  'crates/hydra-server/src/metrics.rs':
    'let c = register_int_counter_vec!("hydra_requests_total", "h", &["a"]).ok()?;\n',
});
check('a name registered with a LITERAL resolves', r.status === 0, `exit=${r.status} ${r.out.trim().slice(0, 90)}`);

// 2. a CONSTANT registration — the false negative that motivated the guard
r = run({
  ...doc('Key series: `hydra_sni_host_mismatch_total`.'),
  'crates/hydra-server/src/tls.rs':
    'const MISMATCH_METRIC: &str = "hydra_sni_host_mismatch_total";\n' +
    'fn c() { prometheus::register_int_counter!(MISMATCH_METRIC, "help").ok() }\n',
});
check('a name registered through a CONSTANT resolves (the false negative the guard exists for)',
  r.status === 0, `exit=${r.status} ${r.out.trim().slice(0, 110)}`);

/* Round 152: the constant's TYPE annotation must not matter. The resolver required `: &str`, so a
 * `&'static str` declaration (or a `static`) left the metric looking UNREGISTERED — a documented
 * series reported as DRIFT while the code registers it. Latent today: every real declaration is
 * `: &str`.
 * NOTE what is NOT claimed: a `String` built at runtime (`String::from("…")`, `OnceLock`) is not
 * resolved by this text-based reader, and the first version of this test pretended otherwise with a
 * `String::new()` fixture that could never have matched. */
for (const [label, body] of [
  ['a `&\'static str` const',
    'pub const MISMATCH_METRIC: &\'static str = "hydra_sni_host_mismatch_total";\n'
    + 'fn c() { prometheus::register_int_counter!(MISMATCH_METRIC, "help").ok() }\n'],
  ['a `pub(crate) static`',
    'pub(crate) static MISMATCH_METRIC: &\'static str = "hydra_sni_host_mismatch_total";\n'
    + 'fn c() { prometheus::register_int_counter!(MISMATCH_METRIC, "help").ok() }\n'],
]) {
  const rr = run({
    ...doc('Key series: `hydra_sni_host_mismatch_total`.'),
    'crates/hydra-server/src/tls.rs': body,
    // A SECOND, ordinary registration: without it the fixture's only registration is the one under
    // test, so the failure mode is `CANNOT VERIFY: no registered metric found` instead of the
    // documented-name-is-missing DRIFT this case is about (measured — the first version of this case
    // asserted the right exit code for the wrong reason).
    'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n',
  });
  check(`${label} resolves too`, rr.status === 0, `exit=${rr.status} ${rr.out.trim().slice(0, 130)}`);
}
{
  // CONTROL: a declaration whose VALUE is not a metric name must still not register anything.
  const rr = run({
    ...doc('Key series: `hydra_not_here_total`.'),
    'crates/hydra-server/src/tls.rs':
      'pub const OTHER: &\'static str = "not_a_metric_name";\n'
      + 'fn c() { prometheus::register_int_counter!(OTHER, "help").ok() }\n',
    'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n',
  });
  check('CONTROL: a constant pointing at a non-metric name does not resolve the documented one',
    rr.status === 1 && /hydra_not_here_total. is not registered/.test(rr.out),
    `exit=${rr.status} ${rr.out.trim().slice(0, 150)}`);
}

// 3. an Opts registration
r = run({
  ...doc('Key series: `hydra_invalidation_trimmed_total`.'),
  'crates/hydra-server/src/x.rs':
    'let o = Opts::new("hydra_invalidation_trimmed_total", "help");\n',
});
check('a name registered through `Opts::new` resolves', r.status === 0, `exit=${r.status}`);

// 4. a typo is caught, named, and gets suggestions
r = run({
  ...doc('Key series: `hydra_requests_totals`.'),
  'crates/hydra-server/src/metrics.rs':
    'let c = register_int_counter_vec!("hydra_requests_total", "h", &["a"]).ok()?;\n',
});
check('a typo is reported as DRIFT with its file:line and a suggestion',
  r.status === 1 && /dev-docs\/ops\.md:1/.test(r.out) && /hydra_requests_total/.test(r.out),
  `exit=${r.status} ${r.out.trim().split('\n')[0].slice(0, 130)}`);

// 5/6. a wildcard prefix and a crate name are not series. The doc ALSO carries one real name: with
//      nothing but skipped tokens the coverage floor would (correctly) report CANNOT VERIFY, since
//      a scan that checked zero names is not evidence of anything.
r = run({
  ...doc('There is deliberately no `hydra_proxy_listener_*` alias; see `hydra_core` and '
    + '`hydra_server`. Key series: `hydra_requests_total`.'),
  'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n',
  // The two crate names must be excluded EXPLICITLY since round 181 (the harness no longer inherits the
  // repository's list): that is the point of the case, and the OK line now reports how many names the
  // exclusion actually skipped.
}, { CDM_NOT_A_METRIC: '{"hydra_core":"the crate","hydra_server":"the crate"}' });
check('a wildcard prefix (`hydra_x_*`) and crate names are ignored',
  r.status === 0 && /2 crate\/tool name\(s\) skipped by NOT_A_METRIC \(hydra_core, hydra_server\)/.test(r.out),
  `exit=${r.status} ${r.out.trim().slice(0, 140)}`);

// 7. the allowlist — and its STALENESS rule (round 144). The built-in map is empty on purpose: the
//    real note in ops.md §9.1 is a wildcard, so the four entries that used to live in the guard were
//    never consulted while the OK line advertised them as "deliberately absent" (measured).
r = run({
  ...doc('There is deliberately no `hydra_proxy_listener_bound` alias.'),
  'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n',
}, { CDM_MIN_CHECKED: '0', CDM_ABSENT_ON_PURPOSE: '{"hydra_proxy_listener_bound":"spelled as absent in the fixture doc"}' });
check('an allowlisted "deliberately absent" name does not fail the guard',
  r.status === 0 && /1 of 1 allowlisted name\(s\) were actually needed/.test(r.out),
  `exit=${r.status} ${r.out.trim().slice(0, 140)}`);

r = run({
  ...doc('There is deliberately no `hydra_proxy_listener_*` alias. Key series: `hydra_requests_total`.'),
  'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n',
}, { CDM_MIN_CHECKED: '0', CDM_ABSENT_ON_PURPOSE: '{"hydra_proxy_listener_bound":"spelled as absent in the fixture doc"}' });
check('...but an entry nobody needs is DRIFT (a dead allowlist entry is where a drift would hide)',
  r.status === 1 && /never consulted/.test(r.out) && /hydra_proxy_listener_bound/.test(r.out),
  `exit=${r.status} ${r.out.trim().split('\n')[0].slice(0, 140)}`);

r = run({
  ...doc('There is deliberately no `hydra_proxy_listener_*` alias. Key series: `hydra_requests_total`.'),
  'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n',
}, { CDM_MIN_CHECKED: '0' });
check('the WILDCARD note passes on its own (skipped token), not via an allowlist entry',
  r.status === 0 && /1 wildcard prefix\(es\) skipped/.test(r.out) && /0 of 0 allowlisted/.test(r.out),
  `exit=${r.status} ${r.out.trim().slice(0, 150)}`);

// 8. a missing document
r = run({ 'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n' });
check('a missing document exits 2 (never 0)', r.status === 2, `exit=${r.status} ${r.out.trim().slice(0, 90)}`);

// 9. the coverage floor
r = run({
  ...doc('Key series: `hydra_requests_total`.'),
  'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n',
}, { CDM_MIN_CHECKED: '25' });
check('the coverage floor turns a small scan into CANNOT VERIFY (never 0)',
  r.status === 2 && /only 1 DISTINCT documented name/.test(r.out), `exit=${r.status} ${r.out.trim().slice(0, 110)}`);

// 10. no registered metrics at all
r = run({ ...doc('Key series: `hydra_requests_total`.') });
check('a tree with no registered metric exits 2 (the scan is broken, not clean)',
  r.status === 2, `exit=${r.status} ${r.out.trim().slice(0, 90)}`);

// 11. A registration inside `#[cfg(test)]` is NOT a live series. `admin/metrics.rs` really does
//     contain `register_int_counter!("hydra_unused_test_marker", "test")` in its test module, and
//     the guard used to count it — so a documented panel pointing at it would have "resolved"
//     while no deployment ever exports the series. (The char literal in the fixture is deliberate:
//     it is what used to derail the brace matching that finds the end of a test item.)
r = run({
  // The live registration keeps the scan non-empty: with NO registered name at all the guard
  // reports CANNOT VERIFY (exit 2) before it ever reaches the drift check — also a failure, but a
  // different one, and this case is about the drift.
  ...doc('Key series: `hydra_only_in_tests_total`, `hydra_live_total`.'),
  'crates/hydra-server/src/metrics.rs':
    "register_int_counter!(\"hydra_live_total\", \"h\");\n#[cfg(test)]\nmod tests {\n    const L: char = '{';\n    #[test]\n    fn t() { register_int_counter!(\"hydra_only_in_tests_total\", \"h\"); }\n}\n",
});
check(
  'a metric registered ONLY inside #[cfg(test)] does not count as registered',
  r.status === 1 && /hydra_only_in_tests_total/.test(r.out) && /is not registered anywhere/.test(r.out),
  `exit=${r.status} ${r.out.trim().slice(0, 130)}`,
);

// 12. CONTROL for 11: the same registration OUTSIDE the test module resolves (otherwise case 11
//     could be passing because the scan broke, e.g. because string contents were blanked).
r = run({
  ...doc('Key series: `hydra_live_total`.'),
  'crates/hydra-server/src/metrics.rs':
    "register_int_counter!(\"hydra_live_total\", \"h\");\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
});
check(
  'CONTROL: the same literal registration outside the test module resolves',
  r.status === 0, `exit=${r.status} ${r.out.trim().slice(0, 130)}`,
);

// 13. A registration in `crates/<crate>/tests/` is compiled only by `cargo test` — same rule.
r = run({
  ...doc('Key series: `hydra_only_in_tests_total`.'),
  'crates/hydra-server/tests/helpers.rs':
    'register_int_counter!("hydra_only_in_tests_total", "h");\n',
  'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_live_total", "h");\n',
});
check(
  'a metric registered only under a crate `tests/` directory does not count either',
  r.status === 1, `exit=${r.status} ${r.out.trim().slice(0, 130)}`,
);

// 14. A registration that appears only in a COMMENT is not a registration (the same rule that
//     makes the `#[cfg(test)]` case work: the scan reads code, not prose).
r = run({
  ...doc('Key series: `hydra_only_in_comment_total`.'),
  'crates/hydra-server/src/metrics.rs':
    '// register_int_counter!("hydra_only_in_comment_total", "h");\nregister_int_counter!("hydra_live_total", "h");\n',
});
check(
  'a registration mentioned only in a comment does not count',
  r.status === 1, `exit=${r.status} ${r.out.trim().slice(0, 130)}`,
);

// 15. Round 136: a drift AND a broken coverage floor must be reported in the same run (the floor used
//     to print alone and hide the drift; exit stays 2 because "did not really run" outranks "drifted").
r = run({
  ...doc('Key series: `hydra_live_total`, `hydra_typo_total`.'),
  'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_live_total", "h");\n',
}, { CDM_MIN_CHECKED: '50' });
check(
  'a drift AND a broken coverage floor are both reported (exit 2)',
  r.status === 2 && /hydra_typo_total/.test(r.out) && /CANNOT VERIFY/.test(r.out),
  `exit=${r.status} ${r.out.trim().slice(0, 150)}`,
);


/* Round 156: the floor counts DISTINCT names, not occurrences. Measured on the real docs: 69 mentions
 * of 36 distinct names — so an occurrence floor could be satisfied by one series repeated, and the OK
 * line called the occurrence count "N documented metric name(s)". */
{
  const many = Array.from({ length: 25 }, () => '`hydra_requests_total`').join(' ');
  const r = run({
    ...doc(`Key series: ${many}.`),
    'crates/hydra-server/src/metrics.rs': 'register_int_counter!("hydra_requests_total", "h");\n',
  }, { CDM_MIN_CHECKED: '20' });
  check('one name mentioned 25× does NOT satisfy the distinct-name floor (it used to)',
    r.status === 2 && /only 1 DISTINCT documented name/.test(r.out),
    `exit=${r.status} ${r.out.trim().split('\n')[0].slice(0, 130)}`);
}

console.log();
/* Round 181: NOT_A_METRIC is an exclusion list, and an exclusion nobody uses is where a
 * documented-but-absent metric hides (a name in it is skipped BEFORE the registration lookup). Every
 * entry must therefore either skip something the scanned docs really mention, or name a crate / tool /
 * package in this tree. Measured when the rule was added: `hydra_dev` and `hydra_ui` qualified on
 * neither count and were deleted. */
check(
  'an exclusion that skips nothing and justifies nothing is reported',
  (() => {
    const r = run({
      'dev-docs/ops.md': '| Series | Notes |\n|---|---|\n| `hydra_really_missing_total` | x |\n',
      'crates/hydra-core/src/lib.rs': 'register_int_counter!("hydra_really_missing_total", "h");\n',
    }, { CDM_NOT_A_METRIC: '{"hydra_ghost_exclusion":"recorded but used by nothing"}' });
    return r.status === 1 && /hydra_ghost_exclusion is in NOT_A_METRIC but skips NOTHING/.test(r.out);
  })(),
  'the dead exclusion must be named',
);

check(
  'CONTROL: an exclusion that IS mentioned by the docs is accepted and counted',
  (() => {
    const r = run({
      'dev-docs/ops.md': 'The `hydra_sdk` package exposes nothing; the series is `hydra_ok_total`.\n'
        + '| Series | Notes |\n|---|---|\n| `hydra_ok_total` | x |\n',
      'crates/hydra-core/src/lib.rs': 'register_int_counter!("hydra_ok_total", "h");\n',
    }, { CDM_NOT_A_METRIC: '{"hydra_sdk":"the Python package"}' });
    return r.status === 0 && /1 crate\/tool name\(s\) skipped by NOT_A_METRIC \(hydra_sdk\)/.test(r.out);
  })(),
  'a used exclusion passes and is printed',
);

check(
  'CONTROL: an exclusion justified by a REAL directory is accepted even when the docs never mention it',
  (() => {
    const r = run({
      'dev-docs/ops.md': '| Series | Notes |\n|---|---|\n| `hydra_ok_total` | x |\n',
      'crates/hydra-core/src/lib.rs': 'register_int_counter!("hydra_ok_total", "h");\n',
      'crates/hydra-ghost/src/lib.rs': '// a crate directory carrying that name\n',
    }, { CDM_NOT_A_METRIC: '{"hydra_ghost":"a crate directory carries it"}' });
    return r.status === 0;
  })(),
  'the crate-directory justification counts',
);

if (failures.length) {
  console.log(`documented-metrics guard tests: FAILED (${failures.length}): ${failures.join('; ')}`);
  process.exit(1);
}
// The count is COMPUTED, not written down: the hardcoded "14 assertions" kept saying 14 after three
// more were added, i.e. the summary reported work that had not been counted (round 152).
console.log(`documented-metrics guard tests: PASSED (${checks} assertions, all three registration shapes `
  + '+ both directions + test-only registrations + constant declarations + cannot-verify)');
