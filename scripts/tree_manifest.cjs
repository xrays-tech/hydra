#!/usr/bin/env node
'use strict';
/**
 * A manifest of the SOURCE TREE, so "this verdict covers the revision it claims to" is CHECKABLE.
 *
 * Why (round 179): the round-178 gate reported GREEN while two of the entries it judged had run BEFORE
 * that round's edits to the very files they check. The verdict was real — but it was not a verdict
 * about the final revision, and only a human re-reading the transcript could tell (the round had to be
 * re-run by hand for exactly this reason).
 *
 * So a run records the tree at its START and verifies it at its END:
 *   * any change under a CODE path  ⇒ exit 1, naming the file(s): the earlier entries judged a
 *     revision that no longer exists;
 *   * a DOCS-only change            ⇒ exit 0 with a printed NOTE: no code moved, but the
 *     doc-dependent entries (`findings-disposition`, `public claims`) may not match the final text;
 *   * added / removed files count as changes too, classified by their path.
 *
 * Deliberately NOT a general-purpose snapshot tool: the covered roots and the exclusions are the
 * repository's, and generated/scratch areas (`target`, `.acceptance`, `node_modules`, `dist`, caches)
 * are left out so a normal run is stable.
 *
 * Usage: node scripts/tree_manifest.cjs --write <file>
 *        node scripts/tree_manifest.cjs --check <file>
 * Exit: 0 clean (or docs-only, with a note) · 1 a code change · 2 cannot verify.
 */
const crypto = require('crypto');
const fs = require('fs');
const path = require('path');

const ROOT = path.resolve(process.env.TREE_MANIFEST_ROOT || path.join(__dirname, '..'));
// Covered roots: everything a guard or a drill reads, plus the deployment artefacts and the docs.
const COVERED = ['scripts', 'integration', 'crates', 'tools', 'tests', 'environment', '.github', 'docs', 'dev-docs'];
const ROOT_FILES = ['Cargo.toml', 'Cargo.lock', 'playwright.config.cjs'];
// Generated / scratch areas. `.acceptance` holds the gate's own log and every drill's scratch dir, so
// including it would make every run look unstable.
const SKIP_DIRS = new Set([
  'target', 'node_modules', 'dist', 'dist-test', '.acceptance', '.git', '.cargo-cache', 'pw-browsers',
  '__pycache__', '.pytest_cache', '.mypy_cache', '.venv', 'venv',
]);
// Where a change invalidates a verdict (code) versus only where it deserves a note (docs).
const DOC_ROOTS = ['docs/', 'dev-docs/'];

function isDoc(rel) {
  return DOC_ROOTS.some((p) => rel.startsWith(p));
}

function walk(dir, out) {
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch {
    return;
  }
  for (const e of entries) {
    if (SKIP_DIRS.has(e.name)) continue;
    if (e.name.endsWith('.log')) continue;
    const full = path.join(dir, e.name);
    if (e.isDirectory()) walk(full, out);
    else if (e.isFile()) out.push(full);
  }
}

function snapshot() {
  const files = [];
  for (const rel of COVERED) {
    const dir = path.join(ROOT, rel);
    if (fs.existsSync(dir)) walk(dir, files);
  }
  for (const rel of ROOT_FILES) {
    const full = path.join(ROOT, rel);
    if (fs.existsSync(full)) files.push(full);
  }
  const lines = files
    .map((full) => {
      const rel = path.relative(ROOT, full);
      const hash = crypto.createHash('sha256').update(fs.readFileSync(full)).digest('hex');
      return `${hash}  ${rel}`;
    })
    .sort();
  return lines;
}

function readManifest(file) {
  const map = new Map();
  for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
    if (!line.trim()) continue;
    const m = /^([0-9a-f]{64}) {2}(.+)$/.exec(line);
    if (!m) return null; // malformed: never guess
    map.set(m[2], m[1]);
  }
  return map;
}

function main(argv) {
  const [mode, file] = argv;
  if ((mode !== '--write' && mode !== '--check') || !file) {
    console.error('usage: node scripts/tree_manifest.cjs --write|--check <file>');
    return 2;
  }
  const now = snapshot();
  if (mode === '--write') {
    fs.mkdirSync(path.dirname(path.resolve(file)), { recursive: true });
    fs.writeFileSync(file, now.join('\n') + '\n');
    console.log(`[tree-manifest] recorded ${now.length} file(s) in ${file}`);
    return 0;
  }
  let before;
  try {
    before = readManifest(file);
  } catch (e) {
    console.error(`[tree-manifest] CANNOT VERIFY: cannot read ${file} (${e.message}) — the run did not record a manifest, so its verdict cannot be tied to a revision`);
    return 2;
  }
  if (before === null) {
    console.error(`[tree-manifest] CANNOT VERIFY: ${file} is malformed`);
    return 2;
  }
  const after = new Map(now.map((l) => [l.slice(66), l.slice(0, 64)]));
  const changed = [];
  const added = [];
  const removed = [];
  for (const [rel, hash] of after) {
    if (!before.has(rel)) added.push(rel);
    else if (before.get(rel) !== hash) changed.push(rel);
  }
  for (const rel of before.keys()) if (!after.has(rel)) removed.push(rel);
  const touched = [...changed.map((r) => `modified ${r}`), ...added.map((r) => `added ${r}`), ...removed.map((r) => `removed ${r}`)];
  const code = touched.filter((t) => !isDoc(t.split(' ').slice(1).join(' ')));
  const docs = touched.filter((t) => isDoc(t.split(' ').slice(1).join(' ')));
  if (docs.length > 0) {
    console.log(
      `[tree-manifest] NOTE: ${docs.length} doc file(s) changed during the run (${docs.slice(0, 5).join('; ')}` +
        `${docs.length > 5 ? ', …' : ''}) — no code moved, but the doc-dependent entries may not match the final text`,
    );
  }
  if (code.length === 0) {
    console.log(`[tree-manifest] OK  (the source tree is unchanged: ${after.size} file(s), docs-only changes: ${docs.length})`);
    return 0;
  }
  for (const t of code) console.error(`[tree-manifest] CHANGED ${t}`);
  console.error(
    `[tree-manifest] ${code.length} code file(s) changed DURING the run: the entries that ran before the ` +
      `change judged a revision that no longer exists, so this run's verdict does not cover the tree as it is now`,
  );
  return 1;
}

if (require.main === module) process.exit(main(process.argv.slice(2)));

module.exports = { snapshot, readManifest, isDoc };
