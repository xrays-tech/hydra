#!/usr/bin/env node
'use strict';
/**
 * Just enough compose-YAML reading for the two guards that must keep working when
 * `docker compose config` cannot run.
 *
 * Why this exists (round 121): `check_compose_grace.cjs` and `check_compose_health.cjs` both skip a
 * compose file when `docker compose config` fails with "needs the gitignored `secure/local-test.env`"
 * and that file is absent. That is the situation in CI (`/secure/` is gitignored, `.gitignore:49`),
 * so the LOCAL stack — `hydra-a`, `hydra-b`, `hydra-c` — was NEVER checked there, and the floor
 * (`MIN_SERVICES = 4`) happened to equal exactly the number of hydra services in the two files that
 * DO render. Measured 2026-09-30: deleting `stop_grace_period` (or a healthcheck) from those three
 * services still printed `OK … 4 hydra service(s) checked`.
 *
 * The static path is deliberately WEAKER than a rendered compose document: it reads text, so it
 * cannot see `extends:`, YAML anchors or values assembled from variables. It exists so that a
 * skipped file degrades to "checked less precisely" instead of "not checked", and callers must label
 * its results as static and refuse to pass when it finds no hydra service at all.
 *
 * Round 129 sharpened the second half: "less precisely" must never become "silently not at all".
 * `unjudgeableServices()` names the services a text read cannot judge (`extends:`, a `${VAR}`
 * image), and the callers REFUSE the whole file when that list is non-empty — measured before the
 * fix: a fixture whose `hydra-a` used `extends:` and whose `hydra-c` used `image: ${HYDRA_IMAGE_TAG}`
 * passed with only one of three services looked at.
 */

