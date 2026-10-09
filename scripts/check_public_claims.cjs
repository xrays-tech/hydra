#!/usr/bin/env node
'use strict';
/**
 * Public numeric claims must equal what the suites actually report.
 *
 * `docs/index.html` is a public page (GitHub Pages) and its stat band advertises
 * an exact Rust test count, e.g.
 *
 *   <div class="note" data-l="zh">797 项 Rust 测试 · ... （2026-09-29）</div>
 *   <div class="note" data-l="en">797 Rust tests · ... (2026-09-29)</div>
 *
 * That number is hand-edited and has already rotted twice (287 → 796 while the
 * suites grew, then 796 → 797 within the same day as the edit). Nothing else in
 * the repo compares it against reality, so a public page could advertise a
 * stale size indefinitely.
 *
 * This checker compares the advertised count against the `test result:` lines
 * produced by the two Rust suites CI already runs:
 *
 *   cargo test -p hydra-core                              | tee core.log
 *   cargo test -p hydra-server --features server          | tee server.log
 *   node scripts/check_public_claims.cjs --core-log=core.log --server-log=server.log
 *
 * `--write` may not CHANGE the advertised count unless it MEASURED it (round 134). With
 * `--core-log/--server-log` it used to rewrite the page to whatever those transcripts said and exit 0
 * — a check that satisfies itself, and the one invocation a CI step must never have. The default is
 * therefore to refuse that shape (see `mayRewriteFromLogs`); `--measure --write` is the refresh path,
 * and `PUBLIC_CLAIMS_ALLOW_LOG_WRITE=1` is the explicit, greppable hook the test suite uses to keep
 * the rewrite mechanics covered.
 *
 * The measurement DATE must not be a LIE, and nothing more (rounds 126/127). Two hard checks: the
 * date may not be in the future, and under `--measure` it must be today (that is exactly what
 * `--write` stamps). A date that is merely OLD is a printed NOTE, not a failure — the first version
 * made it a failure and that was wrong: CI regenerates the transcripts on every run, so a month in
 * which the test count does not change would have turned a CORRECT page red, fixable only by a
 * commit that bumps a date (measured 2026-09-30 on a page whose counts and gate claims were exactly
 * right: exit 1). Finally, `--write` used to stamp UTC (`toISOString()`) while the check compared
 * local dates — one value, two sources; both now use `localDate()`.
 *
 * It can also measure and refresh by itself:
 *
 *   node scripts/check_public_claims.cjs --measure --write
 *
 * Exit codes:
 *   0  every claim matches (with `--write`: the page was refreshed AND nothing unfixable remains)
 *   1  a claim is stale, missing, or the two locales disagree
 *   2  the measurement is missing/unusable (no logs, truncated, failing suite)
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');

const ROOT = path.resolve(__dirname, '..');
const DEFAULT_DOCS = path.join(ROOT, 'docs', 'index.html');

// A truncated or empty log would otherwise agree with a claim of "0 tests".
const MIN_TESTS = Number(process.env.PUBLIC_CLAIMS_MIN_TESTS || 100);

const CLAIM_ZH = /(\d+)\s*项\s*Rust\s*测试/;
const CLAIM_EN = /(\d+)\s+Rust\s+tests/i;
/** `YYYY-MM-DD` in LOCAL time (the page and the logs are read by a human in one place). */
function localDate(d) {
  const p = (n) => String(n).padStart(2, "0");
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
}

/** `YYYY-MM-DD` shifted by `days` (string in, string out — no Date parsing surprises). */
function addDays(iso, days) {
  const [y, m, d] = iso.split("-").map(Number);
  return localDate(new Date(y, m - 1, d + days));
}

