#!/usr/bin/env node
'use strict';
/**
 * Every hydra service must be supervised by a healthcheck that suits its ROLE.
 *
 * Why this exists (found 2026-09-29 by the round-72 cluster drill):
 *   * the OFFICIAL cluster topology (`environment/docker-compose.cluster.yml`) shipped
 *     `hydra-control-a`, `hydra-control-b` and `hydra-edge` with **no healthcheck at
 *     all**, while the single-node file and the local stack both have them — so a
 *     wedged node in the documented cluster topology was invisible to the orchestrator
 *     (the boot-time bind probe catches that at startup, nothing catches it later);
 *   * the probe path is ROLE-dependent, and getting it wrong is worse than no probe:
 *     an EDGE serves no admin API — measured 2026-09-29: `/healthz` 200 (token-free),
 *     `/api/v1/health` **404**, `/metrics` 401 without a token and 200 with one. A
 *     healthcheck written for a control node would therefore mark every healthy edge
 *     unhealthy. The local stack already probes `/healthz` on its edge; the cluster
 *     file had nothing.
 *
 * Rules (checked on the RENDERED compose, so anchors/`extends` cannot hide anything):
 *   1. every service whose image is `hydra*` declares a healthcheck;
 *   2. every hydra service must probe `/api/v1/health` **with** an Authorization
 *      header (it answers 401 otherwise, so a probe without the token would report
 *      unhealthy forever).
 *
 * The rule USED to be role-dependent: an `edge` served no admin API, so it had to be
 * probed through a token-free path. ADR-0001 retired the role (`HYDRA_ROLE` is
 * ignored), every node runs the admin API, and the edge exception went with it — one
 * rule for every service, which is also what makes a new service hard to get wrong.
 *
 * Usage: node scripts/check_compose_health.cjs [--compose-json=FILE]…
 * Exit:  0 ok · 1 a healthcheck is missing or wrong · 2 cannot verify.
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');
const {
  hydraServices,
  envValue,
  collectAnchors,
  hasOwnKey,
  healthcheckDisabled,
  healthcheckTest,
  unjudgeableServices,
} = require('./compose_static.cjs');

const ROOT = path.resolve(__dirname, '..');
const COMPOSE_FILES = [
  path.join(ROOT, 'environment', 'docker-compose.yml'),
  path.join(ROOT, 'environment', 'docker-compose.cluster.yml'),
  path.join(ROOT, 'environment', 'docker-compose.local.yml'),
];
const MIN_SERVICES = Number(process.env.COMPOSE_HEALTH_MIN_SERVICES || 4);
const IMAGE_RE = /(^|\/)hydra(-local)?:/;
const DUMMY_ENV = {
  HYDRA_ADMIN_TOKEN: 'dummy-admin-token-for-validation',
  HYDRA_CLUSTER_TOKEN: 'dummy-cluster-token-for-validation',
  HYDRA_ENCRYPTION_KEY: 'ZHVtbXkta2V5LWZvci12YWxpZGF0aW9uLW9ubHkAMDE=',
};

class ScanError extends Error {
  constructor(message) {
    super(message);
    this.code = 2;
  }
}

function parseArgs(argv) {
  const opts = { composeJson: [], staticFiles: [], help: false };
  for (const arg of argv) {
    if (arg.startsWith('--compose-json=')) opts.composeJson.push(arg.slice('--compose-json='.length));
    else if (arg.startsWith('--static-file=')) opts.staticFiles.push(arg.slice('--static-file='.length));
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else throw new ScanError(`unknown argument: ${arg}`);
  }
  return opts;
}

function render(file) {
  const res = spawnSync('docker', ['compose', '-f', file, 'config', '--format', 'json'], {
    cwd: ROOT, encoding: 'utf8', maxBuffer: 64 * 1024 * 1024, env: { ...process.env, ...DUMMY_ENV },
  });
  if (res.error) throw new ScanError(`could not run docker compose for ${path.relative(ROOT, file)}: ${res.error.message}`);
  if (res.status !== 0) {
    const err = `${res.stderr || ''}${res.stdout || ''}`;
    if (/secure\/local-test\.env/.test(err) && !fs.existsSync(path.join(ROOT, 'secure', 'local-test.env'))) {
      return { skipped: 'needs secure/local-test.env (absent)' };
    }
    throw new ScanError(`docker compose config failed for ${path.relative(ROOT, file)}: ${err.trim().split('\n')[0]}`);
  }
  try {
    return { doc: JSON.parse(res.stdout) };
  } catch (e) {
    throw new ScanError(`could not parse the rendered compose JSON for ${path.relative(ROOT, file)}: ${e.message}`);
  }
}

/**
 * The rule set, applied to values that either extractor produced: the rendered compose document or
 * the static YAML text. Factored out in round 121 so the two paths cannot drift apart (the static
 * path exists because a skipped file used to mean "not checked at all" — see `compose_static.cjs`).
 */
