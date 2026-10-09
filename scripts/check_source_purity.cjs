#!/usr/bin/env node
'use strict';
/**
 * Production code must stay free of `unsafe`, `unwrap()` and panicking macros —
 * and the public page says so, so this asserts both directions.
 *
 * `docs/index.html` (GitHub Pages) advertises
 *
 *   <div class="lab" data-l="zh">生产代码无 unwrap / panic / unsafe</div>
 *   <div class="lab" data-l="en">unsafe / unwrap / panic in production code</div>
 *
 * Nothing enforced it. `dev-docs/HANDOFF.md` names the same rule, and
 * `dev-docs/design-tenant-api.md` records a *manual* grep gate ("`rg 'unwrap\(\)|
 * expect\(|panic!|unimplemented!|todo!'` must be empty") whose recipe cannot be
 * followed literally: it counts `#[cfg(test)]` code too (489 hits) and it asks
 * for something production does not satisfy (`expect`).
 *
 * What this checks:
 *   1. Zero `unsafe`, zero `.unwrap()`, zero `panic!/unreachable!/todo!/
 *      unimplemented!` in `crates/<crate>/src/**` OUTSIDE `#[cfg(test)]` items.
 *      Both `#[cfg(test)]` and `#[cfg(all(test, feature = "..."))]` are excluded
 *      — the earlier hand-grep only knew the first form and misread 20+ test
 *      lines as production (`cluster/forward.rs`'s `registry_tests`).
 *   2. Every crate ROOT carries `#![forbid(unsafe_code)]`: `src/lib.rs` AND every
 *      `[[bin]]` path. A `[[bin]]` is its own compilation unit, so `lib.rs`'s
 *      inner attribute does not cover it — that gap was real (verified: the same
 *      `unsafe` block compiles in `main.rs` once the attribute is removed).
 *   3. The public page still makes the purity claim (in both locales). If the
 *      wording changes, this fails and forces a human to re-verify rather than
 *      letting the guard quietly stop matching anything.
 *
 * `.expect(...)` is NOT counted as a violation: the public claim does not name
 * it, and all current production uses assert an unreachable invariant
 * (constant reqwest builds, poisoned-lock recovery, "leader mode only" pool).
 * The sites are printed every run so growth is visible; whether the project
 * should also forbid `expect` in production is an open question (plan D-13).
 *
 * Exit codes: 0 clean, 1 a violation, 2 the scan could not be performed.
 */

const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(__dirname, '..');
const FORBID = '#![forbid(unsafe_code)]';

