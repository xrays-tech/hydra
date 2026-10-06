/**
 * Tests for the read-modify-write merge used by every `update` subcommand.
 *
 * A partial update body cannot work against this API: the handlers deserialise
 * the request straight into the entity and the SQL replaces every column, so
 * `providers update p --weight 5` used to send `{weight, created_at, updated_at}`
 * and the server answered `400 invalid_json: missing field `id``. Measured against
 * a live instance; every group with an `update` subcommand was affected.
 *
 * These tests pin the two properties that fix depends on:
 *   1. flags win, everything else is carried over from the current record;
 *   2. values the server MASKS on read (`provider-keys.api_key`) are never merged
 *      back — writing the mask over the key would destroy it, which is worse than
 *      the bug being fixed.
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { ENTITY_DEFS } from '../src/types.js';
import { mergeForUpdate, unmergeableMissing } from '../src/commands/entities.js';

const defOf = (route: string) => {
  const d = ENTITY_DEFS.find((e) => e.route === route);
  assert.ok(d, `no entity def for ${route}`);
  return d;
};

describe('mergeForUpdate', () => {
  it('keeps the record and applies the flags on top', () => {
    const current = {
      id: 'p1',
      key: 'k1',
      name: 'Provider One',
      endpoint: 'http://up.example/',
      weight: 3,
      created_at: '2026-01-01 00:00:00',
      updated_at: '2026-01-01 00:00:00',
    };
    const merged = mergeForUpdate(defOf('providers'), current, { weight: 5, updated_at: '' });
    assert.equal(merged.weight, 5);
    assert.equal(merged.id, 'p1');
    assert.equal(merged.key, 'k1');
    assert.equal(merged.name, 'Provider One');
    assert.equal(merged.endpoint, 'http://up.example/');
  });

  it('drops response-only decorations', () => {
    const current = {
      id: 't1',
      name: 'T',
      domain: 't.example',
      auth_url: 'https://auth.example/v',
      enabled: true,
      has_access_token: true,
      snapshot_stale: true,
      created_at: '',
      updated_at: '',
    };
    const merged = mergeForUpdate(defOf('tenants'), current, { name: 'T2' });
    assert.equal(merged.name, 'T2');
    assert.equal(merged.has_access_token, undefined);
    assert.equal(merged.snapshot_stale, undefined);
  });

  it('never merges a MASKED read-back over a stored secret', () => {
    const masked = { id: 'k1', provider_id: 'p1', api_key: 'sk-first10***last4', created_at: '' };
    // Without --api-key there is nothing safe to send …
    const withoutFlag = mergeForUpdate(defOf('provider-keys'), masked, {});
    assert.equal(withoutFlag.api_key, undefined, 'the mask must not be carried over');
    assert.deepEqual(unmergeableMissing(defOf('provider-keys'), withoutFlag), ['--api-key']);
    // … and with it, the operator's value wins.
    const withFlag = mergeForUpdate(defOf('provider-keys'), masked, { api_key: 'sk-brand-new' });
    assert.equal(withFlag.api_key, 'sk-brand-new');
    assert.deepEqual(unmergeableMissing(defOf('provider-keys'), withFlag), []);
  });

  it('treats a non-object read-back as empty instead of throwing', () => {
    const merged = mergeForUpdate(defOf('providers'), null, { weight: 1 });
    assert.deepEqual(merged, { weight: 1 });
  });
});
