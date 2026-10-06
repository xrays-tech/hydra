#!/usr/bin/env node
/* Static contract pre-flight for the Playwright suite.
 *
 * WHY THIS EXISTS
 * `tests/e2e/admin.spec.cjs` is the ONLY coverage for several behaviours (the
 * provider concurrency round-trip T2.2c, the reload-failure toast T2.2d, the
 * health-page fallback T9.5c), and it cannot run everywhere: it needs Chromium
 * plus a freshly built and seeded instance. A selector typo, a renamed `id`, a
 * nav key that no longer exists or a `data-field` that the form stopped
 * rendering therefore survives review — and then either turns CI red on the next
 * run or, worse, makes the case assert nothing at all.
 *
 * This checker reads the specs and the admin-UI sources and fails on that drift.
 * Round 122 widened the timestamp rule and added the two floors it was missing:
 *   * the rule matched ONE argument shape (`… 'path', … body: {` within 80 characters), so a
 *     payload built in a variable — `const payload = {…}; api('POST', '/providers', { body:
 *     payload })` — escaped it entirely (measured: a fixture that dropped BOTH timestamps exited 0).
 *     It now matches every `api('POST'|'PUT', …)` call, follows `body: <ident>` to a same-file
 *     object literal (one hop), and REPORTS a payload it cannot read instead of skipping it.
 *   * every POST/PUT to a timestamp-requiring resource must have been READ (`payloadsRead ==
 *     requiring`), with a floor on the number of calls seen at all; and the pkill walk, which had
 *     no count whatsoever, now has one (`MIN_HARNESS_FILES`) so a moved directory cannot turn
 *     "never kill by process name" into a silent no-op.
 *
 * It is deliberately SHALLOW: it proves the tokens a spec depends on still exist
 * somewhere in the UI, not that the case would pass. Behaviour stays the
 * browser's job.
 *
 * It also refuses to pass vacuously: a checker that extracts nothing and prints
 * OK is the failure mode this repository keeps running into, so the number of
 * tokens examined is asserted against a floor.
 *
 * ALSO CHECKED: the timestamp fields an API-based fixture must send. The suite
 * builds some fixtures with `api('POST', …)` instead of the UI, and the handlers
 * deserialise straight into the entity structs, where `created_at`/`updated_at` are
 * REQUIRED (`Provider`, `Tenant`, …). Two cases added in this session omitted them
 * and were RED the first time the suite was ever run locally (plan §2ah) — a static
 * check that only looked at selectors could not see it.
 *
 * NOT COVERED, on purpose: API routes. The admin dispatcher matches path
 * SEGMENTS (`parts == ["cluster", "status"]` in `admin/mod.rs`), so a textual
 * check of the glob patterns the specs pass to `page.route` would be guesswork;
 * the route assertions are left to the browser run.
 *
 * Usage:
 *   node scripts/check_e2e_contracts.cjs                 # repo default paths
 *   node scripts/check_e2e_contracts.cjs --spec FILE     # one spec file
 * Exit: 0 = every token found, 1 = drift, 2 = usage/IO problem.
 */
"use strict";
const fs = require("fs");
const { records, audit } = require("./recorded_exceptions.cjs");
const path = require("path");

// The root is overridable so the two rules about the UI SOURCE LIST (existence, completeness) can be
// exercised against a fixture tree, like every other guard's root (round 186).
const ROOT = process.env.E2E_CONTRACTS_ROOT
  ? path.resolve(process.env.E2E_CONTRACTS_ROOT)
  : path.join(__dirname, "..");
const UI_FILES = ["admin-ui/app.js", "admin-ui/stats.js", "admin-ui/api-docs.js", "admin-ui/index.html", "admin-ui/style.css", "admin-ui/i18n.js"];
// UI source files deliberately NOT read by this guard, each with a reason. EMPTY ON PURPOSE: today the
// six files above are exactly the six sources under `admin-ui/`. A file that is added to the directory
// and not listed here used to be invisible — a selector declared only there would read as "no admin-ui
// source declares id X" (false drift) — so it is a finding now, and this list is where a legitimate
// exception (a vendored asset, a generated bundle) gets RECORDED instead of skipped (round 186).
const UNSCANNED_UI_OK = records(process.env.E2E_CONTRACTS_UNSCANNED_UI, []);
const UI_SOURCE_EXT = /\.(js|cjs|mjs|ts|html|css)$/;