const FORBIDDEN = [
  { name: 'unsafe', re: /\bunsafe\b/ },
  { name: 'unwrap()', re: /\.unwrap\(\)/ },
  { name: 'panicking macro', re: /\b(?:panic!|unreachable!|todo!|unimplemented!)/ },
];
const EXPECT = /\.expect\(/;

class ScanError extends Error {
  constructor(message) {
    super(message);
    this.code = 2;
  }
}

function parseArgs(argv) {
  const opts = {
    srcRoot: process.env.PURITY_SRC_ROOT || path.join(ROOT, 'crates'),
    docs: process.env.PURITY_DOCS || path.join(ROOT, 'docs', 'index.html'),
  };
  for (const arg of argv) {
    if (arg.startsWith('--src-root=')) opts.srcRoot = arg.slice('--src-root='.length);
    else if (arg.startsWith('--docs=')) opts.docs = arg.slice('--docs='.length);
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else throw new ScanError(`unknown argument: ${arg}`);
  }
  return opts;
}

/**
 * Blanks out every comment (line and block, nested blocks included) AND the
 * interior of every string literal, preserving newlines so line numbers stay
 * usable.
 *
 * Both halves matter: `"http://x"` must not start a comment, and a literal like
 * `"panic!()"` (an error message, say) must not be read as a panicking macro.
 * A real violation has to be code, so blanking string interiors cannot hide one.
 */
// The Rust text-blanking lexer (comments / strings / char literals / whole `#[cfg(test)]`
// items) lives in `rust_blank.cjs` — a single owner, shared by the guards `grep -l
// "require('./rust_blank.cjs')" scripts/check_*.cjs` lists. That command is given INSTEAD OF A NUMBER
// on purpose (round 208): this line said "three" until round 199 and "SEVEN" until round 208, and each
// count was defensible under a different pattern — mentions, `require` calls and test files give
// three different answers. Run it; do not write its output down. The point is that ONE lexer is right
// and every hand-rolled copy had the same class of bug (see that file's header).
const crypto = require('crypto');
const { stripComments, stripTestItems } = require('./rust_blank.cjs');
const { records, audit } = require('./recorded_exceptions.cjs');

/**
 * The `.expect(...)` sites production is ALLOWED to keep, one record each, keyed by
 * `<crate>/src/<path>@<sha1-8 of the line>` (decision **D-13**, adjudicated 2026-10-08).
 *
 * The policy the docs state was "no `unwrap`/`expect`/`panic` in production code", and the recipe given
 * with it ("`rg 'unwrap\(\)|expect\(' … must be empty") was never executable — measured then and now.
 * D-13 asked which side to move, and the answer taken is the narrow one: `unwrap`, panicking macros and
 * `unsafe` stay FORBIDDEN, and `expect` is allowed only for an invariant the code cannot violate, with
 * every remaining site REGISTERED here and its reason written down. An unregistered site is a finding;
 * a registration whose site disappeared is a stale claim. So the set cannot grow — or rot — in silence.
 */
const EXPECT_SITE_OK = records(process.env.PURITY_EXPECT_OK, [
  [
    'hydra-server/src/main.rs@d9758f50',
    'cert store at the TLS listener (main.rs:1163-1166): the store is built UNCONDITIONALLY whenever a ' +
      'TLS feature is compiled in (main.rs:510-517), and this branch is only reached with a TLS ' +
      'listener configured — so the Option is None only in a build that cannot reach this line. There ' +
      'is also no caller to return an error to: this is bootstrap, before any request exists.',
  ],
  [
    'hydra-server/src/proxy/provider_client.rs@8e38d65c',
    'the reqwest client (provider_client.rs:128-131): the builder uses CONSTANT settings and no ' +
      'redirect policy, and a failed build is retried once with an identical builder — a failure that ' +
      'survives that retry is a property of the constant set, not of request data, and it happens at ' +
      'client construction where there is no request to fail.',
  ],
]);

/** One registered `expect` site's identity: the file, plus a fingerprint of the line itself. */
function expectKey(relFile, text) {
  return `${relFile}@${crypto.createHash('sha1').update(text).digest('hex').slice(0, 8)}`;
}

/**
 * Is `file` a module whose DECLARATION carries `cfg(test)`? Then it is not production code, however
 * many `expect`s it contains — `usage/testing.rs` is the case that forced this (2026-10-08): its `mod`
 * line in `usage/mod.rs` is `#[cfg(test)]`, its only users are `#[cfg(test)]` blocks, and this guard
 * was counting its ten sites as "production", i.e. the number it printed was not the thing it claimed.
 *
 * The declaration is FOUND, not assumed: the attribute block directly above `mod <stem>;` must contain
 * `cfg(...test...)`, so a module that loses its gate stops being exempt on the next run.
 */
function testOnlyModule(cratesRoot, file) {
  const stem = path.basename(file, '.rs');
  const dir = path.dirname(file);
  const candidates = [
    path.join(path.dirname(dir), `${path.basename(dir)}.rs`),
    path.join(dir, 'mod.rs'),
    path.join(dir, 'lib.rs'),
    path.join(dir, 'main.rs'),
  ].filter((c) => fs.existsSync(c));
  const declRe = new RegExp(`^\\s*(?:pub(?:\\([^)]*\\))?\\s+)?mod\\s+${stem}\\s*;`);
  for (const cand of candidates) {
    const lines = fs.readFileSync(cand, 'utf8').split('\n');
    for (let i = 0; i < lines.length; i += 1) {
      if (!declRe.test(lines[i])) continue;
      for (let j = i - 1; j >= 0; j -= 1) {
        const t = lines[j].trim();
        if (t === '' || t.startsWith('//')) continue;
        if (!t.startsWith('#[')) break;
        if (/\bcfg\b/.test(t) && /\btest\b/.test(t)) return `${path.relative(cratesRoot, cand)}:${j + 1}`;
      }
    }
  }
  return null;
}

function walkRustFiles(dir, acc = []) {
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) walkRustFiles(full, acc);
    else if (entry.name.endsWith('.rs')) acc.push(full);
  }
  return acc;
}

