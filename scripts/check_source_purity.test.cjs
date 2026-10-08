#!/usr/bin/env node
'use strict';
/**
 * Tests for scripts/check_source_purity.cjs.
 *
 * The checker backs a PUBLIC claim (`docs/index.html`: "no unwrap / panic /
 * unsafe in production code"), so the tests pin both directions and, crucially,
 * the boundary: code inside `#[cfg(test)]` — in either gating form — is test
 * code and must NOT be reported, while a violation in a string or a comment must
 * NOT be either.
 */

const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { spawnSync } = require('node:child_process');

const CHECKER = path.join(__dirname, 'check_source_purity.cjs');
const REPO = path.resolve(__dirname, '..');

const CLAIM_ZH = '<div class="lab" data-l="zh">生产代码无 unwrap / panic / unsafe</div>';
const CLAIM_EN = '<div class="lab" data-l="en">unsafe / unwrap / panic in production code</div>';

function fixture({ lib = '#![forbid(unsafe_code)]\n\npub fn f() -> u8 { 1 }\n', main = '#![forbid(unsafe_code)]\n\nfn main() {}\n', docs = `${CLAIM_ZH}\n${CLAIM_EN}\n`, manifest = '[[bin]]\nname = "demo"\npath = "src/main.rs"\n', extra = {} } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'purity-'));
  const crate = path.join(dir, 'crates', 'demo');
  fs.mkdirSync(path.join(crate, 'src'), { recursive: true });
  fs.writeFileSync(path.join(crate, 'Cargo.toml'), `[package]\nname = "demo"\n\n${manifest}`);
  fs.writeFileSync(path.join(crate, 'src', 'lib.rs'), lib);
  fs.writeFileSync(path.join(crate, 'src', 'main.rs'), main);
  for (const [name, body] of Object.entries(extra)) fs.writeFileSync(path.join(crate, 'src', name), body);
  const docsPath = path.join(dir, 'index.html');
  fs.writeFileSync(docsPath, docs);
  return { dir, crate, docs: docsPath };
}

function run(fx, extraArgs = [], env = {}) {
  const args = [`--src-root=${path.join(fx.dir, 'crates')}`, `--docs=${fx.docs}`, ...extraArgs];
  const res = spawnSync(process.execPath, [CHECKER, ...args], {
    encoding: 'utf8',
    // `PURITY_EXPECT_OK: '{}'` REPLACES the repository's registrations: a fixture tree does not contain
    // `main.rs:1166` or `provider_client.rs:131`, so without this every case would report the two
    // repository records as stale — the exact failure mode `recorded_exceptions.cjs` documents
    // ("getting this wrong cost every guard a round of 'nine existing assertions turned red at once'").
    // Cases that exercise the registration pass their own value through `env`.
    env: { ...process.env, PURITY_EXPECT_OK: '{}', ...env },
  });
  return { status: res.status, stdout: res.stdout || '', stderr: res.stderr || '' };
}

test('a clean tree passes', () => {
  const r = run(fixture());
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /OK: 0 unsafe \/ 0 unwrap\(\) \/ 0 panicking macros/);
  assert.match(r.stdout, /3 crate root|2 crate root/);
});

