#!/usr/bin/env node
"use strict";

const assert = require("assert");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const SCRIPT = path.join(__dirname, "check_dialect_sql.cjs");
const REAL_DB_DIR = path.join(__dirname, "..", "crates", "hydra-server", "src", "db");
const TMP = fs.mkdtempSync(path.join(os.tmpdir(), "hydra-dialect-sql-"));

function run(file) {
  const args = file ? [SCRIPT, `--file=${file}`] : [SCRIPT];
  const r = spawnSync(process.execPath, args, { encoding: "utf8" });
  return { status: r.status, out: `${r.stdout || ""}${r.stderr || ""}` };
}

function fixture(name, body) {
  const file = path.join(TMP, `${name}.rs`);
  fs.writeFileSync(file, body);
  return file;
}

const tests = [];
function test(name, fn) {
  tests.push([name, fn]);
}

test("the real db tree passes", () => {
  assert.ok(fs.existsSync(REAL_DB_DIR), "the split db directory must exist");
  const r = run();
  assert.strictEqual(r.status, 0, r.out);
  assert.match(r.out, /through dialect::sql across \d+ file\(s\)/);
  assert.match(r.out, /1 allowlisted raw_sql/);
});

test("a query through dialect::sql passes", () => {
  const r = run(fixture("good", 'fn q() { let _ = sqlx::query(&dialect::sql("SELECT ?")); }\n'));
  assert.strictEqual(r.status, 0, r.out);
});

test("a raw query fails", () => {
  const r = run(fixture("bad", 'fn q() { let _ = sqlx::query("SELECT ?"); }\n'));
  assert.strictEqual(r.status, 1, r.out);
  assert.match(r.out, /does not pass through dialect::sql/);
});

test("a multi-line wrapped query still passes", () => {
  const r = run(
    fixture(
      "wrapped",
      'fn q() {\n    let _ = sqlx::query(\n        &dialect::sql("SELECT ?"),\n    );\n}\n',
    ),
  );
  assert.strictEqual(r.status, 0, r.out);
});

test("one raw_sql escape hatch passes", () => {
  const r = run(
    fixture(
      "raw-one",
      'fn q() { let sql = "x"; let _ = sqlx::raw_sql(&sql); }\n',
    ),
  );
  assert.strictEqual(r.status, 0, r.out);
  assert.match(r.out, /1 allowlisted raw_sql/);
});

test("two raw_sql escape hatches fail", () => {
  const r = run(
    fixture(
      "raw-two",
      'fn q() { let sql = "x"; let _ = sqlx::raw_sql(&sql); let _ = sqlx::raw_sql(&sql); }\n',
    ),
  );
  assert.strictEqual(r.status, 1, r.out);
  assert.match(r.out, /only delete_tenant/);
});

test("the test-module tail is not judged", () => {
  const r = run(
    fixture(
      "test-tail",
      'fn q() { let _ = sqlx::query(&dialect::sql("SELECT ?")); }\n#[cfg(test)]\nmod tests {\n    fn t() { let _ = sqlx::query("SELECT ?"); }\n}\n',
    ),
  );
  assert.strictEqual(r.status, 0, r.out);
});

let failed = 0;
for (const [name, fn] of tests) {
  try {
    fn();
    console.log(`PASS  ${name}`);
  } catch (e) {
    failed += 1;
    console.error(`FAIL  ${name}  -> ${e.message}`);
  }
}
fs.rmSync(TMP, { recursive: true, force: true });
if (failed > 0) {
  console.error(`\n${failed} DIALECT-SQL TEST(S) FAILED`);
  process.exit(1);
}
console.log("\nALL DIALECT-SQL TESTS PASSED");
