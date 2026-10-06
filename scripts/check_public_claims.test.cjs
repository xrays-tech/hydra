#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/check_public_claims.cjs.
 *
 * The checker exists because the public Rust test count in docs/index.html is
 * hand-edited and had already gone stale twice. These tests pin both directions:
 * a truthful claim passes, and every kind of dishonest or unusable input fails
 * with the right exit code (1 = stale claim, 2 = no usable measurement).
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CHECKER = path.join(__dirname, 'check_public_claims.cjs');
// ONE owner for "what is today": the guard's own `localDate` (it is requirable since round 165).
const { localDate } = require(CHECKER);
const REPO = path.resolve(__dirname, '..');

const CORE_OK = `running 253 tests
test result: ok. 253 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.20s
`;
const SERVER_OK = `running 522 tests
test result: ok. 522 passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 2.00s

running 22 tests
test result: ok. 22 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.30s
`;
// 253 + 522 + 22 = 797

function scratch() {
  return fs.mkdtempSync(path.join(os.tmpdir(), 'claims-'));
}

function docsWith(zh, en, zhDate = '2026-09-29', enDate = '2026-09-29', gateCore = 253, gateServer = 544, gateCoreEn = null, gateServerEn = null) {
  const coreEn = gateCoreEn === null ? gateCore : gateCoreEn;
  const serverEn = gateServerEn === null ? gateServer : gateServerEn;
  return `<!doctype html>
<html><body>
      <div class="note" data-l="zh">${zh} 项 Rust 测试 · 浏览器 / SDK / JS 套件 · clippy -D warnings 硬门禁（${zhDate}）</div>
      <div class="note" data-l="en">${en} Rust tests · browser / SDK / JS suites · clippy -D warnings gate (${enDate})</div>
      <div class="note" data-l="zh">正确性门禁：<code>${gateCore} core + ${gateServer} server</code> 测试（${zhDate} 计数）、<code>clippy -D warnings</code>——CI 硬门槛。</div>
      <div class="note" data-l="en">Correctness gates: <code>${coreEn} core + ${serverEn} server</code> tests (counted ${enDate}), <code>clippy -D warnings</code> — hard CI gate.</div>
</body></html>
`;
}

/** Writes a fixture docs+logs triple and runs the checker over it. */
function run({ zh = 797, en = 797, zhDate = '2026-09-29', enDate = '2026-09-29', gateCore = 253, gateServer = 544, gateCoreEn = null, gateServerEn = null, core = CORE_OK, server = SERVER_OK, mutate = (d) => d, extraArgs = [], env = {} } = {}) {
  const dir = scratch();
  const docs = path.join(dir, 'index.html');
  fs.writeFileSync(docs, mutate(docsWith(zh, en, zhDate, enDate, gateCore, gateServer, gateCoreEn, gateServerEn)));
  const coreLog = path.join(dir, 'core.log');
  const serverLog = path.join(dir, 'server.log');
  if (core !== null) fs.writeFileSync(coreLog, core);
  if (server !== null) fs.writeFileSync(serverLog, server);
  const args = [`--docs=${docs}`, `--core-log=${coreLog}`, `--server-log=${serverLog}`, ...extraArgs];
  const res = spawnSync(process.execPath, [CHECKER, ...args], { encoding: 'utf8', env: { ...process.env, ...env } });
  return { status: res.status, stdout: res.stdout || '', stderr: res.stderr || '', docs, dir, coreLog, serverLog };
}

test('a truthful claim passes and reports the arithmetic', () => {
  const r = run();
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /OK: advertised 797 Rust tests == measured 797 \(hydra-core 253 \+ hydra-server 544\)/);
});

/* Round 126: the measurement date must be TRUE, not merely consistent across locales. Both
 * strings live in the same file, so zh-vs-en agreement said nothing about reality: a page could
 * advertise a future date, or a measurement older than the transcripts it is checked against. */
