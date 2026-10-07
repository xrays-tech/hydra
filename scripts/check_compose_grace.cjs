#!/usr/bin/env node
'use strict';
/**
 * Every service running the hydra image must allow a COMPLETE shutdown drain.
 *
 * Why this exists (measured 2026-09-29, release build, 14-core box):
 *   * the process drains in-flight requests for `HYDRA_SHUTDOWN_DRAIN_SECS`
 *     (default 20) and then spends up to 5s in pingora's final runtime step;
 *     measured wall-clock shutdown: **25s on SIGTERM**, **35s on SIGQUIT**;
 *   * a SIGKILLed drain loses the buffered usage records — control run: the same
 *     20 proxied requests left **0** rows in `usage_record` under SIGKILL and
 *     **20** rows when the drain completed. **Where that evidence lives now
 *     (2026-10-07)**: the rows then came from the node's own SQLite table, which
 *     ADR-0002 retired and migration `0013` DROPPED, so this sentence no longer
 *     names a store that exists. The claim is unchanged and is still measured, by
 *     the drills that count rows at a real HTTP ClickHouse
 *     (`integration/test_shutdown_drain.py` N3: "the usage row that was still
 *     BUFFERED when SIGTERM arrived is persisted after the process is gone") and
 *     by the same measurement run by hand at the time;
 *   * Docker's default stop grace is **10s** (`docker stop` / `compose down` /
 *     `restart` send SIGTERM, wait 10s, then SIGKILL) — so every routine
 *     container restart was cutting the drain short. None of the shipped compose
 *     files set `stop_grace_period`; they do now (30s).
 *
 * The threshold is not hard-coded here: it is read from `main.rs`
 * (`grace_period_seconds: Some(shutdown_drain_secs())` with the default parsed
 * out of `parse_shutdown_drain_secs`, plus the literal
 * `graceful_shutdown_timeout_seconds`), so raising the drain default makes this
 * check demand a larger deployment grace period automatically.
 *
 * Usage: node scripts/check_compose_grace.cjs
 *        node scripts/check_compose_grace.cjs --compose-json=rendered.json   # tests
 * Exit:  0 ok · 1 a hydra service's grace period is missing/too small ·
 *        2 cannot verify (compose command failed, too few services found, …)
 */

const fs = require('fs');
const path = require('path');
const { spawnSync } = require('child_process');
const { hydraServices, scalarOwn, unjudgeableServices } = require('./compose_static.cjs');

const ROOT = path.resolve(__dirname, '..');
const MAIN_RS = path.join(ROOT, 'crates', 'hydra-server', 'src', 'main.rs');
const COMPOSE_FILES = [
  path.join(ROOT, 'environment', 'docker-compose.yml'),
  path.join(ROOT, 'environment', 'docker-compose.cluster.yml'),
  path.join(ROOT, 'environment', 'docker-compose.local.yml'),
];
const SLACK_SECS = Number(process.env.COMPOSE_GRACE_SLACK || 5);
// The floor guards against "found nothing, therefore all good". It is 4 because
// the OFFICIAL topology is 1 (compose) + 3 (compose.cluster) hydra services; the
// LOCAL stack cannot even render in CI (its gitignored env_file is absent there)
// and is skipped with a printed reason.
const MIN_SERVICES = Number(process.env.COMPOSE_GRACE_MIN_SERVICES || 4);
const IMAGE_RE = /(^|\/)hydra(-local)?:/;

class ScanError extends Error {
  constructor(message) {
    super(message);
    this.code = 2;
  }
}

function parseArgs(argv) {
  const opts = { composeJson: [], staticFiles: [], mainRs: process.env.COMPOSE_GRACE_MAIN_RS || MAIN_RS, help: false };
  for (const arg of argv) {
    if (arg.startsWith('--compose-json=')) opts.composeJson.push(arg.slice('--compose-json='.length));
    else if (arg.startsWith('--static-file=')) opts.staticFiles.push(arg.slice('--static-file='.length));
    else if (arg.startsWith('--main-rs=')) opts.mainRs = arg.slice('--main-rs='.length);
    else if (arg === '--help' || arg === '-h') opts.help = true;
    else throw new ScanError(`unknown argument: ${arg}`);
  }
  return opts;
}

/** The two code constants the deployment grace period must cover. */
function codeBudget(mainRs = MAIN_RS) {
  const text = fs.readFileSync(mainRs, 'utf8');
  const drainFn = /fn parse_shutdown_drain_secs[\s\S]*?unwrap_or\((\d+)\)/.exec(text);
  const finalStep = /graceful_shutdown_timeout_seconds:\s*Some\((\d+)\)/.exec(text);
  if (!drainFn || !finalStep) {
    throw new ScanError(`could not read the shutdown budget from ${path.relative(ROOT, mainRs)} (drain default ${drainFn ? drainFn[1] : '?'}, final step ${finalStep ? finalStep[1] : '?'})`);
  }
  return { drain: Number(drainFn[1]), final: Number(finalStep[1]) };
}

