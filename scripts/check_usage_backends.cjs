#!/usr/bin/env node
'use strict';
/**
 * Every usage backend must be declared the same way, and that declaration must
 * agree with the rest of the repository (ADR-0002).
 *
 * The registry (`crates/hydra-server/src/usage/mod.rs`) is what makes "which usage backends
 * exist" a fact with one owner. A table alone is not enough: the parts that make a backend
 * usable live in four other places, and each of them can silently fall out of step —
 *
 *   * the cargo `feature` that compiles it (`Cargo.toml`) — a typo means the backend "does not
 *     exist" in some builds and the error message sends the operator to a feature that is not
 *     there;
 *   * `requires`: the variables it cannot run without — if one is not in the operator runbook
 *     (`ops.md`), the refusal names a variable the runbook never mentioned;
 *   * `recognises`: the variables it reads with a default — documented, or recorded below with a
 *     reason. The first run of this guard found `HYDRA_CLICKHOUSE_IO_TIMEOUT_MS` documented
 *     nowhere but inside another row's prose, and the two ClickHouse timeout knobs now have real
 *     rows; the list is empty, and it may only grow WITH a reason.
 *   * `RETIRED_USAGE_SINKS`: values that must NOT be selectable again (ADR-0002: `sqlite` is
 *     retired by user ruling, and a retired value that quietly reappears in the registry would
 *     undo that without a single test failing).
 *
 * Exit codes: 0 every backend is consistent, 1 something disagrees, 2 CANNOT VERIFY (the
 * registry could not be read, or nothing was parsed — a guard that "passes" by finding nothing
 * is the failure mode this one exists to avoid).
 */

const fs = require('node:fs');
const path = require('node:path');

const ROOT = path.resolve(__dirname, '..');

/** A floor on parsed backends: below it the parse is broken, not the tree. */
const MIN_BACKENDS = Number(process.env.CUB_MIN_BACKENDS || 1);

/**
 * `recognises` names whose documentation is prose-only or absent, each with the reason. Printed on
 * every run so the list cannot grow quietly (same rule as `recorded_exceptions.cjs`).
 */
const UNDOCUMENTED_OK = new Map(
  Object.entries(JSON.parse(process.env.CUB_UNDOCUMENTED_OK || '{}')),
);

/**
 * Variables of a CANDIDATE backend: one that exists to demonstrate the insertion pattern and is not
 * part of what the repository delivers (`dev-docs/usage-backends.md` §5).
 *
 * The operator runbook documents the PRODUCT; a candidate's variables belong in the backend matrix,
 * which is where an implementer looks. Making the guard demand a runbook row would either force
 * non-delivered configuration into the operator's reference or force the candidate to lie about being
 * delivered — so the exception is recorded here, with the document that DOES carry it, and printed on
 * every run like every other record in this repository.
 */
const CANDIDATE_ENV_DOC = new Map(
  Object.entries(
    JSON.parse(
      process.env.CUB_CANDIDATE_ENV_DOC ||
        JSON.stringify({
          HYDRA_TDENGINE_URL: 'dev-docs/usage-backends.md §5.1 (candidate: measured there, not delivered)',
          HYDRA_TDENGINE_CONNECT_TIMEOUT_MS: 'dev-docs/usage-backends.md §5.1 (candidate)',
          HYDRA_TDENGINE_IO_TIMEOUT_MS: 'dev-docs/usage-backends.md §5.1 (candidate)',
        }),
    ),
  ),
);

function parseArgs(argv) {
  const opts = {
    registry: process.env.CUB_REGISTRY || path.join(ROOT, 'crates/hydra-server/src/usage/mod.rs'),
    backendsDir:
      process.env.CUB_BACKENDS_DIR || path.join(ROOT, 'crates/hydra-server/src/usage/backends'),
    cargoToml: process.env.CUB_CARGO || path.join(ROOT, 'crates/hydra-server/Cargo.toml'),
    docs: process.env.CUB_DOCS || path.join(ROOT, 'dev-docs/ops.md'),
    dump: false,
  };
  for (const arg of argv) {
    if (arg.startsWith('--registry=')) opts.registry = arg.slice('--registry='.length);
    else if (arg.startsWith('--backends-dir=')) opts.backendsDir = arg.slice('--backends-dir='.length);
    else if (arg.startsWith('--cargo=')) opts.cargoToml = arg.slice('--cargo='.length);
    else if (arg.startsWith('--docs=')) opts.docs = arg.slice('--docs='.length);
    else if (arg === '--dump') opts.dump = true;
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else throw new Error(`unknown argument: ${arg}`);
  }
  return opts;
}