/**
 * Every crate root that must carry the lint: `src/lib.rs`, each `[[bin]]` declared in `Cargo.toml`,
 * AND cargo's **auto-discovered** binaries (`src/bin/<name>.rs` and the
 * `src/bin/<name>/main.rs` form).
 *
 * The auto-discovery half is round 187's fix: cargo builds `src/bin/foo.rs` as a binary with no
 * `[[bin]]` section at all, so such a file was scanned for `unsafe`/`unwrap` (it lives under `src/`)
 * but was NOT required to carry `#![forbid(unsafe_code)]` — while the OK line claimed the lint was
 * "present on every crate root (`<list>`)". A new auto-discovered binary would have shipped without
 * the lint and nothing would have said so. Measured when this was added: no `src/bin/` exists in this
 * tree, i.e. the hole was latent — which is exactly why it is closed by construction rather than by a
 * list somebody has to remember to update.
 */
function crateRoots(cratesRoot) {
  if (!fs.existsSync(cratesRoot)) throw new ScanError(`crates root not found: ${cratesRoot}`);
  const roots = [];
  for (const entry of fs.readdirSync(cratesRoot, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const crateDir = path.join(cratesRoot, entry.name);
    const manifest = path.join(crateDir, 'Cargo.toml');
    if (!fs.existsSync(manifest)) continue;
    const lib = path.join(crateDir, 'src', 'lib.rs');
    if (fs.existsSync(lib)) roots.push(lib);
    let inBin = false;
    for (const raw of fs.readFileSync(manifest, 'utf8').split('\n')) {
      const line = raw.trim();
      if (line.startsWith('[')) inBin = line === '[[bin]]';
      else if (inBin && line.startsWith('path')) {
        const m = line.match(/path\s*=\s*"([^"]+)"/);
        if (m) roots.push(path.join(crateDir, m[1]));
      }
    }
    // Cargo's auto-discovery: `src/bin/<name>.rs` and `src/bin/<name>/main.rs` are binary targets
    // without any manifest entry. Sorted so the OK line's list is stable.
    const binDir = path.join(crateDir, 'src', 'bin');
    if (fs.existsSync(binDir)) {
      for (const ent of fs.readdirSync(binDir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
        if (ent.isFile() && ent.name.endsWith('.rs')) roots.push(path.join(binDir, ent.name));
        else if (ent.isDirectory()) {
          const nested = path.join(binDir, ent.name, 'main.rs');
          if (fs.existsSync(nested)) roots.push(nested);
        }
      }
    }
  }
  return roots;
}

function scanFile(file) {
  const code = stripTestItems(stripComments(fs.readFileSync(file, 'utf8')));
  const lines = code.split('\n');
  const violations = [];
  const expects = [];
  lines.forEach((line, idx) => {
    for (const rule of FORBIDDEN) {
      if (rule.re.test(line)) violations.push({ file, line: idx + 1, kind: rule.name, text: line.trim() });
    }
    if (EXPECT.test(line)) expects.push({ file, line: idx + 1, text: line.trim() });
  });
  return { violations, expects };
}

function main(argv) {
  const opts = parseArgs(argv);
  if (opts.help) {
    console.log('usage: node scripts/check_source_purity.cjs [--src-root=DIR] [--docs=FILE]');
    return 0;
  }

  const cratesRoot = opts.srcRoot;
  const roots = crateRoots(cratesRoot);
  if (roots.length === 0) throw new ScanError(`no crate roots found under ${cratesRoot}`);

  const files = [];
  for (const entry of fs.readdirSync(cratesRoot, { withFileTypes: true })) {
    if (!entry.isDirectory()) continue;
    const src = path.join(cratesRoot, entry.name, 'src');
    if (fs.existsSync(src)) files.push(...walkRustFiles(src));
  }
  if (files.length === 0) throw new ScanError(`no Rust sources under ${cratesRoot}/*/src`);

  const problems = [];
  const allExpects = [];
  const testOnlyExpects = [];
  let violations = 0;
  for (const file of files) {
    const res = scanFile(file);
    violations += res.violations.length;
    for (const v of res.violations) {
      problems.push(`${path.relative(cratesRoot, v.file)}:${v.line}: ${v.kind}: ${v.text.slice(0, 110)}`);
    }
    if (res.expects.length === 0) continue;
    const declaredAt = testOnlyModule(cratesRoot, file);
    if (declaredAt) testOnlyExpects.push(...res.expects.map((e) => ({ ...e, declaredAt })));
    else allExpects.push(...res.expects);
  }
  // D-13: every remaining production site must be REGISTERED (see EXPECT_SITE_OK).
  const expectSites = allExpects.map((e) => ({ ...e, key: expectKey(path.relative(cratesRoot, e.file), e.text) }));
  const expectAudit = audit({
    records: EXPECT_SITE_OK,
    needed: expectSites.map((e) => e.key),
    applies: (key) => expectSites.some((e) => e.key === key),
  });
  for (const key of expectAudit.unrecorded) {
    const e = expectSites.find((x) => x.key === key);
    problems.push(
      `${path.relative(cratesRoot, e.file)}:${e.line}: .expect(...) at an UNREGISTERED site — production ` +
        `keeps only invariants the code cannot violate, each REGISTERED in EXPECT_SITE_OK with its reason ` +
        `(decision D-13). Its key is "${key}"; register it there, or return a Result/Option instead`,
    );
  }
  for (const key of expectAudit.stale) {
    problems.push(
      `EXPECT_SITE_OK records ${key}, but no production site matches that line any more (it changed, moved ` +
        `or is gone) — a recorded decision that cannot expire is a stale claim`,
    );
  }

  const missingLint = [];
  for (const root of roots) {
    const text = fs.readFileSync(root, 'utf8');
    // The attribute must be an inner attribute of THIS compilation unit.
    if (!text.split('\n').some((l) => l.trim() === FORBID)) missingLint.push(path.relative(cratesRoot, root));
  }

  if (!fs.existsSync(opts.docs)) throw new ScanError(`docs file not found: ${opts.docs}`);
  const html = fs.readFileSync(opts.docs, 'utf8');
  const claimZh = /生产代码无 unwrap \/ panic \/ unsafe/.test(html);
  const claimEn = /unsafe \/ unwrap \/ panic in production code/.test(html);

  // P3-8 (2026-10-09): the README (zh + en) advertises the production `expect`
  // count — "**2** `expect()` invariant assertions". That number used to rot like
  // the test counts (it said 6 until 2026-10-09, and named sites that no longer
  // existed). The production count measured here (`allExpects.length`) is the
  // truth; an advertised number is a claim only as long as a guard checks it.
  const README = [
    path.join(ROOT, 'README.md'),
    path.join(ROOT, 'README.zh-CN.md'),
  ];
  // Allow the markdown bold we actually write (`**2** expect()`), and the zh
  // "处" quantifier; `\**` is "zero or more stars" (markdown **bold**).
  const README_EXPECT_CLAIM = /\**(\d+)\**\s*(?:处\s*)?[`]*expect\(\)[`]*/;
  for (const file of README) {
    if (!fs.existsSync(file)) {
      problems.push(`README missing at ${path.relative(ROOT, file)} — the expect-count claim must stay verifiable`);
      continue;
    }
    const text = fs.readFileSync(file, 'utf8');
    const claim = text.match(README_EXPECT_CLAIM);
    if (!claim) {
      problems.push(`${path.relative(ROOT, file)} carries no "N expect()" claim — P3-8: add the measured production count, or update this checker`);
      continue;
    }
    const advertised = Number(claim[1]);
    if (advertised !== allExpects.length) {
      problems.push(
        `${path.relative(ROOT, file)} advertises ${advertised} production expect() but the scan finds ${allExpects.length} — ` +
          `the README claim must equal what check_source_purity measures`,
      );
    }
  }

  const info = `[purity] scanned ${files.length} file(s) under ${path.relative(ROOT, cratesRoot)}/*/src (outside #[cfg(test)] items); ${roots.length} crate root(s)`;
  // `problems.length === 0` is load-bearing and was MISSING when the D-13 registration landed
  // (2026-10-08): the branch asked only about `violations`, so the new "unregistered .expect site"
  // and "stale registration" findings were pushed into `problems` and then ignored — the guard printed
  // OK and exited 0 while holding findings it had just produced. It is caught here rather than by luck:
  // the first run after wiring the registration reported `2 production sites, every one REGISTERED`
  // although neither real key was in the map yet.
  if (problems.length === 0 && violations === 0 && missingLint.length === 0 && claimZh && claimEn) {
    console.log(`${info}: clean`);
    console.log(`[purity] OK: 0 unsafe / 0 unwrap() / 0 panicking macros in production code`);
    console.log(`[purity] OK: ${FORBID} present on every crate root (${roots.map((r) => path.relative(cratesRoot, r)).join(', ')})`);
    console.log(`[purity] OK: ${path.relative(ROOT, opts.docs)} still claims "no unwrap / panic / unsafe" in production (zh + en)`);
    console.log(
      `[purity] info: ${allExpects.length} production .expect(...) site(s), every one REGISTERED in ` +
        `EXPECT_SITE_OK with its reason (decision D-13: unwrap/panic/unsafe stay forbidden; expect is ` +
        `allowed for invariants the code cannot violate, and an unregistered site is a finding):`,
    );
    for (const e of expectSites) console.log(`[purity]   ${e.key}  (${path.relative(cratesRoot, e.file)}:${e.line})`);
    if (testOnlyExpects.length > 0) {
      // Named, not merely subtracted: a reader must be able to see WHICH code was left out and why.
      const where = [...new Set(testOnlyExpects.map((e) => `${path.relative(cratesRoot, e.file)} (declared ${e.declaredAt})`))];
      console.log(
        `[purity] info: ${testOnlyExpects.length} further .expect(...) site(s) in TEST-ONLY modules, ` +
          `excluded as non-production: ${where.join(', ')}`,
      );
    }
    return 0;
  }

  console.error(`${info}: FAIL`);
  for (const p of problems) console.error(`[purity]   forbidden in production: ${p}`);
  for (const m of missingLint) console.error(`[purity]   missing ${FORBID} in crate root: ${m}`);
  if (!claimZh || !claimEn) {
    console.error(`[purity]   ${path.relative(ROOT, opts.docs)} no longer states the claim (zh: ${claimZh ? 'ok' : 'MISSING'}, en: ${claimEn ? 'ok' : 'MISSING'})`);
    console.error('[purity]   the guard is tied to that wording: re-verify the claim, then update this checker');
  }
  if (problems.length > 0) {
    console.error('[purity] production code must use match/? on a typed error, or a Result-returning API, instead');
  }
  return 1;
}

try {
  process.exit(main(process.argv.slice(2)));
} catch (err) {
  if (err instanceof ScanError) {
    console.error(`[purity] CANNOT SCAN: ${err.message}`);
    process.exit(err.code);
  }
  console.error(`[purity] ERROR: ${err.message}`);
  process.exit(2);
}