const DATE_ZH = /项\s*Rust\s*测试[^（(]*[（(](\d{4}-\d{2}-\d{2})[)）]/;
const DATE_EN = /Rust\s+tests[^(]*\((\d{4}-\d{2}-\d{2})\)/i;
// The perf section advertises the CI correctness gate as "<N> core + <M> server
// tests". Those are per-suite numbers, so they rot independently of the total —
// and they DID: on 2026-09-29 the page still said "114 core + 173 server" while
// the suites reported 253 and 544. The checker already receives both transcripts,
// so it can verify this claim exactly, not just the sum.
const CLAIM_GATE = /(\d+)\s*core\s*\+\s*(\d+)\s*server/gi;
// ...and that claim carries its OWN measurement date. The refresh path rewrote the counts in both
// places and the date in only ONE, so the page went on to assert a date on which those numbers did
// not exist (measured 2026-10-01: `--write` produced `258 core + 555 server 测试（2026-09-30 计数）`
// and `(counted 2026-09-30)` — 555 was measured that day, not the day before). The claim and the
// headline are two statements of ONE measurement, so their dates must be the same date. Equality
// (not "not-newer") is deliberate: an older claim date is exactly the stale state this catches, and
// the refresh is one command.
const CLAIM_GATE_DATE_ZH = /（(\d{4}-\d{2}-\d{2})\s*计数）/;
const CLAIM_GATE_DATE_EN = /\(counted\s+(\d{4}-\d{2}-\d{2})\)/i;

class ClaimError extends Error {
  constructor(code, message) {
    super(message);
    this.code = code;
  }
}

function parseArgs(argv) {
  const opts = { docs: process.env.PUBLIC_CLAIMS_DOCS || DEFAULT_DOCS, coreLog: null, serverLog: null, measure: false, write: false, help: false };
  for (const arg of argv) {
    if (arg === '--measure') opts.measure = true;
    else if (arg === '--write') opts.write = true;
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else if (arg.startsWith('--docs=')) opts.docs = arg.slice('--docs='.length);
    else if (arg.startsWith('--core-log=')) opts.coreLog = arg.slice('--core-log='.length);
    else if (arg.startsWith('--server-log=')) opts.serverLog = arg.slice('--server-log='.length);
    else throw new Error(`unknown argument: ${arg}`);
  }
  return opts;
}

function usage() {
  console.log(`usage: node scripts/check_public_claims.cjs [--core-log=FILE --server-log=FILE] [--measure] [--write] [--docs=FILE]

Compares the Rust test count advertised in docs/index.html against the counts
reported by the two Rust suites. --measure runs cargo itself, --write rewrites
the advertised number and date to the measured values.`);
}

/** Sum of `passed` over every `test result:` line in a cargo test transcript. */
function summarize(text) {
  const results = [];
  const re = /test result:\s*(\w+)\.\s*(\d+) passed;\s*(\d+) failed;/g;
  let m;
  while ((m = re.exec(text)) !== null) {
    results.push({ status: m[1], passed: Number(m[2]), failed: Number(m[3]) });
  }
  return {
    results,
    passed: results.reduce((a, r) => a + r.passed, 0),
    failed: results.reduce((a, r) => a + r.failed, 0),
  };
}

function readLog(label, file) {
  if (!file) throw new ClaimError(2, `missing --${label} log (see --help)`);
  if (!fs.existsSync(file)) throw new ClaimError(2, `${label} log not found: ${file}`);
  const text = fs.readFileSync(file, 'utf8');
  const sum = summarize(text);
  if (sum.results.length === 0) {
    throw new ClaimError(2, `${label} log has no 'test result:' line (truncated or not a cargo test transcript): ${file}`);
  }
  if (sum.failed > 0) {
    throw new ClaimError(2, `${label} suite did not pass (${sum.failed} failed); its count is not comparable`);
  }
  return sum.passed;
}

function measureSuite(label, args) {
  const cargoArgs = ['test', ...args];
  process.stderr.write(`[claims] measuring ${label}: cargo ${cargoArgs.join(' ')}\n`);
  const res = spawnSync('cargo', cargoArgs, { cwd: ROOT, encoding: 'utf8', maxBuffer: 256 * 1024 * 1024 });
  if (res.error) throw new ClaimError(2, `could not run cargo for ${label}: ${res.error.message}`);
  const text = `${res.stdout || ''}\n${res.stderr || ''}`;
  const sum = summarize(text);
  if (sum.results.length === 0) throw new ClaimError(2, `cargo produced no 'test result:' line for ${label} (exit ${res.status})`);
  if (sum.failed > 0 || res.status !== 0) {
    throw new ClaimError(2, `${label} suite did not pass (exit ${res.status}, ${sum.failed} failed); its count is not comparable`);
  }
  return sum.passed;
}

function main(argv) {
  const opts = parseArgs(argv);
  if (opts.help) {
    usage();
    return 0;
  }

  const html = fs.readFileSync(opts.docs, 'utf8');
  const zh = html.match(CLAIM_ZH);
  const en = html.match(CLAIM_EN);
  if (!zh || !en) {
    const missing = [!zh && 'zh (N 项 Rust 测试)', !en && 'en (N Rust tests)'].filter(Boolean).join(', ');
    console.error(`[claims] FAIL: no advertised Rust test count found in ${path.relative(ROOT, opts.docs)} for: ${missing}`);
    return 1;
  }
  const zhDate = html.match(DATE_ZH);
  const enDate = html.match(DATE_EN);

  let core = null;
  let server = null;
  if (opts.measure) {
    core = measureSuite('hydra-core', ['-p', 'hydra-core']);
    server = measureSuite('hydra-server', ['-p', 'hydra-server', '--features', 'server']);
  } else if (opts.coreLog || opts.serverLog) {
    core = readLog('core-log', opts.coreLog);
    server = readLog('server-log', opts.serverLog);
  }

  if (core === null || server === null) {
    throw new ClaimError(2, `no measurement: pass --core-log/--server-log (or --measure). Advertised: ${zh[1]}`);
  }

  const total = core + server;
  if (total < MIN_TESTS) {
    throw new ClaimError(2, `measured only ${total} tests (< floor ${MIN_TESTS}); refusing to compare (set PUBLIC_CLAIMS_MIN_TESTS to change)`);
  }

  const advertised = { zh: Number(zh[1]), en: Number(en[1]) };
  // `--write` must MEASURE before it changes a NUMBER (round 134): with `--core-log/--server-log` it
  // used to rewrite the page to whatever those files said and exit 0 — a check that satisfies
  // itself, which is the shape a gate command must never have. Refreshing the DATE from transcripts
  // stays allowed (the count is unchanged, so nothing is being "made to pass").
  // The one escape hatch is an EXPLICIT environment variable, so a command that rewrites the count
  // from transcripts is a deliberate, greppable act (`PUBLIC_CLAIMS_ALLOW_LOG_WRITE=1`) and never the
  // default shape of a gate. The test suite sets it to keep the rewrite mechanics covered.
  const mayRewriteFromLogs = opts.measure || process.env.PUBLIC_CLAIMS_ALLOW_LOG_WRITE === '1';
  if (opts.write && !mayRewriteFromLogs && advertised.zh !== total) {
    throw new ClaimError(
      2,
      `--write with --core-log/--server-log would rewrite the advertised count (${advertised.zh}) ` +
        `to match those transcripts (${total}), which is how this check becomes self-satisfying — ` +
        `use \`--measure --write\` to re-measure, or run without --write to check`,
    );
  }
  const problems = [];
  const notes = [];
  // Problems the REWRITE cannot fix: they are about the page's SHAPE (a claim that is gone, a date
  // that is not there to replace), so writing today's numbers changes nothing. They used to be
  // printed with a `(was)` prefix and then the command returned 0 — "reports success while the page
  // is still wrong", which is exactly what the exit-code comment above forbids (round 194,
  // adversarial review; measured: a page with no correctness-gate line at all got `rewrote …` and
  // exit 0, and the very next check of that same file said FAIL).
  const unfixable = [];
  if (advertised.zh !== advertised.en) {
    problems.push(`locales disagree: zh advertises ${advertised.zh}, en advertises ${advertised.en}`);
  }
  if (advertised.zh !== total) {
    problems.push(`advertised ${advertised.zh} but the suites report ${total} (hydra-core ${core} + hydra-server ${server})`);
  }
  if (!zhDate || !enDate) {
    const p = 'missing measurement date next to the count (both locales must carry YYYY-MM-DD)';
    problems.push(p);
    unfixable.push(p);
  } else if (zhDate[1] !== enDate[1]) {
    problems.push(`locales disagree on the measurement date: zh ${zhDate[1]}, en ${enDate[1]}`);
  } else {
    // The date must be TRUE, not merely consistent across locales (round 126). Before this, the
    // only date check was zh-vs-en — both strings come from the same file — so a page could
    // advertise a future date, or a measurement older than the transcripts it is checked against,
    // and still print OK. Two independent checks:
    const today = localDate(new Date());
    if (zhDate[1] > today) {
      problems.push(`the measurement date ${zhDate[1]} is in the future (today is ${today})`);
    }
    const logDates = [opts.coreLog, opts.serverLog]
      .filter(Boolean)
      .map((f) => (fs.existsSync(f) ? localDate(fs.statSync(f).mtime) : null))
      .filter(Boolean)
      .sort();
    const newestLog = logDates.length ? logDates[logDates.length - 1] : null;
    // An OLD date is NOT a failure, and the first version of this check got that wrong: CI
    // regenerates the transcripts on every run, so a month in which the test count does not change
    // would turn a CORRECT page red and the only fix would be a commit that bumps a date (measured
    // 2026-09-30 with a page whose counts and gate claims were exactly right: exit 1). A date is a
    // record of when the count was last measured — being old makes it worth refreshing, not false.
    // It is therefore printed as a note; only claims that are LIES stay hard failures (a future
    // date, and `--measure` not being dated today, since `--write` just stamped it).
    if (newestLog && zhDate[1] < addDays(newestLog, -1)) {
      notes.push(
        `the page is dated ${zhDate[1]} while the transcripts are from ${newestLog}; if the count ` +
          `is unchanged that is fine — refresh the date with --write when you next measure`,
      );
    }
    if (opts.measure && zhDate[1] !== today) {
      problems.push(
        `--measure just measured the suites, so the advertised date must be today (${today}), ` +
          `not ${zhDate[1]} — run with --write to refresh it`,
      );
    }
  }

  // Per-suite correctness-gate claim ("<N> core + <M> server tests").
  const gateClaims = [...html.matchAll(CLAIM_GATE)].map((m) => ({ core: Number(m[1]), server: Number(m[2]) }));
  if (gateClaims.length === 0) {
    const p = 'the correctness-gate claim ("<N> core + <M> server tests") is gone from the page — re-verify the gate wording, then update this checker so it keeps matching something';
    problems.push(p);
    unfixable.push(p);
  } else {
    gateClaims.forEach((g, i) => {
      const label = gateClaims.length > 1 ? ` #${i + 1}` : '';
      if (g.core !== core || g.server !== server) {
        problems.push(`correctness-gate claim${label} says ${g.core} core + ${g.server} server but the suites report ${core} core + ${server} server`);
      }
    });
  }

  // The correctness-gate claim states the same measurement as the headline, INCLUDING its date.
  // Two obligations, the same shape as the count check above: the date must be there (a reworded
  // page must red this checker rather than silently retire the rule), and it must be the advertised
  // date, because that is the measurement the per-suite numbers come from.
  if (gateClaims.length > 0) {
    for (const [locale, re, advertisedDate] of [
      ['zh', CLAIM_GATE_DATE_ZH, zhDate && zhDate[1]],
      ['en', CLAIM_GATE_DATE_EN, enDate && enDate[1]],
    ]) {
      const claimDate = html.match(re);
      if (!claimDate) {
        const p = `the correctness-gate claim carries no measurement date (${locale}) — re-verify the gate wording, then update this checker so it keeps matching something`;
        problems.push(p);
        unfixable.push(p);
      } else if (advertisedDate && claimDate[1] !== advertisedDate) {
        problems.push(`the correctness-gate claim (${locale}) is dated ${claimDate[1]} while the page advertises ${advertisedDate}; those per-suite numbers were measured on the advertised date, so both must carry the same one`);
      }
    }
  }

  // P3-8 (2026-10-09): the README (zh + en) must NOT restate the Rust test
  // counts — it used to advertise "core 114 + server 173" while the suites grew
  // to 262/528 and nothing checked it (the guard only verifies docs/index.html).
  // The README now points at docs/index.html for the exact number; a hand-written
  // count in the README is a claim that can rot exactly like the page did, so
  // re-adding one is a failure this checker has to catch. The expect-count claim
  // is checked by check_source_purity.cjs (same P3-8).
  for (const f of ['README.md', 'README.zh-CN.md']) {
    const p = path.join(ROOT, f);
    if (!fs.existsSync(p)) throw new ClaimError(2, `README missing at ${p}: ${f} must stay verifiable`);
    const text = fs.readFileSync(p, 'utf8');
    if (/\d+\s*(?:core|项\s*Rust)\s*\+\s*\d+\s*server|\d+\s*项\s*Rust\s*测试|\d+\s*Rust\s*tests/i.test(text)) {
      problems.push(
        `${f} re-states a Rust test count in prose — P3-8: the README must point at ` +
          `docs/index.html for the exact number (a hand-written count rots exactly like the page did)`,
      );
    }
  }

  if (problems.length === 0) {
    console.log(`[claims] OK: advertised ${advertised.zh} Rust tests == measured ${total} (hydra-core ${core} + hydra-server ${server}), dated ${zhDate[1]}`);
    console.log(`[claims] OK: correctness-gate claim matches per-suite counts (${gateClaims.length} occurrence(s): ${core} core + ${server} server) and carries the same measurement date`);
    for (const n of notes) console.log(`[claims]   note: ${n}`);
    return 0;
  }

  if (opts.write) {
    // LOCAL date, the same helper the check uses: the write path used `toISOString()` (UTC) while
    // the check compares against the local date, so on any machine east of UTC the freshly written
    // page could fail its own check late in the evening.
    const date = localDate(new Date());
    const rewritten = html
      .replace(CLAIM_ZH, `${total} 项 Rust 测试`)
      .replace(CLAIM_EN, `${total} Rust tests`)
      .replace(CLAIM_GATE, `${core} core + ${server} server`);
    const dated = rewritten
      .replace(DATE_ZH, (all, _old) => all.replace(/\d{4}-\d{2}-\d{2}/, date))
      .replace(DATE_EN, (all, _old) => all.replace(/\d{4}-\d{2}-\d{2}/, date))
      // The claim's own date, refreshed in the same pass: leaving it behind is what produced a page
      // that dated today's counts to yesterday (see CLAIM_GATE_DATE_* above).
      .replace(CLAIM_GATE_DATE_ZH, (all, _old) => all.replace(/\d{4}-\d{2}-\d{2}/, date))
      .replace(CLAIM_GATE_DATE_EN, (all, _old) => all.replace(/\d{4}-\d{2}-\d{2}/, date));
    fs.writeFileSync(opts.docs, dated);
    console.log(`[claims] rewrote ${path.relative(ROOT, opts.docs)}: ${total} Rust tests (${core} core + ${server} server), dated ${date}`);
    for (const p of problems) console.log(`[claims]   (was) ${p}`);
    for (const n of notes) console.error(`[claims]   note: ${n}`);
    if (unfixable.length > 0) {
      // The write succeeded but the page is still wrong in a way this command cannot repair: say so
      // and FAIL, because a refresh that reports success leaves the next gate run to find it.
      console.error(`[claims] FAIL: ${unfixable.length} problem(s) are NOT fixable by --write — the page must be edited (or this checker updated):`);
      for (const p of unfixable) console.error(`[claims]   - ${p}`);
      return 1;
    }
    return 0;
  }

  console.error(`[claims] FAIL: ${path.relative(ROOT, opts.docs)}`);
  for (const p of problems) console.error(`[claims]   - ${p}`);
  console.error('[claims] refresh with: node scripts/check_public_claims.cjs --measure --write');
  return 1;
}

// The CLI runs only when this file IS the program: the guard previously executed `main()` on require,
// so its own test suite could not import `localDate()` and instead re-implemented "today" — which is
// how the test came to use UTC while the guard writes the LOCAL date (round 165: the suite failed
// every evening east of UTC). One owner for "what is today" needs the module to be requirable.
if (require.main === module) {
  try {
    process.exit(main(process.argv.slice(2)));
  } catch (err) {
    if (err instanceof ClaimError) {
      console.error(`[claims] CANNOT VERIFY: ${err.message}`);
      process.exit(err.code);
    }
    console.error(`[claims] ERROR: ${err.message}`);
    process.exit(2);
  }
}

module.exports = { localDate, addDays, main };