function read(file, what) {
  try {
    return fs.readFileSync(file, 'utf8');
  } catch (e) {
    const err = new Error(`cannot read ${what} (${path.relative(ROOT, file)}): ${e.message}`);
    err.code = 2;
    throw err;
  }
}

/** The modules listed in `REGISTRY`, in order. */
function registryModules(registrySource) {
  const m = registrySource.match(/pub static REGISTRY[^=]*=\s*&\[([\s\S]*?)\];/);
  if (!m) {
    const err = new Error('REGISTRY not found in the usage module — the guard cannot judge what it cannot read');
    err.code = 2;
    throw err;
  }
  return [...m[1].matchAll(/backends::([a-z0-9_]+)::DESCRIPTOR/g)].map((x) => x[1]);
}

/** Values that must never be selectable again. */
function retiredKinds(registrySource) {
  const m = registrySource.match(/RETIRED_USAGE_SINKS[^=]*=\s*&\[([\s\S]*?)\];/);
  if (!m) return [];
  return [...m[1].matchAll(/"([a-z0-9_]+)"/g)].map((x) => x[1]);
}

/** The `[features]` names of the crate. */
function cargoFeatures(cargo) {
  const i = cargo.indexOf('[features]');
  if (i < 0) {
    const err = new Error('no [features] section in the crate Cargo.toml');
    err.code = 2;
    throw err;
  }
  const section = cargo.slice(i + '[features]'.length).split(/^\[/m)[0];
  const names = new Set();
  for (const line of section.split('\n')) {
    const m = line.match(/^\s*([A-Za-z0-9_-]+)\s*=/);
    if (m) names.add(m[1]);
  }
  return names;
}

/** `const NAME: &str = "VALUE";` — how a backend carries an env name without repeating it. */
function stringConstants(source) {
  const out = new Map();
  for (const m of source.matchAll(/const\s+([A-Z0-9_]+)\s*:\s*&str\s*=\s*"([^"]+)"/g)) {
    out.set(m[1], m[2]);
  }
  return out;
}

/**
 * One backend's declaration, read out of its own module.
 *
 * The layout is part of the pattern (`dev-docs/usage-backends.md` §3 step 2: one module per
 * backend, `<kind>/mod.rs` plus an optional `transport.rs`), so a flat `<kind>.rs` is reported as a
 * structural failure rather than quietly accepted — two layouts is how "one module per backend"
 * stops being true.
 */
