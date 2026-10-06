#!/usr/bin/env node
'use strict';
/**
 * The shipped compose manifests must agree with the binary that runs them.
 *
 * Two rules, each one written because the tree got it wrong and NOTHING failed:
 *
 * 1. NO MANIFEST MAY SET A RETIRED VARIABLE. `cluster/mod.rs`'s `RETIRED_CLUSTER_ENV` exists so a
 *    deployment that still carries a retired knob is TOLD it does nothing. Measured 2026-10-05:
 *    `docker-compose.local.yml` still set `HYDRA_LEADER_LEASE_MS` (on two nodes) and
 *    `HYDRA_CONTROL_POLL_MS` (on all three) — so the shipped local stack booted with three ERROR
 *    lines saying those settings are ignored, and the file told the reader the opposite. The
 *    table is read from the SOURCE, never duplicated here: a name added to `RETIRED_CLUSTER_ENV`
 *    becomes a rule for this guard in the same commit, with no second list to keep in sync.
 *
 * 2. THE FEATURE SET THAT BUILDS THE BINARY MUST COVER WHAT THE MANIFEST ASKS THE BINARY TO DO.
 *    `HYDRA_CLUSTER_PEERS` set on a binary built without the `arachne` feature is a REFUSAL TO
 *    BOOT (`main.rs`: "this build has no control plane"), and `HYDRA_REDIS_URL` /
 *    `HYDRA_REDIS_MODE` on a binary without `cluster-redis` are SILENTLY IGNORED
 *    (`redis_backend` is `None` under `#[cfg(not(feature = "cluster-redis"))]`), so every node
 *    would keep its own rate limits and its own auth L2 while the manifest claims a shared
 *    backbone. Measured 2026-10-05: `environment/build.sh` — the recipe that produces the image
 *    EVERY manifest in this directory runs — listed three features and not `arachne`, while CI
 *    stayed green because CI's own builds pass `arachne` in a separate step. The requirement is
 *    DERIVED from the manifests below, so a manifest that starts using raft drags the recipe with
 *    it.
 *
 * The manifests are read with `compose_static.cjs` — the text reader the other compose guards fall
 * back to when `docker compose config` cannot render a file. In CI that is ALWAYS the case for the
 * local stack (its `env_file` is gitignored), which is exactly where the retired variables were.
 *
 * KNOWN RESIDUAL: `env_file:` pulls in variables this reader can only see when the file exists.
 * The names are read when it does, and a NOTE is printed when it does not — never silence.
 *
 * Exit 0 = both rules hold · 1 = a rule is broken · 2 = CANNOT VERIFY (refused to judge).
 * COMPOSE_ENV_ROOT overrides the repository root (self-test fixtures).
 *
 * Usage: node scripts/check_compose_env.cjs
 */
const fs = require('fs');
const path = require('path');
const {
  hydraServices,
  collectAnchors,
  mergeRefs,
  subBlock,
  unjudgeableServices,
} = require('./compose_static.cjs');

const DEFAULT_REPO = path.resolve(__dirname, '..');
/** The root to read, overridable so the self-test can run against a fixture tree. */
const REPO = process.env.COMPOSE_ENV_ROOT ? path.resolve(process.env.COMPOSE_ENV_ROOT) : DEFAULT_REPO;
/** The image NAME the other compose guards also treat as "this is a Hydra node". */
const IMAGE_RE = /(^|\/)hydra(-local)?:/;

/** The manifests that ship, read as text (never rendered — see the header). */
const MANIFESTS = [
  'environment/docker-compose.yml',
  'environment/docker-compose.cluster.yml',
  'environment/docker-compose.local.yml',
];

/**
 * The recipes that produce a binary a manifest runs, in the order the tree documents them:
 * `build.sh` builds the Docker image (the manifests' `image:`), `release.sh` stages the same
 * binary to `environment/bin/` for the same Dockerfile.
 */
const RECIPES = ['environment/build.sh', 'environment/release.sh'];

/** The `[[bin]] hydra` target is gated on this feature; a recipe without it produces no binary. */
const BIN_FEATURE = 'server';

/**
 * `NAME: value` per line, from `subBlock(…, 'environment')` and from any anchor it merges.
 *
 * Names come out of the text; the VALUES come from `compose_static.envValue`, so a service whose
 * environment is one `<<: *node-env` merge is read exactly like the service that defines the
 * anchor (the shipped cluster file is written that way, and reading only the literal keys of a
 * merging service would make `hydra-b`/`hydra-c` look like standalone nodes).
 */
function envNames(block, anchors) {
  const names = new Set();
  const env = subBlock(block, 'environment');
  for (const line of env) {
    const m = /^\s+([A-Za-z_][A-Za-z0-9_]*):/.exec(line);
    if (m) names.add(m[1]);
  }
  for (const ref of mergeRefs(env)) {
    const anchor = anchors.get(ref.name);
    if (!anchor) continue;
    for (const line of anchor.lines) {
      const m = /^\s+([A-Za-z_][A-Za-z0-9_]*):/.exec(line);
      if (m) names.add(m[1]);
    }
  }
  return names;
}

