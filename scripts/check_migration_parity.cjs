#!/usr/bin/env node
/* Migration parity: `migrations/postgres` and `migrations/mysql` must describe
 * the SAME schema history.
 *
 * WHY: `crates/hydra-server/src/db/pool.rs` embeds one `sqlx::migrate!` per
 * backend, and every build selects exactly one. A migration added to only one
 * directory therefore compiles and tests green for that backend while the other
 * silently stays at an older schema — the drift only surfaces when someone runs
 * the "other" backend, often in production. Nothing else compares the two
 * directories, so this guard does.
 *
 * RULES:
 *   1. both directories exist and contain at least one migration (a missing or
 *      empty directory is CANNOT VERIFY, not a pass);
 *   2. the set of migration FILENAMES is identical;
 *   3. the set of VERSION PREFIXES (the part before the first `_`) is identical;
 *   4. a version prefix is not reused inside one backend (sqlx would reject or
 *      silently order it ambiguously).
 *
 * The migration CONTENTS are intentionally not compared here — the greenfield
 * SQL differs by dialect. This guard only pins that the two histories move
 * together.
 *
 * Usage: node scripts/check_migration_parity.cjs [--root=<repo>]
 * Exit:  0 = parity holds, 1 = drift, 2 = cannot verify (IO/layout problem).
 */
"use strict";

const fs = require("fs");
const path = require("path");

const rootArg = process.argv.find((a) => a.startsWith("--root="));
const ROOT = rootArg
  ? path.resolve(rootArg.slice("--root=".length))
  : path.resolve(__dirname, "..");
const MIGRATIONS = path.join(ROOT, "crates", "hydra-server", "migrations");
const BACKENDS = ["postgres", "mysql"];

function fail(code, message) {
  console.error(message);
  process.exit(code);
}

function listMigrations(backend) {
  const dir = path.join(MIGRATIONS, backend);
  let entries;
  try {
    entries = fs.readdirSync(dir, { withFileTypes: true });
  } catch (e) {
    fail(
      2,
      `[migration-parity] CANNOT VERIFY: cannot read ${path.relative(ROOT, dir)} (${e.code || e.message})`,
    );
  }
  const files = entries
    .filter((e) => e.isFile() && e.name.endsWith(".sql"))
    .map((e) => e.name)
    .sort();
  if (files.length === 0) {
    fail(
      2,
      `[migration-parity] CANNOT VERIFY: ${path.relative(ROOT, dir)} has no .sql migrations`,
    );
  }
  return files;
}

const versionPrefix = (name) => {
  const i = name.indexOf("_");
  return i === -1 ? name.replace(/\.sql$/, "") : name.slice(0, i);
};

function duplicates(files) {
  const seen = new Map();
  const dupes = new Set();
  for (const f of files) {
    const v = versionPrefix(f);
    if (seen.has(v)) dupes.add(v);
    seen.set(v, f);
  }
  return [...dupes].sort();
}

function diff(a, b) {
  const bs = new Set(b);
  return a.filter((x) => !bs.has(x));
}

function main() {
  const byBackend = {};
  for (const backend of BACKENDS) {
    byBackend[backend] = listMigrations(backend);
    const dupes = duplicates(byBackend[backend]);
    if (dupes.length > 0) {
      fail(
        1,
        `[migration-parity] FAIL: ${backend} reuses version prefix(es) ${dupes.join(", ")}`,
      );
    }
    console.log(
      `[migration-parity] ${backend}: ${byBackend[backend].length} migration(s)`,
    );
  }

  const [first, second] = BACKENDS;
  const onlyFirst = diff(byBackend[first], byBackend[second]);
  const onlySecond = diff(byBackend[second], byBackend[first]);
  if (onlyFirst.length > 0 || onlySecond.length > 0) {
    const lines = [`[migration-parity] FAIL: migration filenames differ`];
    if (onlyFirst.length > 0) {
      lines.push(`  only in ${first}: ${onlyFirst.join(", ")}`);
    }
    if (onlySecond.length > 0) {
      lines.push(`  only in ${second}: ${onlySecond.join(", ")}`);
    }
    lines.push(
      "  Every schema change must land in BOTH directories (same filename/version).",
    );
    fail(1, lines.join("\n"));
  }

  const prefixes = {};
  for (const backend of BACKENDS) {
    prefixes[backend] = [...new Set(byBackend[backend].map(versionPrefix))].sort();
  }
  const onlyPrefixFirst = diff(prefixes[first], prefixes[second]);
  const onlyPrefixSecond = diff(prefixes[second], prefixes[first]);
  if (onlyPrefixFirst.length > 0 || onlyPrefixSecond.length > 0) {
    const lines = [`[migration-parity] FAIL: migration version prefixes differ`];
    if (onlyPrefixFirst.length > 0) {
      lines.push(`  only in ${first}: ${onlyPrefixFirst.join(", ")}`);
    }
    if (onlyPrefixSecond.length > 0) {
      lines.push(`  only in ${second}: ${onlyPrefixSecond.join(", ")}`);
    }
    fail(1, lines.join("\n"));
  }

  console.log(
    `[migration-parity] OK: ${first} and ${second} share ${byBackend[first].length} filename(s)/version(s)`,
  );
}

main();
