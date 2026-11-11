#!/usr/bin/env node
/* Test-database cleanup: drop the isolated databases left behind by the
 * real-database test harness in `crates/hydra-server/src/db/testing.rs`.
 *
 * WHY: the PostgreSQL/MySQL integration suites intentionally LEAK databases on
 * purpose (CI containers are disposable, and `DROP DATABASE` forces a checkpoint
 * that costs more than provisioning). Locally the same containers persist across
 * runs, so `hydra_test_*` / `hydra_tmpl_*` databases accumulate until the
 * cluster is noisy and disk-bound. This script is the explicit local exit
 * hatch; it never runs as part of the test suite.
 *
 * BEHAVIOR (parity with `db/testing.rs`):
 *   PostgreSQL — list from `pg_database`, then for each database terminate
 *   remaining sessions (`pg_terminate_backend`, e.g. a spawned binary) and
 *   `DROP DATABASE IF EXISTS ... WITH (FORCE)`.
 *   MySQL — list from `information_schema.schemata` (via `SHOW DATABASES`) and
 *   `DROP DATABASE IF EXISTS ...` (MySQL has no FORCE clause; `DROP DATABASE`
 *   already closes sessions).
 *
 * Only the two harness prefixes are touched: `hydra_test_` and `hydra_tmpl_`.
 * The template prefix is PostgreSQL-only in the harness, but matching it on both
 * backends is harmless and keeps one rule.
 *
 * DEPENDENCY-FREE: no npm driver. It shells out to the `psql` / `mysql` CLIs so
 * the exact server-side behavior (and the operator's normal auth setup) is used.
 * If the required CLI is missing it exits non-zero with setup instructions; it
 * NEVER silently skips.
 *
 * Usage:
 *   HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra \
 *     node scripts/clean-test-dbs.cjs [--dry-run]
 * Exit: 0 = success (including "nothing to drop"), 2 = usage/environment/CLI
 *       problem, 1 = a list/drop command failed.
 */
"use strict";

const { spawnSync } = require("node:child_process");

const PREFIXES = ["hydra_test_", "hydra_tmpl_"];
// Harness names are `hydra_test_<pid>_<seq>_<hex>` / `hydra_tmpl_<pid>`; be
// strict about the identifier before interpolating it into a DROP statement.
const SAFE_IDENTIFIER = /^[A-Za-z0-9_]+$/;

const USAGE = `Usage: node scripts/clean-test-dbs.cjs [--dry-run]

Drops databases created by the real-database test harness:
  hydra_test_%  per-test databases
  hydra_tmpl_%  PostgreSQL template databases

Connection comes from HYDRA_TEST_DATABASE_URL (postgres:// or mysql://),
e.g. postgres://hydra:hydra@127.0.0.1:55432/hydra

  --dry-run   list matching databases without dropping them
  -h, --help  show this help`;

const SETUP = {
  postgres: `The PostgreSQL client 'psql' was not found on PATH.
Install it, e.g.:
  Fedora:   sudo dnf install postgresql
  Debian:   sudo apt-get install postgresql-client
  macOS:    brew install libpq && brew link --force libpq
Or run the client from the server image:
  docker exec -i hydra-phase2-pg psql -U hydra -d hydra ...`,
  mysql: `The MySQL client 'mysql' was not found on PATH.
Install it, e.g.:
  Fedora:   sudo dnf install mariadb
  Debian:   sudo apt-get install default-mysql-client
  macOS:    brew install mysql-client
Or run the client from the server image:
  docker exec -i hydra-phase2-mysql mysql -uhydra -phydra ...`,
};

