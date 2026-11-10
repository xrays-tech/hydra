#!/usr/bin/env node
/* Guard: every production SQL call in `db.rs` must pass through
 * `dialect::sql(...)`. A raw `sqlx::query("... ? ...")` compiles and runs on
 * MySQL (which accepts `?`) and only fails on PostgreSQL at runtime, so this
 * cannot be left to review.
 *
 * The one deliberate escape hatch is MySQL `delete_tenant`, which executes a
 * text-protocol `raw_sql(&sql)` with a hex literal to avoid ER_NEED_REPREPARE
 * (1615). It is counted and named, not silently skipped.
 *
 * Run: node scripts/check_dialect_sql.cjs [--file=PATH]
 * Exit: 0 clean · 1 finding · 2 cannot verify.
 */
"use strict";

const fs = require("fs");
const path = require("path");

const ROOT = path.join(__dirname, "..");
const DEFAULT_FILE = path.join(ROOT, "crates", "hydra-server", "src", "db.rs");

/** The production half of the file (the `#[cfg(test)] mod tests` tail is not production). */
function productionSource(src) {
  const m = src.search(/\n#\[cfg\(test\)\]\s*\nmod tests\b/);
  return m >= 0 ? src.slice(0, m) : src;
}

function check(file) {
  let src;
  try {
    src = fs.readFileSync(file, "utf8");
  } catch (e) {
    console.error(`[dialect-sql] CANNOT VERIFY: cannot read ${file}: ${e.message}`);
    process.exit(2);
  }

  const lines = productionSource(src).split(/\r?\n/);
  const problems = [];
  let queries = 0;

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (/^\s*\/\//.test(line)) continue; // comments are not calls
    if (!/sqlx::query(?:_as)?\s*(?:::<[^>]*>)?\s*\(/.test(line)) continue;

    queries += 1;
    // The argument may be on the same line or the next one (rustfmt wraps a
    // long multi-line SQL literal), so inspect a small window.
    const window = lines.slice(i, i + 3).join("\n");
    if (!window.includes("dialect::sql(")) {
      problems.push(
        `${path.relative(ROOT, file)}:${i + 1}: SQL does not pass through dialect::sql(): ${line.trim()}`,
      );
    }
  }

  const raw = [];
  for (let i = 0; i < lines.length; i += 1) {
    const count = (lines[i].match(/sqlx::raw_sql/g) || []).length;
    if (count > 0) raw.push({ line: lines[i], index: i + 1, count });
  }
  const rawCount = raw.reduce((sum, entry) => sum + entry.count, 0);
  if (rawCount > 1) {
    problems.push(
      `${path.relative(ROOT, file)}: ${rawCount} sqlx::raw_sql call(s); only delete_tenant's MySQL escape hatch is allowed`,
    );
  } else if (rawCount === 1 && !raw[0].line.includes("raw_sql(&sql)")) {
    problems.push(
      `${path.relative(ROOT, file)}:${raw[0].index}: raw_sql is not the documented delete_tenant escape hatch`,
    );
  }

  return { queries, raw: rawCount, problems };
}

function parseArgs(argv) {
  const opts = { file: DEFAULT_FILE };
  for (const arg of argv) {
    if (arg === "--help" || arg === "-h") {
      console.log("usage: node scripts/check_dialect_sql.cjs [--file=PATH]");
      process.exit(0);
    }
    if (arg.startsWith("--file=")) opts.file = arg.slice("--file=".length);
    else {
      console.error(`[dialect-sql] unknown argument: ${arg}`);
      process.exit(2);
    }
  }
  return opts;
}

const { file } = parseArgs(process.argv.slice(2));
const { queries, raw, problems } = check(file);

if (problems.length > 0) {
  console.error("[dialect-sql] FAIL");
  for (const p of problems) console.error(`  ${p}`);
  console.error(
    "  Fix: wrap the SQL in `dialect::sql(...)` so `?` becomes `$n` on PostgreSQL. " +
      "Add a named allowlist entry only for the documented delete_tenant raw_sql path.",
  );
  process.exit(1);
}

console.log(
  `[dialect-sql] OK (${queries} query call(s) through dialect::sql; ${raw} allowlisted raw_sql)`,
);
