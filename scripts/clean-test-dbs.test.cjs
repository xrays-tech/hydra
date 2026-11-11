#!/usr/bin/env node
"use strict";
/**
 * Tests for scripts/clean-test-dbs.cjs.
 *
 * These run WITHOUT a live database: argument/URL parsing, name matching, output
 * formatting and error paths are exercised as pure functions, and the full flow
 * is driven through an injected fake `spawn` that records the exact CLI calls.
 * The live path (real `psql`/`mysql`) is covered separately by running the script
 * against HYDRA_TEST_DATABASE_URL; a leak the guard failed to name would be a
 * silent no-op, so the cases below pin the selection, the drop SQL and the
 * "CLI missing is a hard error" contract.
 */

const test = require("node:test");
const assert = require("node:assert/strict");

const mod = require("./clean-test-dbs.cjs");
const {
  parseDatabaseUrl,
  parseArgs,
  isTestDbName,
  formatDryRun,
  formatDropped,
  main,
} = mod;

// A fake `child_process.spawnSync` that answers `--version`, a list command and
// a drop command, and records every invocation.
function fakeSpawn({ listOutput = "", failVersion = false, failList = false } = {}) {
  const calls = [];
  const spawn = (file, args, options) => {
    calls.push({ file, args: [...args], options });
    if (args.includes("--version")) {
      return failVersion
        ? { status: 1, stdout: "", stderr: "not found" }
        : { status: 0, stdout: "1.0", stderr: "" };
    }
    const isList = args.includes("--command")
      ? args[args.indexOf("--command") + 1].includes("pg_database")
      : args.includes("--execute") && args[args.indexOf("--execute") + 1] === "SHOW DATABASES";
    if (isList) {
      return failList
        ? { status: 1, stdout: "", stderr: "connection refused" }
        : { status: 0, stdout: listOutput, stderr: "" };
    }
    return { status: 0, stdout: "", stderr: "" };
  };
  spawn.calls = calls;
  return spawn;
}

function capture() {
  const lines = [];
  const errors = [];
  return {
    log: (line) => lines.push(line),
    error: (line) => errors.push(line),
    lines,
    errors,
  };
}

// ---------------------------------------------------------------------------
// URL parsing

test("parses a postgres URL with credentials and port", () => {
  const p = parseDatabaseUrl("postgres://hydra:secret@127.0.0.1:55432/hydra");
  assert.equal(p.kind, "postgres");
  assert.equal(p.host, "127.0.0.1");
  assert.equal(p.port, "55432");
  assert.equal(p.user, "hydra");
  assert.equal(p.password, "secret");
  assert.equal(p.database, "hydra");
});

test("accepts the postgresql alias and defaults the port", () => {
  const p = parseDatabaseUrl("postgresql://u:p@db.example/hydra");
  assert.equal(p.kind, "postgres");
  assert.equal(p.port, "5432");
});

test("parses a mysql URL and keeps the ssl-mode query", () => {
  const p = parseDatabaseUrl("mysql://u:p@127.0.0.1:53306/hydra?ssl-mode=disabled");
  assert.equal(p.kind, "mysql");
  assert.equal(p.port, "53306");
  assert.equal(p.query.get("ssl-mode"), "disabled");
});

test("rejects an unsupported scheme", () => {
  const p = parseDatabaseUrl("sqlite:///tmp/x.db");
  assert.match(p.error, /Unsupported/);
});

test("rejects a malformed URL and a URL with no database", () => {
  assert.match(parseDatabaseUrl("not a url").error, /not a valid URL/);
  assert.match(parseDatabaseUrl("postgres://u:p@h:5432/").error, /must name an existing/);
  assert.match(parseDatabaseUrl(undefined).error, /is not set/);
});

// ---------------------------------------------------------------------------
// Argument parsing and name matching

test("parses --dry-run and --help", () => {
  assert.deepEqual(parseArgs(["--dry-run"]), { dryRun: true, help: false, errors: [] });
  assert.equal(parseArgs(["-h"]).help, true);
  assert.equal(parseArgs(["--help"]).help, true);
});

test("unknown arguments are reported, not ignored", () => {
  const opts = parseArgs(["--dry-run", "--nuke"]);
  assert.equal(opts.dryRun, true);
  assert.deepEqual(opts.errors, ["Unknown argument: --nuke"]);
});

test("only the harness prefixes match", () => {
  assert.ok(isTestDbName("hydra_test_123_0_abc"));
  assert.ok(isTestDbName("hydra_tmpl_123"));
  assert.ok(!isTestDbName("hydra"));
  assert.ok(!isTestDbName("hydra_test")); // no trailing underscore
  assert.ok(!isTestDbName("mydb"));
  assert.ok(!isTestDbName(""));
});

// ---------------------------------------------------------------------------
// Formatting

test("dry-run output names every database and says nothing was dropped", () => {
  const out = formatDryRun(["hydra_test_1_0_a", "hydra_tmpl_1"]);
  assert.match(out, /dry-run: 2 matching database\(s\)/);
  assert.match(out, /nothing dropped/);
  assert.match(out, /^  hydra_test_1_0_a$/m);
  assert.match(out, /^  hydra_tmpl_1$/m);
});

test("dropped output names every database", () => {
  const out = formatDropped(["hydra_test_1_0_a"]);
  assert.match(out, /dropped 1 database\(s\)/);
  assert.match(out, /^  hydra_test_1_0_a$/m);
});