/** Parse `HYDRA_TEST_DATABASE_URL` into the parts the CLIs need. */
function parseDatabaseUrl(raw) {
  if (!raw || typeof raw !== "string") {
    return { error: "HYDRA_TEST_DATABASE_URL is not set." };
  }
  let url;
  try {
    url = new URL(raw);
  } catch {
    return { error: `HYDRA_TEST_DATABASE_URL is not a valid URL: ${raw}` };
  }
  const scheme = url.protocol.replace(/:$/, "").toLowerCase();
  let kind;
  if (scheme === "postgres" || scheme === "postgresql") {
    kind = "postgres";
  } else if (scheme === "mysql") {
    kind = "mysql";
  } else {
    return {
      error: `Unsupported HYDRA_TEST_DATABASE_URL scheme '${scheme}:' (expected postgres:// or mysql://).`,
    };
  }
  const database = decodeURIComponent(url.pathname.replace(/^\//, ""));
  if (!database) {
    return {
      error:
        "HYDRA_TEST_DATABASE_URL must name an existing maintenance database " +
        "(e.g. ...:55432/hydra); the server connection uses it.",
    };
  }
  return {
    kind,
    host: url.hostname || "127.0.0.1",
    port: url.port || (kind === "postgres" ? "5432" : "3306"),
    user: decodeURIComponent(url.username || ""),
    password: decodeURIComponent(url.password || ""),
    database: decodeURIComponent(database),
    query: url.searchParams,
  };
}

/** Parse CLI flags. Unknown flags are an error, not silently ignored. */
function parseArgs(argv) {
  const opts = { dryRun: false, help: false, errors: [] };
  for (const arg of argv) {
    if (arg === "--dry-run") {
      opts.dryRun = true;
    } else if (arg === "-h" || arg === "--help") {
      opts.help = true;
    } else {
      opts.errors.push(`Unknown argument: ${arg}`);
    }
  }
  return opts;
}

/** True for a database name owned by the test harness. */
function isTestDbName(name) {
  return (
    typeof name === "string" &&
    PREFIXES.some((prefix) => name.startsWith(prefix))
  );
}

/** Format the names matching the harness prefixes. */
function formatDryRun(names) {
  const header = `[clean-test-dbs] dry-run: ${names.length} matching database(s) (nothing dropped)`;
  return [header, ...names.map((name) => `  ${name}`)].join("\n");
}

/** Format the names that were dropped. */
function formatDropped(names) {
  const header = `[clean-test-dbs] dropped ${names.length} database(s)`;
  return [header, ...names.map((name) => `  ${name}`)].join("\n");
}

function envFor(kind, parsed, baseEnv) {
  const env = { ...baseEnv };
  if (kind === "postgres") {
    if (parsed.password) env.PGPASSWORD = parsed.password;
    const sslmode = parsed.query.get("sslmode");
    if (sslmode) env.PGSSLMODE = sslmode;
  } else {
    if (parsed.password) env.MYSQL_PWD = parsed.password;
  }
  return env;
}

function cliArgs(parsed) {
  // Omit empty credentials so the server's own auth (peer/socket/.my.cnf) can
  // apply instead of sending `--user ''`.
  const args = [];
  if (parsed.host) args.push("--host", parsed.host);
  if (parsed.port) args.push("--port", parsed.port);
  if (parsed.user) args.push("--username", parsed.user);
  return args;
}

function mysqlArgs(parsed) {
  const args = [];
  if (parsed.host) args.push("--host", parsed.host);
  if (parsed.port) args.push("--port", parsed.port);
  if (parsed.user) args.push("--user", parsed.user);
  args.push("--batch", "--skip-column-names");
  return args;
}

function runCli(spawn, file, args, options) {
  const result = spawn(file, args, options);
  if (!result || result.error) {
    const code = result && result.error && result.error.code;
    const err = new Error(
      `cannot run '${file}': ${code || (result && result.error) || "unknown error"}`,
    );
    err.code = code;
    throw err;
  }
  if (result.status !== 0) {
    const stderr = (result.stderr || "").toString().trim();
    throw new Error(
      `'${file} ${args.join(" ")}' exited ${result.status}${stderr ? `: ${stderr}` : ""}`,
    );
  }
  return (result.stdout || "").toString();
}

function cliAvailable(spawn, file, env) {
  const result = spawn(file, ["--version"], { env });
  return Boolean(result && !result.error && result.status === 0);
}

const PG_LIST_SQL =
  "SELECT datname::text FROM pg_database " +
  "WHERE starts_with(datname::text, 'hydra_test_') " +
  "OR starts_with(datname::text, 'hydra_tmpl_') ORDER BY 1";

function listDatabases(kind, parsed, env, spawn) {
  const args =
    kind === "postgres"
      ? [...cliArgs(parsed), "--no-psqlrc", "--tuples-only", "--no-align",
         "--dbname", parsed.database, "--command", PG_LIST_SQL]
      : [...mysqlArgs(parsed), "--execute", "SHOW DATABASES"];
  const file = kind === "postgres" ? "psql" : "mysql";
  const out = runCli(spawn, file, args, { env, encoding: "utf8" });
  const names = out
    .split(/\r?\n/)
    .map((line) => line.trim())
    .filter((line) => isTestDbName(line));
  return [...new Set(names)].sort();
}

function escapeLiteral(value) {
  return value.replace(/'/g, "''");
}

function dropDatabase(kind, parsed, name, env, spawn) {
  if (!SAFE_IDENTIFIER.test(name)) {
    throw new Error(`refusing to drop unexpected database name: ${JSON.stringify(name)}`);
  }
  if (kind === "postgres") {
    // Parity with db/testing.rs::drop_isolated: terminate leftover sessions
    // (e.g. a spawned binary) first, then FORCE-drop.
    const terminate =
      "SELECT pg_terminate_backend(pid) FROM pg_stat_activity " +
      `WHERE datname = '${escapeLiteral(name)}' AND pid <> pg_backend_pid()`;
    const args = [
      ...cliArgs(parsed),
      "--no-psqlrc",
      "--quiet",
      "--dbname",
      parsed.database,
      "--command",
      terminate,
      "--command",
      `DROP DATABASE IF EXISTS "${name}" WITH (FORCE)`,
    ];
    runCli(spawn, "psql", args, { env, encoding: "utf8" });
  } else {
    const args = [
      ...mysqlArgs(parsed),
      "--execute",
      `DROP DATABASE IF EXISTS \`${name}\``,
    ];
    runCli(spawn, "mysql", args, { env, encoding: "utf8" });
  }
}

/**
 * Entry point. `deps` is injectable so tests can supply a fake `spawn`/`env`
 * without a live server (see scripts/clean-test-dbs.test.cjs).
 */
function main(argv = process.argv.slice(2), deps = {}) {
  const env = deps.env || process.env;
  const spawn = deps.spawn || spawnSync;
  const log = deps.log || ((line) => console.log(line));
  const error = deps.error || ((line) => console.error(line));

  const opts = parseArgs(argv);
  if (opts.help) {
    log(USAGE);
    return 0;
  }
  if (opts.errors.length > 0) {
    error(opts.errors.join("\n"));
    error(USAGE);
    return 2;
  }

  const parsed = parseDatabaseUrl(env.HYDRA_TEST_DATABASE_URL);
  if (parsed.error) {
    error(`[clean-test-dbs] ${parsed.error}`);
    error(USAGE);
    return 2;
  }

  const file = parsed.kind === "postgres" ? "psql" : "mysql";
  if (!cliAvailable(spawn, file, env)) {
    error(`[clean-test-dbs] ${SETUP[parsed.kind]}`);
    return 2;
  }

  const cliEnv = envFor(parsed.kind, parsed, env);
  let names;
  try {
    names = listDatabases(parsed.kind, parsed, cliEnv, spawn);
  } catch (e) {
    error(`[clean-test-dbs] failed to list ${parsed.kind} databases: ${e.message}`);
    return 1;
  }

  if (opts.dryRun) {
    log(formatDryRun(names));
    return 0;
  }

  for (const name of names) {
    try {
      dropDatabase(parsed.kind, parsed, name, cliEnv, spawn);
    } catch (e) {
      error(`[clean-test-dbs] failed to drop ${name}: ${e.message}`);
      return 1;
    }
  }
  log(formatDropped(names));
  return 0;
}

module.exports = {
  PREFIXES,
  USAGE,
  SETUP,
  PG_LIST_SQL,
  parseDatabaseUrl,
  parseArgs,
  isTestDbName,
  formatDryRun,
  formatDropped,
  listDatabases,
  dropDatabase,
  main,
};

if (require.main === module) {
  process.exitCode = main();
}