/** Service name -> its YAML block text, for the top-level `services:` mapping. */
function serviceBlocks(text) {
  const lines = text.split('\n');
  const blocks = new Map();
  let inServices = false;
  let current = null;
  for (const line of lines) {
    if (/^services:\s*$/.test(line)) {
      inServices = true;
      continue;
    }
    if (!inServices) continue;
    // A COMMENT is not a key: a column-zero `# …` line inside `services:` used to end the mapping,
    // so every service after it vanished — silently (measured 2026-09-30: `hydra-edge` disappeared
    // from `serviceBlocks` entirely, and the guards' floors still passed).
    if (/^\s*#/.test(line)) continue;
    if (/^\S/.test(line) && line.trim() !== '') {
      // A new top-level key ends the services mapping.
      inServices = false;
      current = null;
      continue;
    }
    // A trailing comment after the service name (`redis:   # local cache`) used to make the line
    // unrecognisable, so its keys were folded into the PREVIOUS service and this one vanished —
    // measured on the real `docker-compose.local.yml`: `hydra-c` was no longer checked at all.
    const m = /^  ([A-Za-z0-9_.-]+):\s*(?:#.*)?$/.exec(line);
    if (m) {
      current = m[1];
      blocks.set(current, []);
      continue;
    }
    if (current) blocks.get(current).push(line);
  }
  return new Map([...blocks].map(([name, body]) => [name, body.join('\n')]));
}

/**
 * Services whose `image:` matches `imageRe`, as `{ name, block }`.
 *
 * Returns ALL matches; a caller that needs at least one must check the length itself (a parse that
 * silently finds nothing is the failure mode these guards were written against).
 */
function hydraServices(text, imageRe) {
  const out = [];
  for (const [name, block] of serviceBlocks(text)) {
    // `scalarOwn`, not `scalar`: `image:` belongs to the service itself, and a nested decoy
    // (`x-decoy: { image: hydra:latest }`) must not make a service look hydra.
    // NOTE the `m` flag inside `scalarOwn`: a block is many lines, so without it `$` anchors to the
    // end of the whole block and the key only matches when it happens to be the LAST line — the
    // parser then reported zero hydra services on all three shipped files (measured).
    const value = scalarOwn(block, 'image');
    if (value === null) continue;
    if (!imageRe.test(value)) continue;
    out.push({ name, image: value, block });
  }
  return out;
}

/**
 * Services the STATIC reader cannot judge — and why.
 *
 * The static path exists so a file docker cannot render is checked "less precisely" rather than not
 * at all. But there are shapes where a text read is not merely less precise: it can report a service
 * as COMPLIANT (or not report it at all) while the real, rendered service violates the rule.
 * Measured 2026-09-30 with a fixture whose `hydra-a` used `extends: base-hydra` and whose `hydra-c`
 * used `image: ${HYDRA_IMAGE_TAG}`: the grace guard saw only `base-hydra` and printed OK (exit 0),
 * while two services that really do get deployed were never checked at all.
 *
 *   * `extends:` — the service inherits `image`, `stop_grace_period`, `healthcheck`, … from another
 *     service (possibly in another file), so nothing about it can be read from this text;
 *   * an `image:` containing `${…}` — the image name is decided at render time, so we cannot even
 *     tell whether this is a hydra service.
 *
 * Callers must REFUSE (CANNOT VERIFY) when this list is non-empty: silence here is the failure mode
 * this module was written to prevent.
 */
function unjudgeableServices(text) {
  const out = [];
  const anchors = collectAnchors(text);
  for (const [name, block] of serviceBlocks(text)) {
    if (scalarOwn(block, 'extends') !== null || /^\s+extends:\s*$/m.test(block) || /^\s+extends:\s*\S/m.test(block)) {
      out.push({ name, reason: 'uses `extends:` (it inherits image/grace/healthcheck from another service)' });
      continue;
    }
    const image = scalarOwn(block, 'image');
    if (image !== null && image.includes('${')) {
      out.push({ name, reason: `its image is not decided until render time (\`${image}\`)` });
      continue;
    }
    // A value at the SERVICE's own level that is decided at render time (`stop_grace_period:
    // ${HYDRA_STOP_GRACE:-30s}`) cannot be judged by a text reader. Round 156: the grace guard called
    // such a value `unparseable` and returned 1 — a DRIFT verdict against a legal compose file —
    // instead of refusing it. The shipped files only interpolate INSIDE `environment:`/`healthcheck:`
    // (nested, and the token is the escaped `$${…}` form), so this rule does not refuse them
    // (verified: `unjudgeableServices()` stays empty for all three real files).
    const unrendered = ownKeys(block).find((l) => /\$\{/.test(l) && !/^\s*#/.test(l));
    if (unrendered !== undefined) {
      out.push({
        name,
        reason: 'a value at its own level is not decided until render time ' +
          `(\`${unrendered.trim().slice(0, 60)}\`), so a text read cannot judge it`,
      });
      continue;
    }
    // A merge key at the SERVICE's own level brings in a whole mapping, so `image:`,
    // `stop_grace_period:` and `healthcheck:` are not necessarily this service's own — the text
    // reader follows only the `environment:` case, and this one must be refused, not guessed.
    const own = ownIndent(block);
    const ownMerge = own === null
      ? undefined
      : block.split('\n').find((l) => /^\s*<<:/.test(l) && /^\s*/.exec(l)[0].length === own);
    if (ownMerge !== undefined) {
      out.push({
        name,
        reason: 'merges a mapping at the SERVICE level (`<<:`), so its image/grace/healthcheck may ' +
          'come from the anchor — a text read cannot follow that',
      });
      continue;
    }
    const bad = mergeRefs(subBlock(block, 'environment')).filter(
      (r) => r.listForm || !anchors.has(r.name),
    );
    if (bad.length > 0) {
      const what = bad
        .map((r) => (r.listForm ? 'a LIST of anchors (`<<: [*a, *b]`)' : `\`*${r.name}\``))
        .join(', ');
      out.push({
        name,
        reason: `its \`environment:\` merges ${what}, which this file does not define inline: the ` +
          `inherited variables (HYDRA_ROLE among them) cannot be read`,
      });
      continue;
    }
    // `environment:` written as a LIST (`- HYDRA_ROLE=edge`) is legal compose and unreadable here
    // (`envValue` reads the mapping form). Round 155: the health guard's `|| 'all'` fallback then
    // turned a correct `edge` into `all` and told the operator to give an edge an admin probe — the
    // FALSE POSITIVE that guard's header exists to prevent, and the rendered path disagreed with the
    // static path about the SAME service. Refusing is this module's rule: unreadable is not guessed.
    const envBlock = subBlock(block, 'environment');
    if (envBlock.some((l) => /^\s*-\s+\S/.test(l)) && envValue(block, 'HYDRA_ROLE', anchors) === null) {
      out.push({
        name,
        reason: 'its `environment:` is a LIST (`- NAME=value`), which this text reader cannot read: ' +
          'HYDRA_ROLE and every other variable would look unset',
      });
      continue;
    }
    // A value decided at render time (`HYDRA_ROLE: ${NODE_ROLE:-edge}`) is unreadable for the same
    // reason — and reading it as "unset" means reading it as `all`.
    const roleValue = envValue(block, 'HYDRA_ROLE', anchors);
    if (roleValue !== null && roleValue.includes('${')) {
      out.push({
        name,
        reason: `HYDRA_ROLE is not decided until render time (\`${roleValue}\`), so role-dependent ` +
          `rules cannot judge this service`,
      });
    }
  }
  return out;
}

/**
 * Strip a YAML line comment from a scalar value (`30s # measured 25s drain`), respecting quotes.
 *
 * Without this, `stop_grace_period: 30s # …` — legal YAML, and the repository's compose files are
 * full of explanatory comments — parsed as an unparseable duration and the guard reported FAIL on a
 * correct file (measured 2026-09-30). A comment is not part of the scalar.
 */
function stripYamlComment(value) {
  let q = null;
  for (let i = 0; i < value.length; i += 1) {
    const c = value[i];
    if (q) {
      if (c === q) q = null;
      continue;
    }
    if (c === '"' || c === "'") {
      q = c;
      continue;
    }
    if (c === '#' && (i === 0 || /\s/.test(value[i - 1]))) return value.slice(0, i);
  }
  return value;
}

/**
 * The service block's OWN keys — the least-indented `key:` lines in it.
 *
 * `scalar`/`hasKey` used to search the whole block text with `^\s+key:`, which is depth-blind: a
 * key inside a NESTED mapping satisfied a question about the service. Measured 2026-09-30 with
 * `x-static-decoy: { healthcheck: { test: … } }` — the guard reported a probe-path violation for a
 * service that declares no healthcheck at all, and the same confusion can hide a missing one.
 */
/**
 * The least-indented key indent in this block — "the service's own level" — or null.
 *
 * Both `ownKeys` and the service-level merge check need it, and they must agree: a `<<:` line at
 * this indent merges a whole mapping INTO the service, so its `image`/grace/healthcheck are not
 * necessarily its own.
 */
function ownIndent(block) {
  const indents = block
    .split('\n')
    .map((l) => /^(\s+)[A-Za-z0-9_.-]+:/.exec(l))
    .filter(Boolean)
    .map((m) => m[1].length);
  return indents.length === 0 ? null : Math.min(...indents);
}

function ownKeys(block) {
  const indent = ownIndent(block);
  if (indent === null) return [];
  return block
    .split('\n')
    .filter((l) => /^(\s+)[A-Za-z0-9_.-]+:/.test(l) && /^\s*/.exec(l)[0].length === indent);
}

/**
 * Every anchor defined ON A KEY LINE in this file: name → the lines of the mapping it is attached to.
 *
 * `environment: &control-env` in the shipped cluster file is the real occurrence: without this the
 * anchor was just noise on the key line, and the service that MERGES it (`<<: *control-env`,
 * `docker-compose.cluster.yml:102`) had `HYDRA_ROLE` read as null.
 */
function collectAnchors(text) {
  const anchors = new Map();
  for (const [owner, block] of serviceBlocks(text)) {
    const lines = block.split('\n');
    for (let i = 0; i < lines.length; i += 1) {
      const m = /^(\s+)([A-Za-z0-9_.-]+):\s*&([A-Za-z0-9_-]+)\s*(?:#.*)?$/.exec(lines[i]);
      if (!m) continue;
      const indent = m[1].length;
      const body = [];
      for (let j = i + 1; j < lines.length; j += 1) {
        const l = lines[j];
        if (l.trim() === '') continue;
        if (l.match(/^\s*/)[0].length <= indent) break;
        body.push(l);
      }
      anchors.set(m[3], { owner, key: m[2], lines: body });
    }
  }
  return anchors;
}

/**
 * The `<<:` merge keys among these lines, as `{ name, listForm }`.
 *
 * YAML merge semantics that this reader models: the service's OWN keys win over the merged ones
 * (that is why the caller only consults the anchor when the own mapping has no such key). The list
 * form (`<<: [*a, *b]`) is reported as `listForm` and NOT guessed at.
 */
function mergeRefs(lines) {
  const out = [];
  for (const l of lines) {
    const m = /^\s*<<:\s*(.*?)\s*$/.exec(l);
    if (!m) continue;
    const value = stripYamlComment(m[1]).trim();
    const alias = /^\*([A-Za-z0-9_-]+)$/.exec(value);
    out.push(alias ? { name: alias[1], listForm: false } : { name: value, listForm: true });
  }
  return out;
}

/**
 * True when the service disables its healthcheck (`healthcheck: { disable: true }`).
 *
 * Docker composes this into "no healthcheck at all", so the rendered path reports the service as
 * UNSUPERVISED; the static path used to see "a healthcheck whose test is empty" and accuse the probe
 * of being wrong. Same file, two different diagnoses (measured) — both paths now agree.
 */
function healthcheckDisabled(block) {
  // Same anchoring rule as `subBlock`: the service's OWN `healthcheck:` line.
  return subBlock(block, 'healthcheck').some((l) => /^\s+disable:\s*true\s*$/.test(l));
}

/**
 * The lines of the sub-mapping introduced by this service's OWN `key:` line (empty when absent).
 *
 * `test:` lives under `healthcheck:` and `HYDRA_ROLE` under `environment:`, so those values must be
 * read from the right sub-mapping — a nested-tolerant "first `test:` anywhere" read can pick up a
 * DECOY's value that appears earlier in the block (the parser's own test caught exactly that: the
 * probe text came from `x-static-decoy.healthcheck.test`).
 */
function subBlock(block, key) {
  const lines = block.split('\n');
  // Anchor on the SERVICE's own `key:` line (the least-indented key lines), not the first line that
  // happens to spell it: a nested decoy (`x-static-decoy: { healthcheck: … }`) appears earlier in
  // the block and used to supply the sub-mapping (caught by the parser's own test).
  const keyLines = lines
    .map((l, i) => {
      const m = /^(\s+)([A-Za-z0-9_.-]+):/.exec(l);
      return m ? { i, indent: m[1].length, text: l } : null;
    })
    .filter(Boolean);
  if (keyLines.length === 0) return [];
  const own = Math.min(...keyLines.map((k) => k.indent));
  // The key line may carry a YAML ANCHOR (`environment: &control-env`) and/or a comment; requiring
  // exactly `key:` made the whole sub-mapping invisible. Measured on the shipped
  // `environment/docker-compose.cluster.yml:45`: `HYDRA_ROLE: leader` is written verbatim under
  // `hydra-control-a`'s `environment: &control-env`, and `subBlock` returned `[]` — so the role was
  // silently read as `all` (see `check_compose_health`, which falls back to `'all'`).
  const found = keyLines.find(
    (k) => k.indent === own && new RegExp(`^\\s+${key}:\\s*(?:&[A-Za-z0-9_-]+)?\\s*(?:#.*)?$`).test(k.text),
  );
  if (found === undefined) return [];
  const idx = found.i;
  const indent = own;
  const out = [];
  for (let i = idx + 1; i < lines.length; i += 1) {
    const l = lines[i];
    if (l.trim() === '') continue;
    if (l.match(/^\s*/)[0].length <= indent) break;
    out.push(l);
  }
  return out;
}

/** The command line of the service's own `healthcheck.test`, or null. */
function healthcheckTest(block) {
  const line = subBlock(block, 'healthcheck').find((l) => /^\s+test:/.test(l));
  if (line === undefined) return null;
  const m = /^\s+test:\s*(.*?)\s*$/.exec(line);
  return m ? m[1].replace(/^["']|["']$/g, '') : null;
}

/** `NAME: value` from one line, with quotes and trailing comment stripped, or null. */
function envLineValue(line, name) {
  const m = new RegExp(`^\\s+${name}:\\s*(.*?)\\s*$`).exec(line);
  if (!m) return null;
  const v = stripYamlComment(m[1]).trim().replace(/^["']|["']$/g, '');
  return v === '' ? null : v;
}

/**
 * A variable's value from the service's own `environment:` mapping, or null.
 *
 * `anchors` (from [`collectAnchors`]) enables YAML merge resolution: a service whose `environment:`
 * carries `<<: *control-env` inherits those variables, and the service's own keys win — so the
 * anchor is consulted ONLY when the own mapping does not define the name. Without `anchors` (or with
 * an anchor this file does not define) the answer is null, and callers must REFUSE rather than
 * substitute a default: `check_compose_health` would otherwise judge a merged `edge` as `all` and
 * demand an admin probe from a node that has no admin API.
 */
function envValue(block, name, anchors) {
  const env = subBlock(block, 'environment');
  const own = env.find((l) => new RegExp(`^\\s+${name}:`).test(l));
  if (own !== undefined) return envLineValue(own, name);
  for (const ref of mergeRefs(env)) {
    if (ref.listForm) return null;
    const anchor = anchors instanceof Map ? anchors.get(ref.name) : undefined;
    if (!anchor) return null;
    const line = anchor.lines.find((l) => new RegExp(`^\\s+${name}:`).test(l));
    if (line !== undefined) return envLineValue(line, name);
  }
  return null;
}

/**
 * The value of a scalar key ANYWHERE in the block, or null — nested-tolerant ON PURPOSE.
 *
 * Some keys live inside a sub-mapping by definition: `test:` is under `healthcheck:`, and
 * `HYDRA_ROLE` is under `environment:`. Restricting every lookup to the service's own keys made the
 * local stack's `HYDRA_ROLE: leader` invisible, so `hydra-a` was diagnosed as `role=all` (caught by
 * the suite). Keys that describe the SERVICE itself use [`scalarOwn`] instead.
 */
function scalar(block, key) {
  const re = new RegExp(`^\\s+${key}:\\s*(.*?)\\s*$`, 'm');
  const m = re.exec(block);
  if (!m) return null;
  const v = stripYamlComment(m[1]).trim().replace(/^["']|["']$/g, '');
  return v === '' ? '' : v;
}

/** The value of a key the SERVICE declares itself (not one nested inside another mapping). */
function scalarOwn(block, key) {
  const re = new RegExp(`^\\s+${key}:\\s*(.*?)\\s*$`);
  const line = ownKeys(block).find((l) => re.test(l));
  if (line === undefined) return null;
  const m = re.exec(line);
  const v = stripYamlComment(m[1]).trim().replace(/^["']|["']$/g, '');
  return v === '' ? '' : v;
}

/** True when the SERVICE declares this mapping key itself (not nested inside another mapping). */
function hasOwnKey(block, key) {
  const re = new RegExp(`^\\s+${key}:`);
  return ownKeys(block).some((l) => re.test(l));
}

module.exports = {
  serviceBlocks,
  hydraServices,
  scalar,
  scalarOwn,
  hasOwnKey,
  ownKeys,
  ownIndent,
  collectAnchors,
  mergeRefs,
  healthcheckDisabled,
  healthcheckTest,
  envValue,
  subBlock,
  unjudgeableServices,
  stripYamlComment,
};