function evaluateService({ label, name, hasHealthcheck, test }) {
  const probs = [];
  if (!hasHealthcheck) {
    probs.push('no healthcheck (nothing supervises this node)');
  } else {
    const adminHealth = /\/api\/v1\/health/.test(test);
    const hasAuth = /Authorization:\s*Bearer/.test(test);
    if (!adminHealth) probs.push('every hydra node runs the admin API — probe /api/v1/health');
    if (!hasAuth) probs.push('/api/v1/health answers 401 without the admin token — the probe needs Authorization: Bearer $HYDRA_ADMIN_TOKEN');
  }
  return { label, name, test: test.slice(0, 120), probs };
}

/**
 * The STATIC path: same rules, values read from the compose TEXT.
 *
 * Used when `docker compose config` cannot run — in CI that is the LOCAL stack (it needs the
 * gitignored `secure/local-test.env`), so `hydra-a/b/c` were never checked there while the floor
 * (`MIN_SERVICES = 4`) happened to equal the two files that DO render. Refuses to pass when it
 * finds no hydra service (the failure mode this whole guard exists for).
 */
function staticHealthCheck(label, file) {
  const text = fs.readFileSync(file, 'utf8');
  // A service this reader cannot judge must stop the check, not be skipped: `extends:` and a
  // `${VAR}` image can hide a service entirely (measured: two of three hydra services vanished and
  // the guard still printed OK).
  const opaque = unjudgeableServices(text);
  if (opaque.length > 0) {
    throw new ScanError(
      `the static fallback cannot judge ${opaque.length} service(s) in ${label}: ` +
        opaque.map((o) => `${o.name} ${o.reason}`).join('; ') +
        ` — render the file (docker compose config) instead of relying on the text path`,
    );
  }
  const services = hydraServices(text, IMAGE_RE);
  const unfilteredNames = hydraNamedServices(text).filter((n) => !services.some((x) => x.name === n.name));
  if (unfilteredNames.length > 0) {
    throw new ScanError(
      `the static fallback would silently skip ${unfilteredNames.length} hydra-NAMED service(s) whose image ` +
        `does not match ${IMAGE_RE}: ${unfilteredNames.map((n) => `${n.name} (image ${n.image || '<none>'})`).join('; ')} ` +
        `— render the file (docker compose config) instead, or fix the image name`,
    );
  }
  if (services.length === 0) {
    throw new ScanError(`static fallback found no hydra-image service in ${label}; refusing to pass by finding nothing`);
  }
  // YAML anchors/merge keys (`environment: &control-env` + `<<: *control-env` in the shipped
  // cluster file): without them the role of a service that MERGES its environment read as null and
  // was silently replaced by `'all'` below — a merged `edge` would then be told to probe the admin
  // API it does not have. Unresolvable merges are refused by `unjudgeableServices` above.
  const anchors = collectAnchors(text);
  return services.map(({ name, block }) => evaluateService({
    label: `${label} (static)`,
    name,
    // The probe command comes from this service's OWN `healthcheck:` (a nested decoy appearing
    // earlier in the block used to supply the probe text — caught by the parser's own test). The
    // role is no longer read: ADR-0001 retired it, so there is one rule for every service.
    // `disable: true` composes into "no healthcheck" (see compose_static.js), so both paths agree.
    hasHealthcheck: hasOwnKey(block, 'healthcheck') && !healthcheckDisabled(block),
    test: healthcheckTest(block) || '',
  }));
}

/**
 * The `name: image` pairs of service NAMES that look like hydra nodes, read from the compose TEXT.
 *
 * ONLY the `services:` block: its sibling `volumes:` also declares 2-space `hydra-…-data:` keys, and the
 * first version of this scanner reported them as services without an image (the grace guard's suite
 * caught that; the same scanner lives here so both guards report the same thing).
 */