test('a FUTURE measurement date is reported', () => {
  const future = (() => {
    const d = new Date();
    d.setDate(d.getDate() + 30);
    const p = (n) => String(n).padStart(2, '0');
    return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  })();
  const r = run({ zhDate: future, enDate: future });
  assert.equal(r.status, 1, r.stdout);
  assert.match(r.stderr, /is in the future/);
});

test('a date OLDER than the transcripts is a NOTE, not a failure', () => {
  // The first version of this check made an old date a hard failure, which was WRONG: CI
  // regenerates the transcripts every run, so a month with no test-count change would turn a
  // correct page red and the only "fix" would be a commit bumping a date (measured 2026-09-30 with
  // a page whose counts and gate claims were exactly right: exit 1). Being old makes a date worth
  // refreshing, not false — so it is printed and the exit stays 0.
  const r = run({ zhDate: '2026-08-01', enDate: '2026-08-01' });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /note: the page is dated 2026-08-01/);
});

test('CONTROL: an old date with a WRONG count still fails (the note does not mask the real check)', () => {
  const r = run({ zh: 700, en: 700, zhDate: '2026-08-01', enDate: '2026-08-01' });
  assert.equal(r.status, 1, r.stdout);
  assert.match(r.stderr, /advertised 700 but the suites report 797/);
});

test('CONTROL: today’s date passes both date checks', () => {
  const today = localDate(new Date());
  const r = run({ zhDate: today, enDate: today });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /dated /);
});

test('`--write` refuses to trust foreign transcripts (the self-satisfying shape)', () => {
  // Measured before this: `--write` together with `--core-log/--server-log` rewrote the page to
  // whatever those files said and exited 0 — a check that satisfies itself. The refresh path is
  // `--measure --write`; a checker run passes logs and no `--write`.
  // The page advertises 700 while the transcripts say 797 — exactly the case `--write` used to
  // "fix" by rewriting the number.
  const r = run({ zh: 700, en: 700, gateCore: 700, gateServer: 700, extraArgs: ['--write'] });
  assert.equal(r.status, 2, `${r.stdout}${r.stderr}`);
  assert.match(r.stderr, /self-satisfying/);
  // ...and it must not have touched the page.
  const after = fs.readFileSync(r.docs, 'utf8');
  assert.match(after, /700 项 Rust 测试/, 'the page was rewritten anyway');
});

test('the shipped docs/index.html stays parseable and self-consistent', () => {
  // This deliberately does NOT pin a specific count: the numbers change every
  // time a test is added (they did, in round 66: 797 -> 798), and the truth of
  // the count is checked in CI against freshly measured transcripts. What must
  // never break is that the shipped page still carries claims the checker can
  // READ — so a malformed/edit-broken page turns this red.
  const html = fs.readFileSync(path.join(REPO, 'docs', 'index.html'), 'utf8');
  const total = Number(/(\d+)\s*项\s*Rust\s*测试/.exec(html)?.[1]);
  const totalEn = Number(/(\d+)\s+Rust\s+tests/i.exec(html)?.[1]);
  const split = /(\d+)\s*core\s*\+\s*(\d+)\s*server/i.exec(html);
  assert.ok(Number.isInteger(total) && total > 100, 'the zh total claim is missing/unreadable');
  assert.equal(total, totalEn, 'the two locales must advertise the same total');
  assert.ok(split, 'the per-suite correctness-gate claim is missing');
  assert.equal(Number(split[1]) + Number(split[2]), total, 'the split must add up to the advertised total');

  // Feed the checker transcripts that MATCH the page: it must accept them.
  const dir = scratch();
  const core = `running ${split[1]} tests\ntest result: ok. ${split[1]} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n`;
  const server = `running ${split[2]} tests\ntest result: ok. ${split[2]} passed; 0 failed; 1 ignored; 0 measured; 0 filtered out; finished in 1.00s\n`;
  fs.writeFileSync(path.join(dir, 'core.log'), core);
  fs.writeFileSync(path.join(dir, 'server.log'), server);
  const res = spawnSync(process.execPath, [CHECKER, `--core-log=${path.join(dir, 'core.log')}`, `--server-log=${path.join(dir, 'server.log')}`], { encoding: 'utf8' });
  assert.equal(res.status, 0, `the shipped page no longer matches its own numbers:\n${res.stderr}`);
  assert.match(res.stdout, /correctness-gate claim matches per-suite counts/);
});

