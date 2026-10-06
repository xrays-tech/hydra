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
// items) lives in `rust_blank.cjs` — a single owner, shared by SEVEN guards (re-measure with
// `grep -rln "rust_blank.cjs" scripts/*.cjs`; this said "three" until round 199, i.e. exactly the
// drifting count the surrounding comments warn about)
// being right, and each hand-rolled copy had the same class of bug (see that file's header).
const { stripComments, stripTestItems } = require('./rust_blank.cjs');

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
  let violations = 0;
  for (const file of files) {
    const res = scanFile(file);
    violations += res.violations.length;
    allExpects.push(...res.expects);
    for (const v of res.violations) {
      problems.push(`${path.relative(cratesRoot, v.file)}:${v.line}: ${v.kind}: ${v.text.slice(0, 110)}`);
    }
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

  const info = `[purity] scanned ${files.length} file(s) under ${path.relative(ROOT, cratesRoot)}/*/src (outside #[cfg(test)] items); ${roots.length} crate root(s)`;
  if (violations === 0 && missingLint.length === 0 && claimZh && claimEn) {
    console.log(`${info}: clean`);
    console.log(`[purity] OK: 0 unsafe / 0 unwrap() / 0 panicking macros in production code`);
    console.log(`[purity] OK: ${FORBID} present on every crate root (${roots.map((r) => path.relative(cratesRoot, r)).join(', ')})`);
    console.log(`[purity] OK: ${path.relative(ROOT, opts.docs)} still claims "no unwrap / panic / unsafe" in production (zh + en)`);
    console.log(`[purity] info: ${allExpects.length} production .expect(...) site(s) — allowed by the claim (it names unwrap/panic/unsafe only); policy question tracked as plan D-13:`);
    for (const e of allExpects) console.log(`[purity]   ${path.relative(cratesRoot, e.file)}:${e.line}: ${e.text.slice(0, 100)}`);
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