function hydraNamedServices(text) {
  const out = [];
  const lines = text.split('\n');
  let inServices = false;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (/^services:\s*$/.test(line)) {
      inServices = true;
      continue;
    }
    if (/^\S/.test(line)) {
      if (inServices) break;
      continue;
    }
    if (!inServices) continue;
    const m = /^  ([A-Za-z0-9_.-]*hydra[A-Za-z0-9_.-]*):\s*$/.exec(line);
    if (!m) continue;
    let image = null;
    for (let j = i + 1; j < lines.length; j += 1) {
      if (/^\s{0,2}\S/.test(lines[j])) break;
      const im = /^\s+image:\s*(\S+)\s*$/.exec(lines[j]);
      if (im) {
        image = im[1];
        break;
      }
    }
    out.push({ name: m[1], image });
  }
  return out;
}

function main(argv) {
  const opts = parseArgs(argv);
  if (opts.help) {
    console.log('usage: node scripts/check_compose_health.cjs [--compose-json=FILE]… [--static-file=FILE]…');
    return 0;
  }
  const sources = opts.composeJson.length > 0
    ? opts.composeJson.map((f) => ({ label: f, doc: JSON.parse(fs.readFileSync(f, 'utf8')) }))
    : COMPOSE_FILES.map((f) => ({ label: path.relative(ROOT, f), file: f, ...render(f) }));
  if (opts.staticFiles.length > 0) {
    for (const f of opts.staticFiles) {
      sources.push({ label: path.relative(ROOT, f), skipped: 'forced static check (--static-file)', file: path.resolve(f) });
    }
  }

  const results = [];
  const skipped = [];
  for (const src of sources) {
    if (src.skipped) {
      if (src.file) {
        skipped.push(`${src.label}: ${src.skipped} — checked STATICALLY instead (weaker: text, not a rendered document)`);
        results.push(...staticHealthCheck(src.label, src.file));
      } else {
        skipped.push(`${src.label}: ${src.skipped}`);
      }
      continue;
    }
    for (const [name, svc] of Object.entries(src.doc.services || {})) {
      if (!IMAGE_RE.test(String(svc.image || ''))) {
        // A service NAMED like a hydra node whose image does not match the filter used to be skipped
        // silently (round 185): its role/healthcheck were never checked while the OK line counted
        // "N hydra service(s) checked" as if they had been. The filter must not eat a service by NAME.
        if (/hydra/i.test(name)) {
          results.push({
            label: src.label,
            name,
            probs: [
              `the service NAME looks like a hydra node but its image \`${svc && svc.image ? svc.image : '<none>'}\` ` +
                `does not match ${IMAGE_RE} — the image filter skipped it, so its healthcheck is unchecked`,
            ],
          });
        }
        continue;
      }
      const hc = svc.healthcheck;
      const test = hc && Array.isArray(hc.test) ? hc.test.join(' ') : (hc ? String(hc.test) : '');
      results.push(evaluateService({ label: src.label, name, hasHealthcheck: Boolean(hc && hc.test), test }));
    }
  }

  if (results.length < MIN_SERVICES) {
    throw new ScanError(`only ${results.length} hydra-image service(s) found (< floor ${MIN_SERVICES}); the compose files may have changed shape`);
  }
  const bad = results.filter((r) => r.probs.length > 0);
  if (bad.length === 0) {
    console.log(`[compose-health] ${results.length} hydra service(s) checked: OK`);
    for (const r of results) console.log(`[compose-health]   OK ${r.label} · ${r.name}`);
    for (const s of skipped) console.log(`[compose-health]   SKIP ${s}`);
    return 0;
  }
  console.error(`[compose-health] FAIL: ${bad.length} of ${results.length} hydra service(s)`);
  for (const r of bad) console.error(`[compose-health]   ${r.label} · ${r.name}: ${r.probs.join('; ')}`);
  return 1;
}

try {
  process.exit(main(process.argv.slice(2)));
} catch (err) {
  if (err instanceof ScanError) {
    console.error(`[compose-health] CANNOT VERIFY: ${err.message}`);
    process.exit(err.code);
  }
  console.error(`[compose-health] ERROR: ${err.message}`);
  process.exit(2);
}