test('a stale claim fails with exit 1 and names both numbers', () => {
  const r = run({ zh: 796, en: 796 });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /advertised 796 but the suites report 797/);
  assert.match(r.stderr, /--measure --write/);
});

test('locales disagreeing on the count fails', () => {
  const r = run({ zh: 797, en: 796 });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /locales disagree: zh advertises 797, en advertises 796/);
});

test('a missing locale claim fails rather than silently checking only one', () => {
  const r = run({ mutate: (d) => d.replace(/<div class="note" data-l="zh">.*?<\/div>\n/, '') });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no advertised Rust test count found/);
  assert.match(r.stderr, /zh \(N 项 Rust 测试\)/);
});

test('a missing measurement date fails', () => {
  const r = run({ mutate: (d) => d.replace(/（2026-09-29）/, '').replace(/\(2026-09-29\)/, '') });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /missing measurement date/);
});

test('locales disagreeing on the date fails', () => {
  const r = run({ zhDate: '2026-09-29', enDate: '2026-08-01' });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /locales disagree on the measurement date/);
});

test('a missing log is exit 2 (cannot verify), never a silent pass', () => {
  const r = run({ server: null });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /CANNOT VERIFY: server-log log not found/);
});

test('a truncated log with no test result line is exit 2', () => {
  const r = run({ core: 'Compiling hydra-core v0.1.0\nFinished `test` profile\n' });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /no 'test result:' line/);
});

test('a failing suite is exit 2, not a phantom stale claim', () => {
  const r = run({ server: 'running 3 tests\ntest result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s\n' });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /did not pass \(1 failed\)/);
});

test('counts from several test binaries in one log are summed', () => {
  const r = run({ server: 'running 544 tests\ntest result: ok. 544 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n' });
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /hydra-core 253 \+ hydra-server 544/);
});

test('a suspiciously small measurement is refused by the floor', () => {
  const r = run({ core: 'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n', server: 'running 1 tests\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n' });
  assert.equal(r.status, 2);
  assert.match(r.stderr, /measured only 3 tests \(< floor 100\)/);
});

test('the floor is overridable so the check itself can be tested at fixture sizes', () => {
  const r = run({
    zh: 3,
    en: 3,
    gateCore: 2,
    gateServer: 1,
    core: 'running 2 tests\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n',
    server: 'running 1 tests\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n',
    env: { PUBLIC_CLAIMS_MIN_TESTS: '1' },
  });
  assert.equal(r.status, 0, r.stderr);
});