test('unwrap() in production code fails and names the file:line', () => {
  const r = run(fixture({ lib: '#![forbid(unsafe_code)]\n\npub fn f() -> u8 { let o: Option<u8> = None; o.unwrap() }\n' }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /demo\/src\/lib\.rs:3: unwrap\(\)/);
});

test('an unsafe block in production code fails', () => {
  const r = run(fixture({ lib: '#![forbid(unsafe_code)]\n\npub fn f() { let p: *const u8 = std::ptr::null(); unsafe { let _ = *p; } }\n' }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /demo\/src\/lib\.rs:3: unsafe/);
});

test('a panicking macro in production code fails', () => {
  const r = run(fixture({ main: '#![forbid(unsafe_code)]\n\nfn main() { panic!("boom") }\n' }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /demo\/src\/main\.rs:3: panicking macro/);
});

test('all four panicking macros are covered', () => {
  for (const macro of ['panic!', 'unreachable!', 'todo!', 'unimplemented!']) {
    const r = run(fixture({ lib: `#![forbid(unsafe_code)]\n\npub fn f() -> u8 { ${macro}("x") }\n` }));
    assert.equal(r.status, 1, `${macro} was not caught`);
    assert.match(r.stderr, /panicking macro/);
  }
});

test('#[cfg(test)] items are not production code', () => {
  const lib = `#![forbid(unsafe_code)]

pub fn f() -> u8 { 1 }

#[cfg(test)]
mod tests {
    #[test]
    fn t() {
        let o: Option<u8> = None;
        o.unwrap();
        panic!("test");
    }
}
`;
  const r = run(fixture({ lib }));
  assert.equal(r.status, 0, r.stderr);
});

test('#[cfg(all(test, feature = "..."))] items are not production code either', () => {
  // This is the form the hand-written grep recipe in dev-docs missed, which is
  // how 20+ test lines were once read as "production" hits.
  const lib = `#![forbid(unsafe_code)]

pub fn f() -> u8 { 1 }

#[cfg(all(test, feature = "cluster-redis"))]
mod registry_tests {
    async fn helper() -> u8 { Some(1u8).unwrap() }

    #[test]
    fn t() { panic!("nope") }
}
`;
  const r = run(fixture({ lib }));
  assert.equal(r.status, 0, r.stderr);
});

test('comments naming unwrap/panic/unsafe are not violations', () => {
  const lib = `#![forbid(unsafe_code)]

// unwrap() is banned here; panic!("x") too. unsafe is not needed.
/// Doc comment: we avoid .unwrap() and unreachable!() here.
/* block: .unwrap(), unsafe, todo!() */
pub fn f() -> u8 { 1 }
`;
  const r = run(fixture({ lib }));
  assert.equal(r.status, 0, r.stderr);
});

test('string literals are not violations, and a // inside a string does not hide code', () => {
  const fine = run(fixture({ lib: '#![forbid(unsafe_code)]\n\npub fn m() -> &\'static str { "panic!() .unwrap() unsafe" }\n' }));
  assert.equal(fine.status, 0, fine.stderr);

  const raw = run(fixture({ lib: '#![forbid(unsafe_code)]\n\npub fn m() -> &\'static str { r#"todo!()"# }\n' }));
  assert.equal(raw.status, 0, raw.stderr);

  // One line, a URL literal first: the // in "http://" must not swallow the rest.
  const hidden = run(fixture({ lib: '#![forbid(unsafe_code)]\n\npub fn f() { let _u = "http://a"; let o: Option<u8> = None; o.unwrap(); }\n' }));
  assert.equal(hidden.status, 1);
  assert.match(hidden.stderr, /unwrap\(\)/);
});

test('a crate root without #![forbid(unsafe_code)] fails, lib and bin alike', () => {
  const noLib = run(fixture({ lib: 'pub fn f() -> u8 { 1 }\n' }));
  assert.equal(noLib.status, 1);
  assert.match(noLib.stderr, /missing #!\[forbid\(unsafe_code\)\] in crate root: demo\/src\/lib\.rs/);

  // A [[bin]] is its own compilation unit — this is the gap that was real.
  const noBin = run(fixture({ main: 'fn main() {}\n' }));
  assert.equal(noBin.status, 1);
  assert.match(noBin.stderr, /missing #!\[forbid\(unsafe_code\)\] in crate root: demo\/src\/main\.rs/);
});

test('the lint must be inside the file, not merely mentioned', () => {
  const r = run(fixture({ lib: '#![forbid(unsafe_code)]\n\n// see #![forbid(unsafe_code)] above\npub fn f() -> u8 { 1 }\n', main: 'fn main() { // #![forbid(unsafe_code)]\n}\n' }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /crate root: demo\/src\/main\.rs/);
});

test('reworded public claim fails so the guard cannot silently stop matching', () => {
  const r = run(fixture({ docs: '<div>production code is safe</div>\n' }));
  assert.equal(r.status, 1);
  assert.match(r.stderr, /no longer states the claim \(zh: MISSING, en: MISSING\)/);
  assert.match(r.stderr, /re-verify the claim/);

  const half = run(fixture({ docs: `${CLAIM_ZH}\n` }));
  assert.equal(half.status, 1);
  assert.match(half.stderr, /zh: ok, en: MISSING/);
});

/* Decision D-13 (2026-10-08): `expect` is NOT banned outright — the docs' "must be empty" recipe was
 * never executable — but every remaining production site must be REGISTERED with a reason, so the set
 * cannot grow in silence. This replaced a test that asserted `expect` is "reported as information, never
 * as a violation", i.e. the behaviour that made the old number unactionable. */
test('expect() must be REGISTERED: unregistered is a finding, registered is reported', () => {
  const fx = fixture({ lib: '#![forbid(unsafe_code)]\n\npub fn f() -> u8 { let o: Option<u8> = None; o.expect("infallible") }\n' });

  const unregistered = run(fx);
  assert.equal(unregistered.status, 1, unregistered.stdout);
  assert.match(unregistered.stderr, /UNREGISTERED site/);
  const key = (unregistered.stdout + unregistered.stderr).match(/Its key is "([^"]+)"/)[1];
  assert.match(key, /^demo\/src\/lib\.rs@[0-9a-f]{8}$/, key);

  const registered = run(fx, [], { PURITY_EXPECT_OK: JSON.stringify({ [key]: 'the Option cannot be None here' }) });
  assert.equal(registered.status, 0, registered.stderr);
  assert.match(registered.stdout, /1 production \.expect\(\.\.\.\) site\(s\), every one REGISTERED/);
  assert.ok(registered.stdout.includes(key), registered.stdout);

  // A registration whose site is gone is a stale claim, not a licence to keep the line around.
  const stale = run(fx, [], { PURITY_EXPECT_OK: JSON.stringify({ 'demo/src/lib.rs@deadbeef': 'long gone' }) });
  assert.equal(stale.status, 1, stale.stdout);
  assert.match(stale.stderr, /is a stale claim/);
});

/* The number this guard printed was not the thing it claimed (2026-10-08): a module declared
 * `#[cfg(test)]` in its PARENT (`usage/testing.rs`, declared in `usage/mod.rs:332`) is not production
 * code, yet its ten `.expect(...)` sites were counted as production. The exemption is verified, not
 * assumed: the attribute block above the `mod <stem>;` line must actually contain `cfg(...test...)`. */
test('a module declared #[cfg(test)] is excluded, and the exclusion is verified', () => {
  const fx = fixture({
    lib: '#![forbid(unsafe_code)]\n\n#[cfg(test)]\npub mod helper;\n\npub fn f() -> u8 { 1 }\n',
    extra: { 'helper.rs': 'pub fn g() -> u8 { let o: Option<u8> = None; o.expect("infallible") }\n' },
  });
  const r = run(fx);
  assert.equal(r.status, 0, r.stderr);
  assert.match(r.stdout, /0 production \.expect\(\.\.\.\) site\(s\)|every one REGISTERED/);
  assert.match(r.stdout, /1 further \.expect\(\.\.\.\) site\(s\) in TEST-ONLY modules, excluded as non-production/);
  assert.match(r.stdout, /demo\/src\/helper\.rs \(declared demo\/src\/lib\.rs:3\)/);

  // Lose the gate and the SAME file becomes production code again, so its site must be registered.
  const ungated = run(fixture({
    lib: '#![forbid(unsafe_code)]\n\npub mod helper;\n\npub fn f() -> u8 { 1 }\n',
    extra: { 'helper.rs': 'pub fn g() -> u8 { let o: Option<u8> = None; o.expect("infallible") }\n' },
  }));
  assert.equal(ungated.status, 1, ungated.stdout);
  assert.match(ungated.stderr, /UNREGISTERED site/);
});

test('a missing crates root is exit 2 (cannot scan), never a silent pass', () => {
  const fx = fixture();
  const res = spawnSync(process.execPath, [CHECKER, `--src-root=${path.join(fx.dir, 'nope')}`, `--docs=${fx.docs}`], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  assert.match(res.stderr, /CANNOT SCAN: crates root not found/);
});

test('a crate with no detectable root (no lib.rs, no [[bin]]) is exit 2', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'purity-empty-'));
  const crate = path.join(dir, 'crates', 'demo');
  fs.mkdirSync(crate, { recursive: true });
  fs.writeFileSync(path.join(crate, 'Cargo.toml'), '[package]\nname = "demo"\n');
  const docs = path.join(dir, 'index.html');
  fs.writeFileSync(docs, `${CLAIM_ZH}\n${CLAIM_EN}\n`);
  const res = spawnSync(process.execPath, [CHECKER, `--src-root=${path.join(dir, 'crates')}`, `--docs=${docs}`], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  // It refuses rather than reporting "0 violations" for a tree it could not read.
  assert.match(res.stderr, /CANNOT SCAN: no crate roots found/);
});

test('a [[bin]] path that does not exist is exit 2, not a phantom clean scan', () => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'purity-nosrc-'));
  const crate = path.join(dir, 'crates', 'demo');
  fs.mkdirSync(crate, { recursive: true });
  fs.writeFileSync(path.join(crate, 'Cargo.toml'), '[package]\nname = "demo"\n\n[[bin]]\nname = "demo"\npath = "src/main.rs"\n');
  const docs = path.join(dir, 'index.html');
  fs.writeFileSync(docs, `${CLAIM_ZH}\n${CLAIM_EN}\n`);
  const res = spawnSync(process.execPath, [CHECKER, `--src-root=${path.join(dir, 'crates')}`, `--docs=${docs}`], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  assert.match(res.stderr, /no Rust sources under/);
});

test('a missing docs file is exit 2', () => {
  const fx = fixture();
  const res = spawnSync(process.execPath, [CHECKER, `--src-root=${path.join(fx.dir, 'crates')}`, `--docs=${path.join(fx.dir, 'nope.html')}`], { encoding: 'utf8' });
  assert.equal(res.status, 2);
  assert.match(res.stderr, /docs file not found/);
});

test('unknown arguments are rejected and --help works', () => {
  const bad = spawnSync(process.execPath, [CHECKER, '--nope'], { encoding: 'utf8' });
  assert.equal(bad.status, 2);
  assert.match(bad.stderr, /unknown argument: --nope/);
  const help = spawnSync(process.execPath, [CHECKER, '--help'], { encoding: 'utf8' });
  assert.equal(help.status, 0);
  assert.match(help.stdout, /check_source_purity/);
});

/**
 * `#[cfg(not(test))]` is PRODUCTION code, and the blanket `\btest\b` test treated it as a test
 * item, blanking the whole module. Measured 2026-09-30: the same `.unwrap()` was reported in a
 * plain function (exit 1) and silently accepted inside `not(test)` (exit 0) — the guard accepted
 * "this gate says NOT test" as proof that it WAS test code. A feature whose name starts with
 * `test-` was swallowed the same way (the word appears inside the string).
 */
test('a `#[cfg(not(test))]` module is scanned (it is production-only code)', () => {
  const lib = '#![forbid(unsafe_code)]\n\npub fn f() -> u8 { 1 }\n\n#[cfg(not(test))]\nmod boot {\n    pub fn s() -> u8 { let o: Option<u8> = None; o.unwrap() }\n}\n';
  const res = run(fixture({ lib }));
  assert.equal(res.status, 1, `a violation inside not(test) was accepted:\n${res.stdout}`);
  assert.match(res.stderr, /unwrap\(\)/);
});

// This case locks in a property that today holds for TWO reasons: `stripComments` blanks the
// string contents before the gate predicate runs (so the word `test` is gone), and the fallback
// predicate is an exact `test` match. It is here so a future reordering cannot reintroduce the
// bug — note that it does NOT go red under the OLD substring predicate (verified), which is
// exactly why F1's real evidence is the `not(test)` case below/above it.
test('a feature named `test-…` does not make its code a test item', () => {
  const lib = '#![forbid(unsafe_code)]\n\n#[cfg(feature = "test-helpers")]\nmod h {\n    pub fn s() -> u8 { let o: Option<u8> = None; o.unwrap() }\n}\n';
  const res = run(fixture({ lib }));
  assert.equal(res.status, 1, `a violation under a test-named FEATURE was accepted:\n${res.stdout}`);
});

test('CONTROL: `#[cfg(all(test, feature = "x"))]` IS a test item (the four sites in this tree)', () => {
  const lib = '#![forbid(unsafe_code)]\n\n#[cfg(all(test, feature = "cluster-redis"))]\nmod t {\n    #[test]\n    fn s() { let o: Option<u8> = None; o.unwrap(); }\n}\n';
  const res = run(fixture({ lib }));
  assert.equal(res.status, 0, `a feature-gated TEST module was reported:\n${res.stderr}`);
});

test('the shipped repository passes the purity check', () => {
  const res = spawnSync(process.execPath, [CHECKER], { encoding: 'utf8' });
  assert.equal(res.status, 0, `the shipped tree violates its own public claim:\n${res.stderr}`);
  assert.match(res.stdout, /hydra-server\/src\/main\.rs/);
});

test('the shipped page still carries both locale claims', () => {
  const html = fs.readFileSync(path.join(REPO, 'docs', 'index.html'), 'utf8');
  assert.ok(html.includes(CLAIM_ZH), 'zh purity claim missing from docs/index.html');
  assert.ok(html.includes(CLAIM_EN), 'en purity claim missing from docs/index.html');
});

/**
 * A `'{'` in a `#[cfg(test)]` item must not hide the production code that FOLLOWS it.
 *
 * `stripTestItems` finds the end of a gated item by counting braces; before char literals
 * were blanked, `const L: char = '{';` in a test module (there are 15 such modules with code
 * after them, e.g. `redis/mod.rs`, `main.rs`, `sink.rs`) added one to the depth, the closing
 * `}` of the module only brought it back to 1, and EVERY later line was blanked — so a real
 * `.unwrap()` in the same file was never scanned. Measured 2026-09-30: the checker printed
 * `OK: 0 unsafe / 0 unwrap()` (exit 0) with that violation in the tree. The control case
 * (same shape without the char literal) proves the fixture itself is sound.
 */
test("a '{' in a test module does not hide a later production violation", () => {
  const violation = 'pub fn g() -> u8 { let o: Option<u8> = None; o.unwrap() }\n';
  const plain = '\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() { assert!(true); }\n}\n';
  const braced = "\n#[cfg(test)]\nmod tests {\n    const L: char = '{';\n    #[test]\n    fn t() { assert!(true); }\n}\n";
  const control = run(fixture({ lib: `#![forbid(unsafe_code)]\n\n${plain}${violation}` }));
  assert.equal(control.status, 1, 'control: the violation must be caught when no char literal is present');
  const res = run(fixture({ lib: `#![forbid(unsafe_code)]\n\n${braced}${violation}` }));
  assert.equal(res.status, 1, `one char literal disarmed the scan; stderr:\n${res.stderr}`);
  assert.match(res.stderr, /unwrap\(\)/);
});

/**
 * The opposite direction: a `'}'` in a test module must not END the item early and turn later
 * TEST code into reported "production" violations.
 */
test("a '}' in a test module does not report the test code after it", () => {
  const lib = "#![forbid(unsafe_code)]\n\npub fn f() -> u8 { 1 }\n\n#[cfg(test)]\nmod tests {\n    const R: char = '}';\n    #[test]\n    fn t() { let o: Option<u8> = None; o.unwrap(); }\n}\n";
  const res = run(fixture({ lib }));
  assert.equal(res.status, 0, `test code was reported as production code:\n${res.stderr}`);
});

/** Lifetime syntax must not be mistaken for a char literal by the new scanner. */
test("lifetimes are not char literals (the scanner must not blank `&'a str`)", () => {
  const lib = "#![forbid(unsafe_code)]\n\npub fn f<'a>(s: &'a str) -> &'a str { s }\n\n#[cfg(test)]\nmod tests {\n    const E: char = '\\'';\n    #[test]\n    fn t() { assert!(true); }\n}\n";
  const res = run(fixture({ lib }));
  assert.equal(res.status, 0, `lifetimes/escaped quotes broke the scan:\n${res.stderr}`);
});

/* Round 187: cargo AUTO-DISCOVERS `src/bin/<name>.rs` (and the `src/bin/<name>/main.rs` form) as
 * binary targets with no `[[bin]]` section. Such a file was scanned for violations (it lives under
 * `src/`) but was NOT required to carry `#![forbid(unsafe_code)]`, while the OK line claimed the lint
 * was "present on every crate root". Measured: no `src/bin/` exists in this tree today, so the hole was
 * latent — closed by construction so a new binary cannot slip through. */
test('an AUTO-DISCOVERED binary must carry the lint too', () => {
  const fx = fixture();
  const bin = path.join(fx.crate, 'src', 'bin');
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(path.join(bin, 'extra.rs'), 'fn main() {}\n');
  const r = run(fx);
  assert.equal(r.status, 1, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stdout + r.stderr, /src\/bin\/extra\.rs/);
  assert.match(r.stdout + r.stderr, /forbid\(unsafe_code\)/);
});

test('CONTROL: the same auto-discovered binary WITH the lint passes', () => {
  const fx = fixture();
  const bin = path.join(fx.crate, 'src', 'bin');
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(path.join(bin, 'extra.rs'), '#![forbid(unsafe_code)]\n\nfn main() {}\n');
  const r = run(fx);
  assert.equal(r.status, 0, `${r.status} ${r.stdout}${r.stderr}`);
});

test('...and the nested `src/bin/<name>/main.rs` form is covered as well', () => {
  const fx = fixture();
  const nested = path.join(fx.crate, 'src', 'bin', 'tool');
  fs.mkdirSync(nested, { recursive: true });
  fs.writeFileSync(path.join(nested, 'main.rs'), 'fn main() {}\n');
  const r = run(fx);
  assert.equal(r.status, 1, `${r.status} ${r.stdout}${r.stderr}`);
  assert.match(r.stdout + r.stderr, /src\/bin\/tool\/main\.rs/);
});

test('CONTROL: a NON-Rust file in src/bin is not treated as a crate root', () => {
  const fx = fixture();
  const bin = path.join(fx.crate, 'src', 'bin');
  fs.mkdirSync(bin, { recursive: true });
  fs.writeFileSync(path.join(bin, 'README.md'), '# not a binary\n');
  const r = run(fx);
  assert.equal(r.status, 0, `${r.status} ${r.stdout}${r.stderr}`);
});