// ---------------------------------------------------------------------------
// End-to-end through main() with a fake spawn

test("dry-run lists matches without issuing any DROP", () => {
  const spawn = fakeSpawn({
    listOutput:
      "hydra\nhydra_test_1_0_aaa\nkeepme\nhydra_tmpl_1\nhydra_test_2_0_bbb\n",
  });
  const cap = capture();
  const code = main(["--dry-run"], {
    env: { HYDRA_TEST_DATABASE_URL: "postgres://hydra:hydra@127.0.0.1:55432/hydra" },
    spawn,
    log: cap.log,
    error: cap.error,
  });

  assert.equal(code, 0);
  const out = cap.lines.join("\n");
  assert.match(out, /dry-run: 3 matching database\(s\)/);
  assert.match(out, /hydra_test_1_0_aaa/);
  assert.match(out, /hydra_test_2_0_bbb/);
  assert.match(out, /hydra_tmpl_1/);
  assert.ok(!out.includes("keepme"), "non-matching database must be ignored");
  assert.ok(!out.includes("\n  hydra\n"), "the maintenance database must be ignored");

  const drops = spawn.calls.filter(
    (c) => !c.args.includes("--version") && (c.args.join(" ").includes("DROP DATABASE")),
  );
  assert.equal(drops.length, 0, "dry-run must not issue DROP");
});

test("real run terminates sessions and FORCE-drops postgres databases", () => {
  const spawn = fakeSpawn({ listOutput: "hydra_test_1_0_aaa\n" });
  const cap = capture();
  const code = main([], {
    env: { HYDRA_TEST_DATABASE_URL: "postgres://hydra:hydra@127.0.0.1:55432/hydra" },
    spawn,
    log: cap.log,
    error: cap.error,
  });

  assert.equal(code, 0);
  assert.match(cap.lines.join("\n"), /dropped 1 database\(s\)/);
  const dropCall = spawn.calls.find((c) => c.args.join(" ").includes("DROP DATABASE"));
  assert.ok(dropCall, "a DROP was expected");
  const joined = dropCall.args.join(" ");
  assert.match(joined, /pg_terminate_backend/);
  assert.match(joined, /DROP DATABASE IF EXISTS "hydra_test_1_0_aaa" WITH \(FORCE\)/);
  assert.equal(dropCall.options.env.PGPASSWORD, "hydra");
});

test("real run drops mysql databases with backticks and no FORCE", () => {
  const spawn = fakeSpawn({ listOutput: "hydra_test_1_0_aaa\nhydra_tmpl_x\nother\n" });
  const cap = capture();
  const code = main([], {
    env: { HYDRA_TEST_DATABASE_URL: "mysql://u:p@127.0.0.1:53306/hydra" },
    spawn,
    log: cap.log,
    error: cap.error,
  });

  assert.equal(code, 0);
  const drops = spawn.calls.filter((c) => c.args.join(" ").includes("DROP DATABASE"));
  assert.equal(drops.length, 2);
  for (const d of drops) {
    assert.match(d.args.join(" "), /DROP DATABASE IF EXISTS `hydra_(test|tmpl)_[^`]+`/);
    assert.ok(!d.args.join(" ").includes("FORCE"));
  }
  assert.equal(drops[0].options.env.MYSQL_PWD, "p");
});

test("a missing CLI is a hard error with setup instructions (no silent skip)", () => {
  const cap = capture();
  const code = main([], {
    env: { HYDRA_TEST_DATABASE_URL: "postgres://hydra:hydra@127.0.0.1:55432/hydra" },
    spawn: fakeSpawn({ failVersion: true }),
    log: cap.log,
    error: cap.error,
  });
  assert.equal(code, 2);
  const err = cap.errors.join("\n");
  assert.match(err, /psql/);
  assert.match(err, /dnf install postgresql/);
});

test("a missing URL is a usage error", () => {
  const cap = capture();
  const code = main([], { env: {}, spawn: fakeSpawn(), log: cap.log, error: cap.error });
  assert.equal(code, 2);
  assert.match(cap.errors.join("\n"), /HYDRA_TEST_DATABASE_URL is not set/);
});

test("a list failure exits 1 and reports the CLI message", () => {
  const cap = capture();
  const code = main([], {
    env: { HYDRA_TEST_DATABASE_URL: "postgres://hydra:hydra@127.0.0.1:55432/hydra" },
    spawn: fakeSpawn({ failList: true }),
    log: cap.log,
    error: cap.error,
  });
  assert.equal(code, 1);
  assert.match(cap.errors.join("\n"), /connection refused/);
});

test("unknown arguments exit 2 with usage", () => {
  const cap = capture();
  const code = main(["--bogus"], {
    env: { HYDRA_TEST_DATABASE_URL: "postgres://hydra:hydra@127.0.0.1:55432/hydra" },
    spawn: fakeSpawn(),
    log: cap.log,
    error: cap.error,
  });
  assert.equal(code, 2);
  assert.match(cap.errors.join("\n"), /Unknown argument: --bogus/);
});

test("--help prints usage and exits 0", () => {
  const cap = capture();
  const code = main(["--help"], { env: {}, spawn: fakeSpawn(), log: cap.log, error: cap.error });
  assert.equal(code, 0);
  assert.match(cap.lines.join("\n"), /Usage: node scripts\/clean-test-dbs\.cjs/);
});