function readBackend(dir, module) {
  const file = path.join(dir, module, 'mod.rs');
  if (!fs.existsSync(file)) {
    return {
      module,
      file,
      notFound: true,
    };
  }
  const src = read(file, `the ${module} backend`);
  const consts = stringConstants(src);
  const kind = src.match(/kind:\s*"([a-z0-9_]+)"/);
  const feature = src.match(/feature:\s*"([A-Za-z0-9_-]+)"/);
  if (!kind || !feature) {
    return { module, file, missing: true };
  }
  const slice = (name, endRe) => {
    const m = src.match(new RegExp(`${name}:\\s*&\\[([\\s\\S]*?)${endRe}`, 'm'));
    return m ? m[1] : '';
  };
  const requiresBlock = slice('requires', '\\],');
  const requires = [...requiresBlock.matchAll(/name:\s*([A-Z0-9_]+|"[^"]+")/g)].map((m) =>
    m[1].startsWith('"') ? m[1].slice(1, -1) : consts.get(m[1]) || m[1],
  );
  const recognisesBlock = slice('recognises', '\\],');
  const recognises = [...recognisesBlock.matchAll(/"([A-Z][A-Z0-9_]+)"/g)].map((m) => m[1]);
  return {
    module,
    file,
    kind: kind[1],
    feature: feature[1],
    requires,
    recognises,
    hasNote: /notes:\s*"/.test(src) || /notes:\s*"[^"]*"[^"]*"/.test(src),
    source: src,
  };
}

/** Backticked names in the FIRST column of any table row — the operator-runbook shape. */
function documentedNames(markdown) {
  const names = new Set();
  for (const line of markdown.split('\n')) {
    if (!line.trimStart().startsWith('|')) continue;
    const cells = line.split('|').slice(1, -1);
    if (cells.length < 2) continue;
    for (const m of cells[0].matchAll(/`([A-Z][A-Z0-9_]{2,})`/g)) names.add(m[1]);
  }
  return names;
}

function check(opts) {
  const problems = [];
  const notes = [];

  const registrySource = read(opts.registry, 'the usage registry');
  const modules = registryModules(registrySource);
  if (modules.length < MIN_BACKENDS) {
    const err = new Error(
      `only ${modules.length} backend(s) parsed from REGISTRY (< floor ${MIN_BACKENDS}); refusing to pass by finding nothing`,
    );
    err.code = 2;
    throw err;
  }
  if (new Set(modules).size !== modules.length) problems.push(`REGISTRY lists a module twice: ${modules.join(', ')}`);

  const features = cargoFeatures(read(opts.cargoToml, 'the crate manifest'));
  const docs = documentedNames(read(opts.docs, 'the operator runbook'));
  const retired = retiredKinds(registrySource);

  const backends = modules.map((m) => readBackend(opts.backendsDir, m));
  for (const b of backends) {
    if (b.notFound) {
      problems.push(
        `${b.module}: no ${path.relative(ROOT, b.file)} — a backend is one module, \`<kind>/mod.rs\` ` +
          '(dev-docs/usage-backends.md §3 step 2); a flat `<kind>.rs` is a second layout',
      );
      continue;
    }
    if (b.missing) {
      problems.push(`${b.module}: no \`kind\`/\`feature\` in ${path.relative(ROOT, b.file)} — a descriptor the guard cannot read is a backend nobody can audit`);
      continue;
    }
    if (!features.has(b.feature)) {
      problems.push(`${b.module}: declares feature \`${b.feature}\`, which is not a feature of crates/hydra-server/Cargo.toml`);
    }
    for (const name of b.requires) {
      if (docs.has(name)) continue;
      if (CANDIDATE_ENV_DOC.has(name)) {
        notes.push(`${b.module}: \`${name}\` is documented for a CANDIDATE backend — ${CANDIDATE_ENV_DOC.get(name)}`);
        continue;
      }
      problems.push(`${b.module}: requires \`${name}\`, which the operator runbook (dev-docs/ops.md) does not document — the refusal would name a variable nobody has heard of`);
    }
    for (const name of b.recognises) {
      if (docs.has(name)) continue;
      if (CANDIDATE_ENV_DOC.has(name)) {
        notes.push(`${b.module}: \`${name}\` is documented for a CANDIDATE backend — ${CANDIDATE_ENV_DOC.get(name)}`);
        continue;
      }
      if (UNDOCUMENTED_OK.has(name)) {
        notes.push(`${b.module}: \`${name}\` is undocumented on purpose — ${UNDOCUMENTED_OK.get(name)}`);
        continue;
      }
      problems.push(`${b.module}: recognises \`${name}\`, which is documented nowhere. Either add a runbook row or record why it needs none (UNDOCUMENTED_OK in this guard)`);
    }
  }

  const kinds = backends.filter((b) => !b.missing).map((b) => b.kind);
  if (new Set(kinds).size !== kinds.length) problems.push(`two backends share a kind: ${kinds.join(', ')}`);
  for (const kind of retired) {
    if (kinds.includes(kind)) {
      problems.push(`\`${kind}\` is in RETIRED_USAGE_SINKS and in REGISTRY — a retired value that is selectable again`);
    }
  }

  return { problems, notes, backends, retired };
}

function main(argv) {
  let opts;
  try {
    opts = parseArgs(argv);
  } catch (e) {
    console.error(`usage-backends: ${e.message}`);
    return 2;
  }
  if (opts.help) {
    console.log('usage: node scripts/check_usage_backends.cjs [--registry=… --backends-dir=… --cargo=… --docs=… --dump]');
    return 0;
  }
  let result;
  try {
    result = check(opts);
  } catch (e) {
    console.error(`usage-backends: CANNOT VERIFY: ${e.message}`);
    return e.code === 2 ? 2 : 1;
  }
  if (opts.dump) {
    console.log(JSON.stringify({ backends: result.backends.map((b) => ({ module: b.module, kind: b.kind, feature: b.feature, requires: b.requires, recognises: b.recognises })), retired: result.retired }, null, 2));
  }
  for (const n of result.notes) console.log(`usage-backends: NOTE ${n}`);
  if (result.problems.length) {
    console.error(`usage-backends: FAIL`);
    for (const p of result.problems) console.error(`  - ${p}`);
    return 1;
  }
  const kinds = result.backends.map((b) => b.kind).join(', ');
  console.log(
    `OK: ${result.backends.length} usage backend(s) declared consistently (${kinds}); ` +
      `${result.retired.length} retired value(s) stay unselectable; ` +
      `${result.notes.length} recorded exception(s)`,
  );
  return 0;
}

if (require.main === module) process.exit(main(process.argv.slice(2)));

module.exports = { main, check, registryModules, retiredKinds, documentedNames, readBackend };