/** Minimum number of tokens that must be extracted, or the run is meaningless.
 *  Overridable so a focused fixture (a two-line spec exercising one rule) does not
 *  have to pad itself with selectors it does not care about; the default is what
 *  guards the real suite against a broken extraction. */
const { maskJsLiteralsReport } = require("./js_blank.cjs");

const MIN_TOKENS = Number(process.env.E2E_CONTRACTS_MIN_TOKENS ?? 40);
// Floor on api('POST'|'PUT', …) calls: the real suite has 7, and a call regex that stops matching
// must FAIL rather than report "no problems found".
const MIN_API_SITES = Number(process.env.E2E_CONTRACTS_MIN_API_SITES ?? 5);
// Floor on the files the pkill rule walks: that walk had no count at all, so renaming a harness
// directory would have turned "never kill by process name" into a silent no-op that still said OK.
const MIN_HARNESS_FILES = Number(process.env.E2E_CONTRACTS_MIN_HARNESS_FILES ?? 30);

/** The brace-balanced object literal starting at `open`, as `{ text, end }` (or null). */
function balancedObject(src, open) {
  if (src[open] !== "{") return null;
  let depth = 0;
  for (let i = open; i < src.length; i += 1) {
    const c = src[i];
    if (c === "{") depth += 1;
    else if (c === "}") {
      depth -= 1;
      if (depth === 0) return { text: src.slice(open + 1, i), end: i };
    }
  }
  return null;
}

/** The text of `const|let|var <name> = { … }` in the same file, or null. */
function objectLiteralOf(src, name) {
  const m = new RegExp(`(?:const|let|var)\\s+${name}\\s*=\\s*\\{`).exec(src);
  if (!m) return null;
  const open = src.indexOf("{", m.index);
  const obj = balancedObject(src, open);
  return obj ? obj.text : null;
}

function readSpecs(argv) {
  const i = argv.indexOf("--spec");
  if (i !== -1) {
    const f = argv[i + 1];
    if (!f) {
      console.error("error: --spec needs a path");
      process.exit(2);
    }
    return [path.resolve(f)];
  }
  const dir = path.join(ROOT, "tests", "e2e");
  return fs
    .readdirSync(dir)
    .filter((f) => f.endsWith(".spec.cjs"))
    .sort()
    .map((f) => path.join(dir, f));
}