/** Go-style duration ("30s", "1m30s", "500ms") or a bare number of seconds. */
function durationSecs(value) {
  if (typeof value === 'number') return value;
  if (typeof value !== 'string') return null;
  const m = /^(\d+(?:\.\d+)?)(ns|us|µs|ms|s|m|h)?$/.exec(value.trim());
  if (!m) return null;
  const n = Number(m[1]);
  switch (m[2]) {
    case undefined:
    case 's': return n;
    case 'ms': return n / 1000;
    case 'us':
    case 'µs': return n / 1e6;
    case 'ns': return n / 1e9;
    case 'm': return n * 60;
    case 'h': return n * 3600;
    default: return null;
  }
}

const DUMMY_ENV = {
  HYDRA_ADMIN_TOKEN: 'dummy-admin-token-for-validation',
  HYDRA_CLUSTER_TOKEN: 'dummy-cluster-token-for-validation',
  HYDRA_ENCRYPTION_KEY: 'ZHVtbXkta2V5LWZvci12YWxpZGF0aW9uLW9ubHkAMDE=',
};

function renderCompose(file) {
  const res = spawnSync('docker', ['compose', '-f', file, 'config', '--format', 'json'], {
    cwd: ROOT,
    encoding: 'utf8',
    maxBuffer: 64 * 1024 * 1024,
    env: { ...process.env, ...DUMMY_ENV },
  });
  if (res.error) throw new ScanError(`could not run docker compose for ${path.relative(ROOT, file)}: ${res.error.message}`);
  if (res.status !== 0) {
    const err = `${res.stderr || ''}${res.stdout || ''}`;
    // The LOCAL stack needs a gitignored env_file; its absence is expected on a
    // clean checkout (same convention as the local gate) and is not a failure.
    if (/secure\/local-test\.env/.test(err) && !fs.existsSync(path.join(ROOT, 'secure', 'local-test.env'))) {
      return { skipped: `needs secure/local-test.env (absent)` };
    }
    throw new ScanError(`docker compose config failed for ${path.relative(ROOT, file)}: ${err.trim().split('\n')[0]}`);
  }
  try {
    return { doc: JSON.parse(res.stdout) };
  } catch (e) {
    throw new ScanError(`could not parse docker compose JSON for ${path.relative(ROOT, file)}: ${e.message}`);
  }
}

function checkDoc(label, doc, need) {
  const services = doc && doc.services ? doc.services : {};
  const found = [];
  for (const [name, svc] of Object.entries(services)) {
    const image = svc && svc.image ? String(svc.image) : '';
    if (!IMAGE_RE.test(image)) {
      // A service NAMED like a hydra node whose image does not match the filter used to be skipped
      // silently (round 185): nothing about its `stop_grace_period` was checked, and the OK line said
      // "N hydra service(s) checked" as if it had been. The filter must never eat a service by NAME.
      if (/hydra/i.test(name)) {
        found.push({ name, image, raw: svc && svc.stop_grace_period, secs: null, unfiltered: true });
      }
      continue;
    }
    const raw = svc.stop_grace_period;
    const secs = durationSecs(raw === undefined ? null : raw);
    found.push({ name, image, raw, secs });
  }
  return found.map((s) => {
    if (s.unfiltered) {
      return {
        ...s,
        label,
        ok: false,
        why: `the service NAME looks like a hydra node but its image \`${s.image || '<none>'}\` does not ` +
          `match ${IMAGE_RE} — the image filter skipped it, so nothing about its stop_grace_period is checked`,
      };
    }
    if (s.secs === null) {
      return { ...s, label, ok: false, why: `no stop_grace_period (Docker's default 10s < ${need}s drain budget)` };
    }
    if (s.secs < need) {
      return { ...s, label, ok: false, why: `stop_grace_period=${s.raw} (${s.secs}s) < ${need}s` };
    }
    return { ...s, label, ok: true };
  });
}

/**
 * The STATIC path: read the compose file as text and check every hydra-image service.
 *
 * Used when `docker compose config` cannot run (in CI the LOCAL stack needs the gitignored
 * `secure/local-test.env`, so that file was skipped there and its three services were never
 * checked — while the floor happened to equal the 4 services of the two files that DO render).
 * Weaker than a rendered document by construction: `extends:`/anchors/variable interpolation are
 * invisible here. It must therefore never pass by finding nothing.
 */