test('the per-suite correctness gate claim is verified, not just the sum', () => {
  const ok = run();
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /correctness-gate claim matches per-suite counts \(2 occurrence\(s\): 253 core \+ 544 server\)/);

  // The real rot: the page advertised the gate as 114 core + 173 server while the
  // suites had grown to 253 + 544 (the sum happened to differ too, but a page
  // could easily keep a correct total and a wrong split).
  const stale = run({ gateCore: 114, gateServer: 173 });
  assert.equal(stale.status, 1);
  assert.match(stale.stderr, /correctness-gate claim #1 says 114 core \+ 173 server but the suites report 253 core \+ 544 server/);
  assert.match(stale.stderr, /#2 says 114 core \+ 173 server/);
});

test('a correct TOTAL with a wrong per-suite split is still caught', () => {
  // 200 + 597 = 797: the stats-band total matches, the split does not.
  const r = run({ gateCore: 200, gateServer: 597 });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /correctness-gate claim #1 says 200 core \+ 597 server/);
  assert.doesNotMatch(r.stderr, /advertised 797 but the suites report/);
});

/* Round 192. The refresh path rewrote the per-suite counts and the headline date, and left the
 * CLAIM's own date behind — so on 2026-10-01 `--measure --write` produced a page saying
 * "258 core + 555 server 测试（2026-09-30 计数）": a measurement date on which those numbers did not
 * exist. Nothing caught it (the counts matched), which is why the rule below exists and why all
 * three legs are asserted: the stale date must FAIL, a page with no claim date must FAIL loudly
 * rather than retire the rule, and `--write` must move that date together with the counts. */
test('the correctness-gate claim date must be the advertised measurement date', () => {
  const ok = run();
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /correctness-gate claim matches per-suite counts .* and carries the same measurement date/);

  // Exactly the shipped drift: counts right, claim dated one day earlier.
  const stale = run({ zhDate: '2026-10-01', enDate: '2026-10-01', mutate: (d) => d.replace(/（2026-10-01 计数）/, '（2026-09-30 计数）').replace(/\(counted 2026-10-01\)/, '(counted 2026-09-30)') });
  assert.equal(stale.status, 1, stale.stdout);
  assert.match(stale.stderr, /correctness-gate claim \(zh\) is dated 2026-09-30 while the page advertises 2026-10-01/);
  assert.match(stale.stderr, /correctness-gate claim \(en\) is dated 2026-09-30/);

  // A reworded page must not silently retire the rule.
  const undated = run({ mutate: (d) => d.replace(/（2026-09-29 计数）/, '').replace(/ \(counted 2026-09-29\)/, '') });
  assert.equal(undated.status, 1, undated.stdout);
  assert.match(undated.stderr, /correctness-gate claim carries no measurement date \(zh\)/);
  assert.match(undated.stderr, /carries no measurement date \(en\)/);
});

test('--write refreshes the correctness-gate claim date too (the 2026-10-01 drift)', () => {
  const p = (n) => String(n).padStart(2, '0');
  const d = new Date();
  const today = `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())}`;
  const r = run({
    zhDate: today,
    enDate: today,
    extraArgs: ['--write'],
    env: { PUBLIC_CLAIMS_ALLOW_LOG_WRITE: '1' },
    mutate: (html) => html.replace(/（\d{4}-\d{2}-\d{2} 计数）/, '（2020-01-01 计数）').replace(/\(counted \d{4}-\d{2}-\d{2}\)/, '(counted 2020-01-01)'),
  });
  assert.equal(r.status, 0, r.stderr);
  const written = fs.readFileSync(r.docs, 'utf8');
  assert.match(written, new RegExp(`（${today} 计数）`), 'zh claim date must move with the counts');
  assert.match(written, new RegExp(`\\(counted ${today}\\)`), 'en claim date must move with the counts');
  // ...and the refreshed page must pass its own check (the write is not a self-satisfying one).
  const recheck = spawnSync(process.execPath, [CHECKER, `--docs=${r.docs}`, `--core-log=${r.coreLog}`, `--server-log=${r.serverLog}`], { encoding: 'utf8' });
  assert.equal(recheck.status, 0, recheck.stderr);
});

test('the two locales must agree on the per-suite split too', () => {
  const r = run({ gateCoreEn: 253, gateServerEn: 500 });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /correctness-gate claim #2 says 253 core \+ 500 server/);
});

test('a removed correctness-gate claim fails rather than checking nothing', () => {
  const r = run({ mutate: (d) => d.replace(/正确性门禁[^\n]*\n/g, '').replace(/Correctness gates[^\n]*\n/g, '') });
  assert.equal(r.status, 1);
  assert.match(r.stderr, /correctness-gate claim \("<N> core \+ <M> server tests"\) is gone from the page/);
});

