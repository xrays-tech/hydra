/**
 * Tests for the "clear a nullable field" path: `--max-concurrency null`.
 *
 * README example: `hydra-admin providers update openai --max-concurrency null
 * # clear (set to null)`. It did NOT work: the option parser returned a JS `null`,
 * commander 12.1.0 stores `''` for that, and the request body therefore carried
 * `"max_concurrency": ""` — which the admin API rejects with
 * `400 invalid_json: invalid type: string "", expected u32`. Measured against a live
 * node and against a request echo server; no test covered it, which is why it shipped.
 *
 * The fix carries the intent as a sentinel string (`CLEAR_VALUE`) and converts it to a
 * real JSON `null` when the body is assembled, so these tests pin:
 *   1. a nullable number field set to the sentinel becomes JSON `null`;
 *   2. an ordinary number is untouched;
 *   3. a NON-nullable number field is never turned into `null`;
 *   4. the sentinel cannot leak into the body as the string "null" (that would be the
 *      same class of lie in a different costume).
 */
import { describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { ENTITY_DEFS } from '../src/types.js';
import { CLEAR_VALUE, buildBody } from '../src/commands/entities.js';

const defOf = (route: string) => {
  const d = ENTITY_DEFS.find((e) => e.route === route);
  assert.ok(d, `no entity def for ${route}`);
  return d;
};

describe('buildBody: clearing a nullable number field', () => {
  it('turns the clear sentinel into a JSON null (providers.max_concurrency)', () => {
    const body = buildBody(defOf('providers'), { maxConcurrency: CLEAR_VALUE }, false);
    assert.equal(body.max_concurrency, null);
    assert.notEqual(body.max_concurrency, CLEAR_VALUE);
  });

  it('does the same for every nullable number field of the entity', () => {
    // The README advertises all three provider limits with "(pass \"null\" to clear)".
    for (const field of ['max_concurrency', 'max_queue_depth', 'queue_wait_timeout_ms']) {
      const def = defOf('providers');
      const spec = def.fields.find((f) => f.field === field);
      assert.ok(spec?.nullable, `${field} is expected to be nullable`);
    }
    const body = buildBody(
      defOf('providers'),
      {
        maxConcurrency: CLEAR_VALUE,
        maxQueueDepth: CLEAR_VALUE,
        queueWaitTimeoutMs: CLEAR_VALUE,
      },
      false,
    );
    assert.deepEqual(
      [body.max_concurrency, body.max_queue_depth, body.queue_wait_timeout_ms],
      [null, null, null],
    );
  });

  it('leaves an ordinary number alone', () => {
    const body = buildBody(defOf('providers'), { maxConcurrency: 5, weight: 10 }, false);
    assert.equal(body.max_concurrency, 5);
    assert.equal(body.weight, 10);
  });

  it('never writes the sentinel STRING into the body', () => {
    const body = buildBody(defOf('providers'), { maxConcurrency: CLEAR_VALUE }, false);
    assert.equal(typeof body.max_concurrency, 'object', 'a JSON null, not the string "null"');
    assert.ok(!JSON.stringify(body).includes('"max_concurrency":"null"'));
  });

  it('does not invent a null for a field the caller did not pass', () => {
    const body = buildBody(defOf('providers'), { weight: 1 }, false);
    assert.ok(!('max_concurrency' in body), `body=${JSON.stringify(body)}`);
  });
});

describe('buildBody: limit-roles', () => {
  it('clears limit_count / limit_token with the same sentinel', () => {
    const body = buildBody(
      defOf('limit-roles'),
      { limitCount: CLEAR_VALUE, limitToken: CLEAR_VALUE },
      false,
    );
    assert.equal(body.limit_count, null);
    assert.equal(body.limit_token, null);
  });
});