function staticGraceCheck(label, file, need) {
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
  // Same rule as the rendered path: a hydra-NAMED service the image filter does not select must be
  // reported, not skipped.
  const named = hydraNamedServices(text);
  const unfiltered = named
    .filter((n) => !services.some((s) => s.name === n.name))
    .map((n) => ({
      label: `${label} (static)`,
      name: n.name,
      raw: n.image || 'MISSING',
      ok: false,
      why: `the service NAME looks like a hydra node but its image \`${n.image || '<none>'}\` does not ` +
        `match ${IMAGE_RE} — the image filter skipped it, so nothing about its stop_grace_period is checked`,
    }));
  if (services.length === 0 && unfiltered.length === 0) {
    throw new ScanError(`static fallback found no hydra-image service in ${label}; refusing to pass by finding nothing`);
  }
  return unfiltered.concat(services.map(({ name, block }) => {
    const raw = scalarOwn(block, 'stop_grace_period');
    const secs = raw === null ? null : durationSecs(raw);
    if (secs === null) {
      return {
        label: `${label} (static)`,
        name,
        raw: raw === null ? 'MISSING' : raw,
        ok: false,
        why: raw === null
          ? 'no `stop_grace_period`: docker stops with SIGTERM, waits its 10s default, then SIGKILLs — the drain is cut short and buffered usage is lost'
          : `unparseable \`stop_grace_period: ${raw}\``,
      };
    }
    return { label: `${label} (static)`, name, raw, ok: secs >= need, why: `stop_grace_period=${raw} < required ${need}s` };
  }));
}

/**
 * The `name: image` pairs of service NAMES that look like hydra nodes, read from the compose TEXT.
 *
 * Deliberately name-based and shallow: its only job is to catch a service the IMAGE filter would skip
 * (`unjudgeableServices` handles the shapes that cannot be read at all), so a false positive here costs
 * one line of output, while a miss costs an unchecked service.
 */
function hydraNamedServices(text) {
  const out = [];
  const lines = text.split('\n');
  // ONLY the `services:` block: its sibling `volumes:` also declares 2-space `hydra-…-data:` keys, and
  // the first version of this scanner reported them as services without an image (measured: the
  // shipped local stack produced two false findings, and the suite's own CONTROL caught it).
  let inServices = false;
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (/^services:\s*$/.test(line)) {
      inServices = true;
      continue;
    }
    if (/^\S/.test(line)) {
      if (inServices) break; // left the services block
      continue;
    }
    if (!inServices) continue;
    const m = /^  ([A-Za-z0-9_.-]*hydra[A-Za-z0-9_.-]*):\s*$/.exec(line);
    if (!m) continue;
    let image = null;
    for (let j = i + 1; j < lines.length; j += 1) {
      if (/^\s{0,2}\S/.test(lines[j])) break; // the next service (or the end of the block)
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
    console.log('usage: node scripts/check_compose_grace.cjs [--compose-json=FILE]… [--static-file=FILE]…');
    return 0;
  }

  const budget = codeBudget(opts.mainRs);
  const need = budget.drain + budget.final + SLACK_SECS;
  const sources = opts.composeJson.length > 0
    ? opts.composeJson.map((f) => ({ label: f, doc: JSON.parse(fs.readFileSync(f, 'utf8')) }))
    : COMPOSE_FILES.map((f) => ({ label: path.relative(ROOT, f), file: f, ...renderCompose(f) }));
  if (opts.staticFiles.length > 0) {
    // Tests drive the static path directly (no docker, no fixtures that depend on the environment).
    for (const f of opts.staticFiles) {
      const label = path.relative(ROOT, f);
      sources.push({ label, skipped: 'forced static check (--static-file)', file: path.resolve(f) });
    }
  }

  const results = [];
  const skipped = [];
  for (const src of sources) {
    if (src.skipped) {
      // A skipped SHIPPED file is not "nothing to check": fall back to reading its text.
      if (src.file) {
        skipped.push(`${src.label}: ${src.skipped} — checked STATICALLY instead (weaker: text, not a rendered document)`);
        results.push(...staticGraceCheck(src.label, src.file, need));
      } else {
        skipped.push(`${src.label}: ${src.skipped}`);
      }
      continue;
    }
    results.push(...checkDoc(src.label, src.doc, need));
  }

  if (results.length < MIN_SERVICES) {
    throw new ScanError(`only ${results.length} hydra-image service(s) found (< floor ${MIN_SERVICES}); the compose files may have changed shape — refusing to "pass" by finding nothing`);
  }
  const bad = results.filter((r) => !r.ok);
  const summary = `[compose-grace] drain ${budget.drain}s + final ${budget.final}s + slack ${SLACK_SECS}s = ${need}s required; ${results.length} hydra service(s) checked`;
  if (bad.length === 0) {
    console.log(`${summary}: OK`);
    for (const r of results) console.log(`[compose-grace]   OK ${r.label} · ${r.name} (stop_grace_period=${r.raw})`);
    for (const s of skipped) console.log(`[compose-grace]   SKIP ${s}`);
    return 0;
  }
  console.error(`${summary}: FAIL`);
  for (const r of bad) console.error(`[compose-grace]   ${r.label} · ${r.name}: ${r.why}`);
  console.error('[compose-grace] a SIGKILLed drain loses buffered usage (measured: 20 requests -> 0 rows, counted in the usage store of the day; see the header for where that is measured now)');
  return 1;
}

try {
  process.exit(main(process.argv.slice(2)));
} catch (err) {
  if (err instanceof ScanError) {
    console.error(`[compose-grace] CANNOT VERIFY: ${err.message}`);
    process.exit(err.code);
  }
  console.error(`[compose-grace] ERROR: ${err.message}`);
  process.exit(2);
}