/** The `env_file:` paths this service declares, as written (relative to the compose file). */
function envFileRefs(block) {
  const out = [];
  const line = subBlock(block, 'env_file').concat(
    (function own() {
      // `env_file: [a, b]` is a scalar on the service's OWN level, so it is not in the sub-block.
      const own = block.split('\n').find((l) => /^\s+env_file:/.test(l));
      return own ? [own] : [];
    })(),
  );
  for (const l of line) {
    const m = /^\s+env_file:\s*(.*)$/.exec(l) ?? /^\s+-\s*(.*)$/.exec(l);
    if (!m) continue;
    let value = m[1].replace(/#.*$/, '').trim();
    if (value.startsWith('[')) value = value.replace(/^\[|\]$/g, '');
    for (const part of value.split(',')) {
      const p = part.trim().replace(/^["']|["']$/g, '');
      if (p) out.push(p);
    }
  }
  return out;
}

/** `NAME=value` names from a `env_file` (compose's dotenv format), or null when it does not exist. */
function envFileNames(file) {
  if (!fs.existsSync(file)) return null;
  const out = new Set();
  for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
    const m = /^\s*(?:export\s+)?([A-Za-z_][A-Za-z0-9_]*)\s*=/.exec(line);
    if (m) out.add(m[1]);
  }
  return out;
}

/** A source-declared name table, e.g. `const CLUSTER_ONLY_ENV: [&str; 7] = [ … ];`. */
function readTable(src, name) {
  const re = new RegExp(`const\\s+${name}\\s*:\\s*\\[&str;\\s*(\\d+)\\]\\s*=\\s*\\[([\\s\\S]*?)\\];`);
  const m = re.exec(src);
  if (!m) return null;
  const names = [...m[2].matchAll(/"([A-Z0-9_]+)"/g)].map((x) => x[1]);
  if (names.length !== Number(m[1])) return null;
  return { names, set: new Set(names) };
}

/**
 * What one manifest asks of the binary, as `{ hydraServices, names, features, notes }`.
 *
 * Throws a `ScanError`-shaped Error (see below) when the text cannot be judged at all.
 */
function readManifest(label, file, clusterOnly, retired) {
  const text = fs.readFileSync(file, 'utf8');
  const opaque = unjudgeableServices(text);
  if (opaque.length > 0) {
    throw new Error(
      `cannot judge ${label}: ` +
        opaque.map((o) => `${o.name} ${o.reason}`).join('; ') +
        ' — render the file (`docker compose config`) instead of relying on the text path',
    );
  }
  const services = hydraServices(text, IMAGE_RE);
  if (services.length === 0) {
    throw new Error(
      `found no hydra-image service in ${label}; refusing to pass by finding nothing ` +
        `(expected an image matching ${IMAGE_RE})`,
    );
  }
  const anchors = collectAnchors(text);
  const names = new Set();
  const notes = [];
  for (const { name, block } of services) {
    for (const n of envNames(block, anchors)) names.add(n);
    for (const ref of envFileRefs(block)) {
      const resolved = envFileNames(path.resolve(path.dirname(file), ref));
      if (resolved === null) {
        notes.push(`${name}: env_file ${ref} is not present, so its variables are not judged`);
        continue;
      }
      for (const n of resolved) names.add(n);
    }
  }

  const set = (n) => names.has(n);
  const unlisted = [...names].filter((n) => n.startsWith('HYDRA_') && !clusterOnly.set.has(n) && !retired.set.has(n));
  const retiredSet = [...names].filter((n) => retired.set.has(n));

  // 2. Feature requirement, derived from what the manifest sets (not from a list here).
  const features = new Set([BIN_FEATURE]);
  if (set('HYDRA_CLUSTER_PEERS')) features.add('arachne');
  if (set('HYDRA_REDIS_URL') || set('HYDRA_REDIS_MODE')) features.add('cluster-redis');

  // A manifest that sets cluster WIRING but not the member list is the "standalone node that
  // silently drops its configuration" case `cluster_decision` reports as `WiringWithoutMembers`:
  // the manifests must not ship that shape at all.
  const wiringWithoutMembers = !set('HYDRA_CLUSTER_PEERS')
    && clusterOnly.names.filter((n) => n !== 'HYDRA_CLUSTER_PEERS').filter(set);

  return { label, file, services: services.length, names, unlisted, retired: retiredSet, features, wiringWithoutMembers, notes };
}

/** `--features a,b,c` lists in one recipe, one per build invocation. */
function readRecipe(label, file) {
  const text = fs.readFileSync(file, 'utf8');
  const lists = [];
  for (const line of text.split('\n')) {
    if (/^\s*#/.test(line)) continue;
    const m = /--features\s+([^\s"'\\]+)/.exec(line);
    if (m) lists.push({ line: line.trim(), features: new Set(m[1].split(',').map((f) => f.trim()).filter(Boolean)) });
  }
  if (lists.length === 0) {
    throw new Error(
      `found no \`--features …\` in ${label}; refusing to judge a recipe whose feature set this ` +
        'reader cannot read (if the build moved elsewhere, point RECIPES at its new home)',
    );
  }
  return { label, file, lists };
}

function main() {
  // A MISSING owner file is a CANNOT VERIFY, not a crash and not "no retired names": reading the
  // tables out of the source is the whole point, and a guard that dies with a stack trace where a
  // verdict belongs is indistinguishable from a broken CI step (caught by the self-test's
  // "the source tables moving" case, which used to throw ENOENT).
  const owner = path.join(REPO, 'crates/hydra-server/src/cluster/mod.rs');
  let clusterSrc;
  try {
    clusterSrc = fs.readFileSync(owner, 'utf8');
  } catch (e) {
    console.error(
      `[compose-env] CANNOT VERIFY: cannot read ${path.relative(REPO, owner)} (${e.code || e.message}) — ` +
        'the tables this guard judges the manifests against live there',
    );
    process.exit(2);
  }
  const clusterOnly = readTable(clusterSrc, 'CLUSTER_ONLY_ENV');
  const retired = readTable(clusterSrc, 'RETIRED_CLUSTER_ENV');
  if (!clusterOnly || !retired) {
    console.error(
      '[compose-env] CANNOT VERIFY: could not read CLUSTER_ONLY_ENV / RETIRED_CLUSTER_ENV from ' +
        'crates/hydra-server/src/cluster/mod.rs — the tables moved or changed shape',
    );
    process.exit(2);
  }

  let manifests;
  let recipes;
  try {
    manifests = MANIFESTS.map((rel) => readManifest(rel, path.join(REPO, rel), clusterOnly, retired));
    recipes = RECIPES.map((rel) => readRecipe(rel, path.join(REPO, rel)));
  } catch (e) {
    console.error(`[compose-env] CANNOT VERIFY: ${e.message}`);
    process.exit(2);
  }

  const problems = [];
  for (const m of manifests) {
    for (const n of m.retired) {
      problems.push(
        `${m.label}: sets ${n}, which ADR-0001 RETIRED — the node boots with an ERROR saying it is ` +
          'ignored, so the manifest is telling the reader the opposite of what happens',
      );
    }
    if (m.wiringWithoutMembers.length > 0) {
      problems.push(
        `${m.label}: sets cluster wiring (${m.wiringWithoutMembers.join(', ')}) but no ` +
          'HYDRA_CLUSTER_PEERS — that node boots standalone and drops every one of those settings',
      );
    }
    for (const n of m.unlisted) {
      // Not a failure on its own: a manifest may legitimately set a non-cluster knob
      // (HYDRA_ADMIN_TOKEN, HYDRA_USAGE_SINK, …). Reported so the reader can see what was judged.
      console.log(`[compose-env] note: ${m.label} sets ${n} (not a cluster-topology variable)`);
    }
    for (const note of m.notes) console.log(`[compose-env] note: ${m.label}: ${note}`);
  }

  // Rule 2 is a cross-product: EVERY recipe must cover EVERY manifest's requirement, because both
  // recipes stage a binary that a manifest in this directory runs.
  for (const r of recipes) {
    for (const m of manifests) {
      for (const list of r.lists) {
        const missing = [...m.features].filter((f) => !list.features.has(f));
        if (missing.length > 0) {
          problems.push(
            `${r.label}: \`${list.line}\` is missing feature(s) ${missing.join(', ')} required by ` +
              `${m.label} (which asks the binary for ` +
              `${[...m.features].filter((f) => f !== BIN_FEATURE).join(', ') || 'no optional feature'}); ` +
              `built that way the binary refuses to boot / silently ignores the setting`,
          );
        }
      }
    }
  }

  const judged = manifests.map((m) => `${m.label}=${m.services}`).join(', ');
  if (problems.length > 0) {
    for (const p of problems) console.error(`[compose-env] FAIL ${p}`);
    console.error(`[compose-env] ${problems.length} problem(s); services judged: ${judged}`);
    process.exit(1);
  }
  console.log(
    `[compose-env] OK: ${manifests.length} manifest(s) judged (${judged}); none sets any of the ` +
      `${retired.names.length} variable(s) RETIRED_CLUSTER_ENV retires, every cluster node carries ` +
      'the member list, and every recipe that stages a binary carries the features the manifests need',
  );
  console.log(
    `[compose-env] OK: recipes judged: ${recipes.map((r) => `${r.label}=${r.lists.length} build(s)`).join(', ')}`,
  );
  return;
}

if (require.main === module) main();

module.exports = { readTable, envNames, envFileRefs, envFileNames, readManifest, readRecipe, MANIFESTS, RECIPES };