test('--write refreshes the count and the date in both locales', () => {
  // `PUBLIC_CLAIMS_ALLOW_LOG_WRITE=1` is required to rewrite a COUNT from transcripts (round 134);
  // it is set here on purpose so the rewrite MECHANICS stay covered, while the default shape — a
  // command that could make the check pass — is refused (see the case above).
  const r = run({
    zh: 700, en: 700, gateCore: 700, gateServer: 700, zhDate: '2026-01-01', enDate: '2026-01-01',
    extraArgs: ['--write'], env: { PUBLIC_CLAIMS_ALLOW_LOG_WRITE: '1' },
  });
  assert.equal(r.status, 0, r.stderr);
  const out = fs.readFileSync(r.docs, 'utf8');
  // The guard's OWN `localDate` (round 126 unified write and check to the local calendar; round 165
  // made the module requirable so this test cannot drift from it). Until round 165 the test used
  // `toISOString()`, i.e. UTC, and failed every evening east of UTC (measured: local 2026-10-01 /
  // UTC 2026-09-30 ⇒ the page got today's local date while the expectation demanded yesterday's).
  const today = localDate(new Date());
  assert.match(out, /797 项 Rust 测试/);
  assert.match(out, /797 Rust tests/);
  assert.match(out, new RegExp(`（${today}）`));
  assert.match(out, new RegExp(`\\(${today}\\)`));
  assert.doesNotMatch(out, /700/);
  assert.match(out, /253 core \+ 544 server/); // the stale 700+700 split was rewritten to the measured one
  // ...and the rewritten file then satisfies the checker without --write.
  const again = spawnSync(process.execPath, [CHECKER, `--docs=${r.docs}`, `--core-log=${r.coreLog}`, `--server-log=${r.serverLog}`], { encoding: 'utf8' });
  assert.equal(again.status, 0, again.stderr);
});

test('without a measurement the checker refuses instead of assuming success', () => {
  const dir = scratch();
  const docs = path.join(dir, 'index.html');
  fs.writeFileSync(docs, docsWith(797, 797));
  const res = spawnSync(process.execPath, [CHECKER, `--docs=${docs}`], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  assert.match(res.stderr, /no measurement: pass --core-log\/--server-log \(or --measure\)/);
});

test('an unknown argument is rejected', () => {
  const res = spawnSync(process.execPath, [CHECKER, '--wat'], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  assert.match(res.stderr, /unknown argument: --wat/);
});

test('--help documents the refresh path', () => {
  const res = spawnSync(process.execPath, [CHECKER, '--help'], { encoding: 'utf8' });
  assert.equal(res.status, 0);
  assert.match(res.stdout, /--measure runs cargo itself/);
});

/* Round 194, adversarial review: `--write` printed the problems it could NOT repair with a `(was)`
 * prefix and then returned 0 — "reports success while the page is still wrong". Measured then: a page
 * with no correctness-gate line at all got `rewrote …` and exit 0, and the very next check of the
 * file it had just written said FAIL. Now the fixable half is still refreshed and the command fails. */
test('--write FAILS when a problem is not fixable by the rewrite', () => {
  const strip = (html) => html
    .replace(/\s*<div class="note" data-l="zh">正确性门禁[\s\S]*?<\/div>\n/, '\n')
    .replace(/\s*<div class="note" data-l="en">Correctness gates[\s\S]*?<\/div>\n/, '\n');
  const r = run({ extraArgs: ['--write'], env: { PUBLIC_CLAIMS_ALLOW_LOG_WRITE: '1' }, mutate: strip });
  assert.equal(r.status, 1, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stdout, /rewrote /, 'the fixable half must still be refreshed');
  assert.match(r.stderr, /1 problem\(s\) are NOT fixable by --write/);
  assert.match(r.stderr, /the correctness-gate claim .* is gone from the page/);

  // CONTROL: the same page WITHOUT --write is unchanged in kind (exit 1, no rewrite).
  const checkOnly = run({ mutate: strip });
  assert.equal(checkOnly.status, 1, `${checkOnly.status} ${checkOnly.stdout}`);
  assert.doesNotMatch(checkOnly.stdout, /rewrote /);
});

test('the real repository docs file is readable and contains both locale claims', () => {
  const html = fs.readFileSync(path.join(REPO, 'docs', 'index.html'), 'utf8');
  assert.match(html, /(\d+) 项 Rust 测试/);
  assert.match(html, /(\d+) Rust tests/);
});
