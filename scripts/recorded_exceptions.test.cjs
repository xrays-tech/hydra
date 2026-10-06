#!/usr/bin/env node
'use strict';
/**
 * Tests for `scripts/recorded_exceptions.cjs` — the shared algebra behind five guards' recorded
 * exceptions (`PROSE_ONLY_OK`, `READ_BY_DEPENDENCY`, `UNVERIFIED_OK` ×2, `UNJUDGED_WITH_EXIT_OK`,
 * `NOT_A_METRIC`). The three invariants in its header are each pinned here, because each of them was
 * learned by a guard turning red for the wrong reason.
 *
 * Run: node --test scripts/recorded_exceptions.test.cjs
 */
const test = require('node:test');
const assert = require('node:assert/strict');
const { records, audit } = require('./recorded_exceptions.cjs');

test('the built-in list is used when no override is given', () => {
  const m = records(undefined, [['A', 'because A']]);
  assert.equal(m.get('A'), 'because A');
  assert.equal(m.size, 1);
});

test('an override REPLACES the built-in list (it never merges)', () => {
  const m = records('{"B":"because B"}', [['A', 'because A']]);
  assert.deepEqual([...m.keys()], ['B'], 'A must not survive the override');
});

test('an override that is not an object of reasons is refused loudly', () => {
  assert.throws(() => records('["A"]', new Map()), /must be a JSON object/);
  assert.throws(() => records('{"A":""}', new Map()), /needs a non-empty reason/);
  assert.throws(() => records('null', new Map()), /must be a JSON object/);
});

test('an unrecorded needed item is reported', () => {
  const { unrecorded, stale, used } = audit({
    records: new Map([['A', 'r']]),
    needed: new Set(['A', 'B']),
    applies: () => true,
  });
  assert.deepEqual(unrecorded, ['B']);
  assert.deepEqual(stale, []);
  assert.deepEqual(used, ['A']);
});

test('a record that no longer applies is reported as stale', () => {
  const { unrecorded, stale } = audit({
    records: new Map([['GONE', 'r']]),
    needed: new Set(),
    applies: (n) => n !== 'GONE',
  });
  assert.deepEqual(unrecorded, []);
  assert.deepEqual(stale, ['GONE']);
});

test('CONTROL: a record that still applies is neither unrecorded nor stale', () => {
  const { unrecorded, stale, used } = audit({
    records: new Map([['A', 'r']]),
    needed: new Set(['A']),
    applies: () => true,
  });
  assert.deepEqual([unrecorded, stale, used], [[], [], ['A']]);
});

test('CONTROL: without `applies`, a record is stale exactly when it is not needed', () => {
  const { stale } = audit({ records: new Map([['A', 'r'], ['B', 'r']]), needed: new Set(['A']) });
  assert.deepEqual(stale, ['B']);
});

test('an audit without a Map of records is refused (never a silent pass)', () => {
  assert.throws(() => audit({ records: [], needed: [] }), /needs a Map of records/);
});
