/**
 * Tests for subcommand DISPATCH — the class of bug a unit test of the client can
 * never see.
 *
 * `tenants auth-test` was unreachable: `new Command('auth-test <auth-url>')` registers a
 * subcommand literally NAMED `auth-test <auth-url>` (the constructor does not parse an
 * argument spec; only `.command('get <id>')` does). The group's default subcommand
 * (`list`, `isDefault: true`) therefore swallowed the tokens, and the documented
 * `hydra-admin tenants auth-test <url>` printed a TENANT LIST and exited 0 — a silent
 * wrong answer. `--tenant-id` failed with "unknown option" for the same reason.
 *
 * These tests pin the two properties the fix rests on:
 *   1. the subcommand is named `auth-test` and declares exactly one argument;
 *   2. parsing `auth-test <url>` really reaches `/api/v1/tenants/auth/test` and does
 *      NOT fall through to the default `list` route.
 */
import { describe, it, before, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { ENTITY_DEFS } from '../src/types.js';
import { buildEntityCommand } from '../src/commands/entities.js';
import { buildTenantAuthTestCommand } from '../src/commands/system.js';

let server: http.Server;
let base = '';
let paths: string[] = [];

function json(res: http.ServerResponse, payload: unknown): void {
  const body = JSON.stringify(payload);
  res.writeHead(200, { 'Content-Type': 'application/json' });
  res.end(body);
}

before(async () => {
  server = http.createServer((req, res) => {
    paths.push(req.url ?? '');
    if (req.url === '/api/v1/tenants/auth/test') {
      json(res, { ok: true, verdict: 'allowed', reachable: true, status: 200, duration_ms: 1 });
      return;
    }
    json(res, []);
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}`;
});

after(() => {
  server.close();
});

const defOf = (route: string) => {
  const d = ENTITY_DEFS.find((e) => e.route === route);
  assert.ok(d, `no entity def for ${route}`);
  return d;
};

describe('tenants auth-test dispatch', () => {
  it('is named `auth-test` and declares exactly one argument', () => {
    const cmd = buildTenantAuthTestCommand();
    assert.equal(cmd.name(), 'auth-test', 'the name must not absorb the argument spec');
    assert.equal(cmd.registeredArguments.length, 1);
    assert.equal(cmd.registeredArguments[0]?.name(), 'auth-url');
  });

  it('reaches /tenants/auth/test instead of the default `list` route', async () => {
    const group = buildEntityCommand(defOf('tenants'));
    group.addCommand(buildTenantAuthTestCommand());
    group.exitOverride();
    paths = [];
    // `{ from: 'user' }`: without it commander slices off the first two argv entries
    // (it assumes `node script`), so `parseAsync([...])` silently dropped 'auth-test'
    // and the whole argument list shifted — which is how this test first "failed" with
    // commander's `unknown option '--tenant-id'` while the CLI itself was fine.
    await group.parseAsync(
      [
        'auth-test',
        `${base}/auth`,
        '--tenant-id',
        't1',
        '--token',
        'test-token',
        '--base-url',
        base,
        '--json',
      ],
      { from: 'user' },
    );
    assert.deepEqual(paths, ['/api/v1/tenants/auth/test'], `paths=${JSON.stringify(paths)}`);
  });

  it('the group still defaults to `list` when no subcommand is given', async () => {
    const group = buildEntityCommand(defOf('tenants'));
    group.exitOverride();
    paths = [];
    await group.parseAsync(['--token', 'test-token', '--base-url', base, '--json'], {
      from: 'user',
    });
    assert.deepEqual(paths, ['/api/v1/tenants'], `paths=${JSON.stringify(paths)}`);
  });
});
