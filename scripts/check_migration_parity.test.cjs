#!/usr/bin/env node
"use strict";
/**
 * Tests for scripts/check_migration_parity.cjs.
 *
 * The guard exists because each backend embeds its OWN `sqlx::migrate!` and
 * only one backend is compiled per build, so a one-sided migration is invisible
 * to every suite. The cases below falsify both directions: a fixture that
 * matches must PASS (otherwise the guard could be failing everything), and a
 * one-sided file, a reused version and an empty/missing directory must all be
 * reported rather than silently accepted.
 */

const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { spawnSync } = require("node:child_process");

const CHECKER = path.join(__dirname, "check_migration_parity.cjs");
const REPO = path.resolve(__dirname, "..");

function run(root) {
  const args = root ? [CHECKER, `--root=${root}`] : [CHECKER];
  const r = spawnSync(process.execPath, args, { encoding: "utf8" });
  return { status: r.status, out: `${r.stdout || ""}${r.stderr || ""}` };
}

function fixture(postgres, mysql) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "migration-parity-"));
  for (const [backend, files] of [
    ["postgres", postgres],
    ["mysql", mysql],
  ]) {
    const dir = path.join(root, "crates", "hydra-server", "migrations", backend);
    fs.mkdirSync(dir, { recursive: true });
    for (const [name, body] of Object.entries(files)) {
      fs.writeFileSync(path.join(dir, name), body ?? "-- fixture\n");
    }
  }
  return root;
}

test("the real migration directories pass", () => {
  const r = run();
  assert.strictEqual(r.status, 0, r.out);
  assert.match(r.out, /\[migration-parity\] OK/);
});

test("identical filenames pass", () => {
  const root = fixture(
    { "0001_init.sql": "-- pg\n", "0002_add_table.sql": "-- pg\n" },
    { "0001_init.sql": "-- my\n", "0002_add_table.sql": "-- my\n" },
  );
  const r = run(root);
  assert.strictEqual(r.status, 0, r.out);
  assert.match(r.out, /share 2 filename/);
});

test("a migration added to only one backend fails and is named", () => {
  const root = fixture(
    { "0001_init.sql": "-- pg\n", "0002_pg_only.sql": "-- pg\n" },
    { "0001_init.sql": "-- my\n" },
  );
  const r = run(root);
  assert.strictEqual(r.status, 1, r.out);
  assert.match(r.out, /only in postgres: 0002_pg_only\.sql/);
  assert.match(r.out, /BOTH directories/);
});

test("the opposite one-sided migration is named too", () => {
  const root = fixture(
    { "0001_init.sql": "-- pg\n" },
    { "0001_init.sql": "-- my\n", "0002_mysql_only.sql": "-- my\n" },
  );
  const r = run(root);
  assert.strictEqual(r.status, 1, r.out);
  assert.match(r.out, /only in mysql: 0002_mysql_only\.sql/);
});

test("a reused version prefix fails", () => {
  const root = fixture(
    { "0001_init.sql": "-- pg\n", "0001_again.sql": "-- pg\n" },
    { "0001_init.sql": "-- my\n", "0001_again.sql": "-- my\n" },
  );
  const r = run(root);
  assert.strictEqual(r.status, 1, r.out);
  assert.match(r.out, /reuses version prefix\(es\) 0001/);
});

test("an empty backend directory is cannot-verify, not a pass", () => {
  const root = fixture({ "0001_init.sql": "-- pg\n" }, {});
  const r = run(root);
  assert.strictEqual(r.status, 2, r.out);
  assert.match(r.out, /CANNOT VERIFY/);
  assert.match(r.out, /no \.sql migrations/);
});

test("a missing migration directory is cannot-verify", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "migration-parity-"));
  fs.mkdirSync(path.join(root, "crates", "hydra-server", "migrations"), {
    recursive: true,
  });
  const r = run(root);
  assert.strictEqual(r.status, 2, r.out);
  assert.match(r.out, /CANNOT VERIFY/);
});

test("non-SQL files are ignored by the comparison", () => {
  const root = fixture(
    { "0001_init.sql": "-- pg\n", "README": "notes\n" },
    { "0001_init.sql": "-- my\n", "README": "notes\n" },
  );
  const r = run(root);
  assert.strictEqual(r.status, 0, r.out);
});