/** Every quoted argument of a selector-taking Playwright call. */
function selectorsIn(src) {
  const out = [];
  const calls = /\b(?:locator|fill|waitForSelector|selectOption|click|check|uncheck|isVisible|textContent|inputValue)\(\s*(['"`])((?:\\.|(?!\1).)*)\1/g;
  let m;
  while ((m = calls.exec(src))) out.push({ raw: m[2], index: m.index });
  return out;
}

/** `navItem(page, 'key')` section keys. */
function navKeysIn(src) {
  const out = [];
  const re = /navItem\(\s*\w+\s*,\s*(['"])([^'"]+)\1/g;
  let m;
  while ((m = re.exec(src))) out.push({ raw: m[2], index: m.index });
  return out;
}

// REMOVED (round 190): a `assertedStringsIn()` helper used to sit here — it collected the literal
// strings a spec asserts are on screen (`toContainText('…')`), and NOTHING called it. The obvious rule
// it was written for ("every asserted literal must exist in a UI source") is UNSOUND, and that is
// measured rather than assumed: across the three specs there are 12 literal assertions, and 4 of them
// are values the SPEC ITSELF creates at runtime (`node-b` and two seeded hostnames) — they appear in
// neither the UI sources nor `tests/e2e/seed-data.json`. A static check would therefore have reported
// false drift on a correct suite, so the helper is gone instead of wired up for its own sake. The
// decision is pinned by a case in `check_e2e_contracts.test.cjs` ("a runtime-created asserted value is
// not drift"), so re-adding the naive rule reddens that test first.

function lineOf(src, index) {
  return src.slice(0, index).split("\n").length;
}

/**
 * 1-based line numbers of lines that kill a process by NAME, in any of the harness files.
 *
 * Only three EXECUTABLE shapes count, so prose (a docstring or comment explaining the old
 * command — like the ones this fix added) is not reported as a violation:
 *   1. an argv list:      ["pkill", "-x", "hydra"]
 *   2. a shell string:    subprocess.run("pkill -x hydra", shell=True)
 *   3. a bare command:    pkill -x hydra   /  sudo killall hydra
 * `pkill -f "$BIN"` matches none of them: it names a path, not a process name.
 */
function nameKillLines(src) {
  const NAME = "(?:hydra|node|cargo)";
  const SHAPES = [
    new RegExp(`\\[\\s*["'](?:pkill|killall)["']\\s*,\\s*["']-[a-zA-Z0-9]+["']\\s*,\\s*["']${NAME}["']`),
    new RegExp(`["'](?:pkill|killall)\\s+-[a-zA-Z0-9]+\\s+${NAME}\\b`),
    new RegExp(`^\\s*(?:[^\\n;&|]*[;&|]\\s*)?(?:sudo\\s+)?(?:pkill|killall)\\s+(?:-[a-zA-Z0-9]+\\s+)*${NAME}\\b`),
  ];
  const hits = [];
  for (const [i, line] of src.split("\n").entries()) {
    if (/^\s*(#|\/\/|\*)/.test(line)) continue; // comment
    if (SHAPES.some((re) => re.test(line))) hits.push(i + 1);
  }
  return hits;
}

function main() {
  const specFiles = readSpecs(process.argv.slice(2));
  if (!specFiles.length) {
    console.error("error: no spec files found");
    process.exit(2);
  }

  // PRECONDITIONS about the list itself (round 186). Both directions are findings:
  //   * a LISTED file that is missing made every selector check read an EMPTY source, so the guard
  //     reported dozens of false drifts ("no admin-ui source declares id X") instead of the one fact
  //     that matters — the file is gone;
  //   * an UNLISTED source file means a token declared only there is invisible to the same checks.
  const uiProblems = [];
  for (const f of UI_FILES) {
    if (!fs.existsSync(path.join(ROOT, f))) {
      uiProblems.push(
        `${f} is listed in UI_FILES but does not exist: every selector/nav check below would read an ` +
          `empty source and report false drift instead of naming the missing file`,
      );
    }
  }
  const uiDir = path.join(ROOT, "admin-ui");
  const onDisk = fs.existsSync(uiDir)
    ? fs.readdirSync(uiDir, { withFileTypes: true })
        .filter((e) => e.isFile() && UI_SOURCE_EXT.test(e.name))
        .map((e) => `admin-ui/${e.name}`)
    : [];
  const { unrecorded: unlistedUi, stale: staleUiRecords } = audit({
    records: UNSCANNED_UI_OK,
    needed: onDisk.filter((f) => !UI_FILES.includes(f)),
    applies: (f) => onDisk.includes(f) && !UI_FILES.includes(f),
  });
  for (const f of unlistedUi) {
    uiProblems.push(
      `${f} is a UI source under admin-ui/ but is NOT in UI_FILES and not recorded in ` +
        `UNSCANNED_UI_OK: a selector or nav key declared only there reads as missing`,
    );
  }
  for (const f of staleUiRecords) {
    uiProblems.push(
      `UNSCANNED_UI_OK records ${f}, but that no longer applies (the file is gone or is listed in ` +
        `UI_FILES now): a recorded decision that cannot expire is a stale claim`,
    );
  }
  if (uiProblems.length > 0) {
    for (const p of uiProblems) console.error(`DRIFT  ${p}`);
    console.error(`${uiProblems.length} UI-source list problem(s)`);
    process.exit(1);
  }

  const ui = UI_FILES.map((f) => {
    const p = path.join(ROOT, f);
    return fs.existsSync(p) ? fs.readFileSync(p, "utf8") : "";
  }).join("\n");

  /** Does the UI source contain this token at all? */
  const esc = (t) => t.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  /** A quote character class, built once so the patterns below stay readable. */
  const Q = "[\"'`]";
  const hasWord = (tok) => new RegExp(`(^|[^\\w$.-])${esc(tok)}($|[^\\w$-])`).test(ui);
  const hasId = (id) => new RegExp("id[=:]\\s*" + Q + esc(id) + Q).test(ui);
  const hasDataField = (name) => new RegExp("(name|field):\\s*" + Q + esc(name) + Q).test(ui);
  const hasEnglishValue = (v) => new RegExp(Q + esc(v) + Q).test(ui);
  /** A `data-*` attribute: either the literal HTML attribute or the `dataset`
   *  property the UI actually assigns (`dataset: { key }` IS `data-key`). Its
   *  VALUE may be computed at runtime (the nav keys are), so the value is checked
   *  for existence in the UI copy rather than for a literal assignment. */
  const hasDataAttr = (attr, val) => {
    const camel = attr.replace(/^data-/, "").replace(/-([a-z])/g, (_, c) => c.toUpperCase());
    const literal = new RegExp(esc(attr) + "\\s*=\\s*" + Q + esc(val) + Q).test(ui);
    const viaDataset =
      new RegExp("dataset:\\s*\\{[^}]*\\b" + esc(camel) + "\\b").test(ui) && hasEnglishValue(val);
    return literal || viaDataset;
  };

  const problems = [];
  let checked = 0;

  for (const file of specFiles) {
    const src = fs.readFileSync(file, "utf8");
    const rel = path.relative(ROOT, file);

    for (const { raw, index } of selectorsIn(src)) {
      // Split a compound selector into the tokens that name something in the UI.
      const at = `${rel}:${lineOf(src, index)}`;
      // Skip template-built selectors (`${...}`), localStorage keys and URLs.
      if (raw.includes("${") || /:\/\//.test(raw) || raw.startsWith("hydra-admin")) continue;
      if (!/[#.\[]/.test(raw)) continue;
      for (const id of raw.match(/#[\w-]+/g) || []) {
        checked++;
        const name = id.slice(1);
        if (!hasId(name)) problems.push(`${at}: selector "${raw}" needs id "${name}", which no admin-ui source declares`);
      }
      for (const cls of raw.match(/\.[\w-]+/g) || []) {
        checked++;
        const name = cls.slice(1);
        if (!hasWord(name)) problems.push(`${at}: selector "${raw}" needs class "${name}", which no admin-ui source mentions`);
      }
      for (const m of raw.matchAll(/\[([\w-]+)(?:=["']([^"']*)["'])?\]/g)) {
        const [, attr, val] = m;
        if (attr === "data-field" && val) {
          checked++;
          if (!hasDataField(val)) problems.push(`${at}: selector "${raw}" needs a form field named "${val}"`);
        } else if (attr === "title" && val) {
          checked++;
          if (!hasEnglishValue(val)) problems.push(`${at}: selector "${raw}" needs the label "${val}" to exist in the UI copy`);
        } else if (attr.startsWith("data-") && val) {
          checked++;
          if (!hasDataAttr(attr, val)) {
            problems.push(`${at}: selector "${raw}" needs ${attr}="${val}" (literal attribute or the dataset property the UI assigns)`);
          }
        } else if (!["disabled", "type"].includes(attr)) {
          checked++;
          if (!hasWord(attr)) problems.push(`${at}: selector "${raw}" needs attribute "${attr}"`);
        }
      }
    }

    for (const { raw, index } of navKeysIn(src)) {
      checked++;
      const at = `${rel}:${lineOf(src, index)}`;
      // Every nav key is a section key in the UI (`"providers"`, `"health"`, ...).
      if (!new RegExp(`["'\`]${raw}["'\`]`).test(ui)) {
        problems.push(`${at}: nav key "${raw}" does not appear in any admin-ui source`);
      }
    }
  }

  /* Required timestamp fields in API-based fixtures ----------------------------
   * Deliberately narrow: only `created_at`/`updated_at`, and only for resources
   * whose entity struct declares them as required. A general "send every field"
   * rule would be wrong (many fields are `Option`/`#[serde(default)]`); this rule
   * is exactly the defect that got through, and it can be widened when another
   * required field bites. */
  const STRUCT_OF_RESOURCE = {
    "/providers": "Provider",
    "/provider-keys": "ProviderKey",
    "/provider-models": "ProviderModel",
    "/tenants": "Tenant",
    "/limit-roles": "LimitRole",
    "/sub-tenants": "SubTenant",
    "/key-prefix-bindings": "ProviderKeyBinding",
    "/tenant-models": "TenantModel",
    "/tenant-providers": "TenantProvider",
  };
  const modelSrc = fs.existsSync(path.join(ROOT, "crates", "hydra-core", "src", "model.rs"))
    ? fs.readFileSync(path.join(ROOT, "crates", "hydra-core", "src", "model.rs"), "utf8")
    : "";
  const requiredTimestamps = new Set();
  if (modelSrc) {
    for (const m of modelSrc.matchAll(/pub struct (\w+)\s*\{([\s\S]*?)\n\}/g)) {
      const [, name, body] = m;
      // A non-Option String field, so serde requires it in the JSON body.
      if (/pub created_at:\s*String/.test(body) && /pub updated_at:\s*String/.test(body)) {
        requiredTimestamps.add(name);
      }
    }
  }
  if (modelSrc && requiredTimestamps.size < 3) {
    problems.push(`only ${requiredTimestamps.size} entity struct(s) with required timestamps parsed from model.rs; the pattern is probably wrong`);
  }

  let apiSites = 0; // every api('POST'|'PUT', …) call in every spec
  let requiring = 0; // …of those, the ones whose resource REQUIRES timestamps
  let payloadsRead = 0; // …of those, the ones whose payload this guard actually read
  const unreadable = [];
  for (const file of specFiles) {
    const src = fs.readFileSync(file, "utf8");
    const rel = path.relative(ROOT, file);
    // Match the CALL, not one argument shape. The original regex required
    // `… , 'path', … body: {` within 80 characters, so a payload built in a variable
    // (`const payload = {…}; await api('POST', '/providers', { body: payload })`) was invisible:
    // measured 2026-09-30 with a fixture that dropped BOTH timestamps and still exited 0.
    // Everything is matched/search on the MASKED copy (identical offsets), so a call that only
    // appears inside a comment or a string is not a call at all; the method and path are then read
    // back from the ORIGINAL text through the captured quote positions (the quotes survive masking).
    const { masked, unterminated } = maskJsLiteralsReport(src);
    // A spec with an unterminated string/template is a syntax error, and scanning to the next quote
    // would blank everything after it — silently emptying this rule (measured 2026-09-30: one stray
    // backtick made every later `api()` call invisible, leaving the rule alive only because the
    // unrelated `MIN_API_SITES` floor happened to catch it). Say so instead.
    if (unterminated) {
      problems.push(
        `${rel}: an unterminated string or template literal — this file cannot be parsed as JS, ` +
          `and until it is fixed the api()/timestamp checks below prove nothing`,
      );
      continue;
    }
    for (const m of masked.matchAll(/api\(\s*(['"])([^'"]*)\1\s*,\s*(['"])([^'"]*)\3/dg)) {
      const method = src.slice(m.indices[2][0], m.indices[2][1]);
      const rawPath = src.slice(m.indices[4][0], m.indices[4][1]);
      if (method !== "POST" && method !== "PUT") continue;
      apiSites += 1;
      // The resource is the path without its id segment.
      const resource = "/" + rawPath.split("/").filter(Boolean)[0];
      const structName = STRUCT_OF_RESOURCE[resource];
      if (!structName || !requiredTimestamps.has(structName)) continue;
      requiring += 1;

      // The request options object: the next brace-balanced literal after the path.
      const open = masked.indexOf("{", m.index + m[0].length);
      const options = open === -1 ? null : balancedObject(masked, open);
      if (!options) {
        unreadable.push(`${rel}:${lineOf(src, m.index)}: ${method} ${rawPath} — could not read the request options object`);
        continue;
      }
      const body = /\bbody\s*:\s*(?:\{|([A-Za-z_$][\w$]*))/.exec(options.text);
      if (!body) continue; // no body sent: nothing for this rule to check
      let payload = null;
      if (body[1]) {
        // `body: payload` — resolve a same-file object literal (one hop).
        payload = objectLiteralOf(masked, body[1]);
        if (!payload) {
          unreadable.push(
            `${rel}:${lineOf(src, m.index)}: ${method} ${rawPath} sends \`body: ${body[1]}\`, which is not an object literal in this file, ` +
              `so whether it carries created_at/updated_at cannot be checked — build the payload inline or as \`const ${body[1]} = { … }\` in the same spec`,
          );
          continue;
        }
      } else {
        const brace = options.text.indexOf("{", body.index);
        const inline = brace === -1 ? null : balancedObject(options.text, brace);
        if (!inline) {
          unreadable.push(`${rel}:${lineOf(src, m.index)}: ${method} ${rawPath} — the inline body object is not brace-balanced`);
          continue;
        }
        payload = inline.text;
      }
      payloadsRead += 1;
      for (const field of ["created_at", "updated_at"]) {
        if (!new RegExp(`\\b${field}\\s*:`).test(payload)) {
          problems.push(
            `${rel}:${lineOf(src, m.index)}: ${method} ${rawPath} omits \`${field}\`, which ${structName} requires (the handler deserialises straight into it)`,
          );
        }
      }
    }
  }
  for (const u of unreadable) problems.push(u);
  // Every POST/PUT to a timestamp-requiring resource must have been READ: a shape that silently
  // skips one is exactly the defect this rule was widened for.
  if (payloadsRead < requiring) {
    problems.push(`only ${payloadsRead} of ${requiring} timestamp-requiring call(s) were verified`);
  }
  if (apiSites < MIN_API_SITES) {
    problems.push(`only ${apiSites} api('POST'|'PUT', …) call(s) found in ${specFiles.length} spec file(s) (< floor ${MIN_API_SITES}); the call regex is probably wrong`);
  }

  // NOTE: this floor used to `process.exit(1)` here, which made everything after it unreachable —
  // including the name-kill scan and its own `MIN_HARNESS_FILES` floor (round 129). It is recorded
  // as a problem instead and the run continues to the end.
  if (checked < MIN_TOKENS) {
    problems.push(`only ${checked} token(s) examined (< ${MIN_TOKENS}); the extraction regexes are probably broken`);
  }

  // A suite must never clean up by PROCESS NAME. `pkill -x hydra` also matched the local
  // dev stack: the containers of environment/docker-compose.local.yml run a process named
  // exactly `hydra` from /usr/local/bin/hydra, owned by the same uid, so every suite run
  // killed the user's three dev containers and Docker restarted them (measured
  // 2026-09-30: RestartCount=14, unhealthy, exit code 0 = a clean SIGTERM exit). The
  // harnesses now match the EXECUTABLE (/proc/<pid>/exe == BIN, see
  // integration/test_*.py::kill_our_instances) — this rule keeps it that way.
  const HARNESS_DIRS = ["integration", "scripts", ".github/workflows"];
  const harnessFiles = [];
  const walk = (dir) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else harnessFiles.push(full);
    }
  };
  for (const d of HARNESS_DIRS) {
    const full = path.join(ROOT, d);
    if (fs.existsSync(full)) walk(full);
  }
  //
  // Only three EXECUTABLE shapes count, so that prose (a docstring or comment explaining
  // the old command — e.g. the ones this very fix added) is not reported as a violation.
  // See `nameKillLines` for the shapes; `pkill -f "$BIN"` (scripts/handover.test.sh)
  // matches none of them because it names a path, not a process name.
  let harnessScanned = 0;
  for (const f of harnessFiles) {
    // `*.test.cjs` = the guard's own tests: they must contain the LITERAL bad command as a
    // fixture, so scanning them would report the fixture as a violation (and did, in the
    // first run of this rule: the two "runs the checker over the real tree" cases went red).
    if (f.endsWith(".test.cjs")) continue;
    harnessScanned += 1;
    const src = fs.readFileSync(f, "utf8");
    for (const n of nameKillLines(src)) {
      const line = src.split("\n")[n - 1] ?? "";
      problems.push(
        `${path.relative(ROOT, f)}:${n}: kills a process by NAME (\`${line.trim()}\`) — it matches unrelated deployments that happen to use the same binary name; match the executable instead (see integration/test_trusted_proxies.py::kill_our_instances)`,
      );
    }
  }
  // The walk had NO count at all: renaming `integration/` (or moving the harnesses) would have
  // turned the pkill rule into a no-op that still printed OK. Same class as the floors above.
  if (harnessScanned < MIN_HARNESS_FILES) {
    problems.push(
      `only ${harnessScanned} harness file(s) scanned for name-kill commands (< floor ${MIN_HARNESS_FILES}); ` +
        `the harness directories may have moved, so this rule proves nothing`,
    );
  }
  if (problems.length) {
    for (const p of problems) console.error("DRIFT  " + p);
    console.error(`${problems.length} e2e contract drift issue(s)`);
    process.exit(1);
  }

  const specs = specFiles.map((f) => path.relative(ROOT, f)).join(", ");
  // Round 196: the UI-source audit above (every `admin-ui/` source is either in `UI_FILES` or
  // RECORDED in `UNSCANNED_UI_OK`) was invisible on the success path — its findings only printed on
  // failure, so a reader could not tell how much of the list was leaning on recorded exceptions.
  console.log(`OK  (${checked} selector/nav tokens across ${specFiles.length} spec file(s): ${specs}; ${apiSites} api call(s) seen, ${payloadsRead} payload(s) verified; ${harnessScanned} harness file(s) scanned for name-kill commands; ${UI_FILES.length} UI source(s) judged against the list, ${UNSCANNED_UI_OK.size} recorded as deliberately unscanned (${[...UNSCANNED_UI_OK.keys()].join(', ') || 'none'}))`);
}

if (require.main === module) main();

module.exports = { selectorsIn, navKeysIn, nameKillLines };
