import { describe, it, before, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { HydraClient, HydraApiError } from '../src/client.js';
import { resolveConfig, ConfigError } from '../src/config.js';

interface Captured {
  method: string;
  url: string;
  auth: string | undefined;
  accept: string | undefined;
  contentType: string | undefined;
  body: string;
}

let server: http.Server;
let base = '';
let last: Captured = {
  method: '',
  url: '',
  auth: undefined,
  accept: undefined,
  contentType: undefined,
  body: '',
};

function json(res: http.ServerResponse, status: number, payload: unknown): void {
  const body = JSON.stringify(payload);
  res.writeHead(status, {
    'Content-Type': 'application/json',
    'Content-Length': Buffer.byteLength(body),
  });
  res.end(body);
}

before(async () => {
  server = http.createServer((req, res) => {
    const chunks: Buffer[] = [];
    req.on('data', (c: Buffer) => chunks.push(c));
    req.on('end', () => {
      const raw = Buffer.concat(chunks).toString('utf8');
      last = {
        method: req.method ?? '',
        url: req.url ?? '',
        auth: req.headers['authorization'],
        accept: req.headers['accept'],
        contentType: req.headers['content-type'],
        body: raw,
      };

      if (last.url === '/api/v1/health' && last.method === 'GET') {
        return json(res, 200, { status: 'ok', version: '1.0.0' });
      }
      if (last.url === '/api/v1/reload' && last.method === 'POST') {
        return json(res, 200, { reloaded: true });
      }
      if (last.url === '/metrics' && last.method === 'GET') {
        const text = '# HELP hydra_x A metric\n# TYPE hydra_x gauge\nhydra_x 42\n';
        res.writeHead(200, { 'Content-Type': 'text/plain', 'Content-Length': Buffer.byteLength(text) });
        return res.end(text);
      }
      if (last.url === '/api/v1/concurrency' && last.method === 'GET') {
        return json(res, 200, {
          providers: [
            { provider_id: 'p1', gated: true, max_concurrency: 10, inflight: 2, available: 8, queue_depth: 0 },
          ],
        });
      }
      if (last.url === '/api/v1/breaker' && last.method === 'GET') {
        return json(res, 200, { dead: ['p1'] });
      }
      if (last.url === '/api/v1/breaker/p1' && last.method === 'DELETE') {
        return json(res, 200, { reset: 'p1', was_dead: true, dead: [] });
      }
      if (last.url === '/api/v1/auth/cache' && last.method === 'DELETE') {
        return json(res, 200, { invalidated: 2, tenant_id: 't1' });
      }
      if (last.url === '/api/v1/stats/usage' && last.method === 'GET') {
        return json(res, 200, {
          generated_at: '2026-01-01T00:00:00+00:00',
          totals: { requests: 42, tokens: 1000, tokens_prompt: 600, tokens_completion: 400, tenants: 1, providers: 1 },
          by_tenant: [{ name: 't1', requests: 42, tokens: 1000, tokens_prompt: 600, tokens_completion: 400 }],
          by_provider: [{ name: 'p1', requests: 42, tokens: 1000, tokens_prompt: 600, tokens_completion: 400 }],
        });
      }
      if (last.url === '/api/v1/cluster/status' && last.method === 'GET') {
        return json(res, 200, {
          cluster: false,
          mode: 'single',
          node_id: '',
          this_node_leader: false,
          lease_holder: null,
          nodes: [],
        });
      }
      if (last.url === '/api/v1/tenants/auth/test' && last.method === 'POST') {
        return json(res, 200, {
          ok: true,
          reachable: true,
          status: 401,
          protocol_ok: true,
          verdict: 'denied',
          detail: 'auth service rejected the simulated api-key',
          duration_ms: 42,
          body_snippet: '',
        });
      }
      if (last.url === '/api/v1/providers' && last.method === 'GET') {
        return json(res, 200, [
          { id: 'p1', key: 'openai', name: 'OpenAI', endpoint: 'https://api.openai.com', weight: 10 },
        ]);
      }
      if (last.url === '/api/v1/providers' && last.method === 'POST') {
        const sent = JSON.parse(raw || '{}') as Record<string, unknown>;
        return json(res, 201, { ...sent, created_at: '2026-01-01T00:00:00Z', updated_at: '2026-01-01T00:00:00Z' });
      }
      if (last.url === '/api/v1/providers/p1' && last.method === 'PUT') {
        const sent = JSON.parse(raw || '{}') as Record<string, unknown>;
        return json(res, 200, { id: 'p1', ...sent, updated_at: '2026-02-02T00:00:00Z' });
      }
      if (last.url === '/api/v1/providers/p1' && last.method === 'DELETE') {
        return json(res, 200, { deleted: 'p1' });
      }
      return json(res, 404, { error: 'not found' });
    });
  });
  await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
  const addr = server.address() as AddressInfo;
  base = `http://127.0.0.1:${addr.port}`;
});

after(async () => {
  await new Promise<void>((resolve) => server.close(() => resolve()));
});

function client(): HydraClient {
  return new HydraClient({ baseUrl: base, token: 'test-token', json: false, verbose: false });
}

describe('HydraClient transport', () => {
  it('sends GET with Bearer token and parses the array', async () => {
    const res = await client().list('providers');
    assert.ok(Array.isArray(res));
    assert.equal((res as Array<Record<string, unknown>>)[0]?.['id'], 'p1');
    assert.equal(last.method, 'GET');
    assert.equal(last.url, '/api/v1/providers');
    assert.equal(last.auth, 'Bearer test-token');
    assert.equal(last.accept, 'application/json');
  });

  it('builds a POST with JSON body and echoes server timestamps', async () => {
    const res = await client().create('providers', {
      id: 'p1',
      key: 'openai',
      weight: 10,
      created_at: '',
      updated_at: '',
    });
    assert.equal((res as Record<string, unknown>)['id'], 'p1');
    assert.equal((res as Record<string, unknown>)['created_at'], '2026-01-01T00:00:00Z');
    assert.equal(last.method, 'POST');
    assert.equal(last.url, '/api/v1/providers');
    assert.equal(last.contentType, 'application/json');
    const sent = JSON.parse(last.body) as Record<string, unknown>;
    assert.equal(sent['weight'], 10);
    assert.equal(sent['created_at'], '');
    assert.equal(sent['updated_at'], '');
  });

  it('issues PUT to /:id', async () => {
    const res = await client().update('providers', 'p1', { name: 'New', created_at: '', updated_at: '' });
    assert.equal(last.method, 'PUT');
    assert.equal(last.url, '/api/v1/providers/p1');
    assert.equal((res as Record<string, unknown>)['name'], 'New');
  });

  it('issues DELETE to /:id', async () => {
    const res = await client().delete('providers', 'p1');
    assert.equal(last.method, 'DELETE');
    assert.equal(last.url, '/api/v1/providers/p1');
    assert.equal((res as Record<string, unknown>)['deleted'], 'p1');
  });

  it('returns raw Prometheus text for metrics (text/plain accept)', async () => {
    const text = await client().metrics();
    assert.match(text, /# HELP/);
    assert.equal(last.url, '/metrics');
    assert.equal(last.accept, 'text/plain');
  });

  it('parses concurrency as the latest {providers:[...]} payload', async () => {
    const res = await client().concurrency() as Record<string, unknown>;
    assert.ok(Array.isArray(res['providers']));
    assert.equal((res['providers'] as Array<Record<string, unknown>>)[0]?.['provider_id'], 'p1');
  });

  it('lists and resets circuit breakers', async () => {
    const list = await client().breakerList() as Record<string, unknown>;
    assert.deepEqual(list['dead'], ['p1']);
    const reset = await client().breakerReset('p1') as Record<string, unknown>;
    assert.equal(reset['reset'], 'p1');
    assert.equal(last.method, 'DELETE');
    assert.equal(last.url, '/api/v1/breaker/p1');
  });

  it('invalidates the auth cache with a JSON body', async () => {
    const res = await client().authCacheInvalidate({ tenant_id: 't1', api_keys: ['sk-a', 'sk-b'] }) as Record<string, unknown>;
    assert.equal(res['invalidated'], 2);
    assert.equal(last.method, 'DELETE');
    assert.equal(last.url, '/api/v1/auth/cache');
    const sent = JSON.parse(last.body) as Record<string, unknown>;
    assert.equal(sent['tenant_id'], 't1');
    assert.deepEqual(sent['api_keys'], ['sk-a', 'sk-b']);
  });

  it('fetches usage stats and cluster status', async () => {
    const usage = await client().statsUsage() as Record<string, unknown>;
    assert.equal((usage['totals'] as Record<string, unknown>)['requests'], 42);
    const cluster = await client().clusterStatus() as Record<string, unknown>;
    assert.equal(cluster['cluster'], false);
  });

  it('probes a tenant auth URL', async () => {
    const res = await client().tenantAuthTest({ auth_url: 'https://auth.example.test/v' }) as Record<string, unknown>;
    assert.equal(res['ok'], true);
    assert.equal(res['verdict'], 'denied');
    assert.equal(last.method, 'POST');
    assert.equal(last.url, '/api/v1/tenants/auth/test');
    const sent = JSON.parse(last.body) as Record<string, unknown>;
    assert.equal(sent['auth_url'], 'https://auth.example.test/v');
  });

  it('parses health as an object', async () => {
    const res = await client().health();
    assert.equal((res as Record<string, unknown>)['status'], 'ok');
  });

  it('throws HydraApiError carrying the HTTP status on non-2xx', async () => {
    await assert.rejects(
      () => client().get('providers', 'missing'),
      (err: unknown) => {
        assert.ok(err instanceof HydraApiError, 'expected HydraApiError');
        assert.equal((err as HydraApiError).status, 404);
        return true;
      },
    );
  });

  it('rejects on transport failure (bad host)', async () => {
    const bad = new HydraClient({ baseUrl: 'http://127.0.0.1:1', token: 't', json: false, verbose: false });
    await assert.rejects(() => bad.list('providers'), HydraApiError);
  });
});

describe('resolveConfig', () => {
  it('throws ConfigError when no token is available', () => {
    delete process.env.HYDRA_ADMIN_TOKEN;
    assert.throws(() => resolveConfig({}), ConfigError);
  });

  it('reads the token from HYDRA_ADMIN_TOKEN and uses the default base URL', () => {
    process.env.HYDRA_ADMIN_TOKEN = 'envtok';
    try {
      const cfg = resolveConfig({});
      assert.equal(cfg.token, 'envtok');
      assert.equal(cfg.baseUrl, 'http://127.0.0.1:8081');
    } finally {
      delete process.env.HYDRA_ADMIN_TOKEN;
    }
  });

  it('flag token beats env token, and strips trailing slashes', () => {
    process.env.HYDRA_ADMIN_TOKEN = 'envtok';
    try {
      const cfg = resolveConfig({ token: 'flagtok', baseUrl: 'http://example.com///' });
      assert.equal(cfg.token, 'flagtok');
      assert.equal(cfg.baseUrl, 'http://example.com');
    } finally {
      delete process.env.HYDRA_ADMIN_TOKEN;
    }
  });
});
