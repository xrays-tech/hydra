import { describe, it, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import {
  HydraClient,
  HTTPError,
  InvalidatePendingError,
  InvalidateUnavailableError,
  MAX_TIMEOUT_MS,
  isDoneState,
  isInvalidatePendingError,
  isInvalidateUnavailableError,
  isRetryableState,
  type FleetState,
  type HydraClientConfig,
  type WaitMode,
} from '../src/client.js';

/** Overrides for {@link fleetBody}, i.e. the documented §5.2 response envelope. */
interface FleetBodyOptions {
  state?: FleetState;
  nodesTotal?: number;
  nodesApplied?: number;
  lagging?: string[];
  /** `null` renders the `event_id: null` the server sends for `single_node`. */
  eventId?: string | null;
  waitedMs?: number;
  invalidated?: number;
  checked?: number;
  scope?: string;
}

/**
 * Renders the documented invalidate response envelope (§5.2): the per-node
 * counters plus the `fleet` object the caller must branch on.
 */
function fleetBody(options: FleetBodyOptions = {}): string {
  return JSON.stringify({
    invalidated: options.invalidated ?? 2,
    checked: options.checked ?? 2,
    tenant_id: 't-acme',
    scope: options.scope ?? 'keys',
    fleet: {
      state: options.state ?? 'applied',
      nodes_total: options.nodesTotal ?? 3,
      nodes_applied: options.nodesApplied ?? 3,
      lagging: options.lagging ?? [],
      event_id: options.eventId === undefined ? '1737-0' : options.eventId,
      waited_ms: options.waitedMs ?? 41,
    },
  });
}

/**
 * The body a node sends for `status` when a test does not override it: the
 * documented envelope for the fleet-aware statuses (200 applied, 202 pending,
 * 503 unavailable) and a bare empty body for everything else — which is what
 * makes a `500` a NON-fleet-aware failure, as the SDK requires.
 */
function defaultBody(status: number): string {
  if (status === 202) {
    return fleetBody({
      state: 'pending',
      nodesTotal: 3,
      nodesApplied: 1,
      lagging: ['hydra-2', 'hydra-3'],
      waitedMs: 2000,
    });
  }
  if (status === 503) {
    return fleetBody({
      state: 'unavailable',
      nodesTotal: 3,
      nodesApplied: 0,
      lagging: ['hydra-1', 'hydra-2', 'hydra-3'],
      eventId: '1739-0',
      waitedMs: 7,
    });
  }
  if (status >= 200 && status < 300) return fleetBody();
  return '';
}

interface TestNode {
  leaderStatus: number;
  endpointStatus: number;
  endpointCalls: number;
  /** Raw body override; when unset the node sends {@link defaultBody}. */
  endpointBody?: string;
  /** `X-Hydra-Trace-Id` sent on the invalidation response. */
  traceId?: string;
  auth?: string;
  /** Full request URI (path + query) seen by the invalidation endpoint. */
  requestPath?: string;
}

const nodes = new Map<string, TestNode>();
const servers: http.Server[] = [];

function startNode(initial: Partial<TestNode> = {}): Promise<string> {
  const state: TestNode = {
    leaderStatus: 503,
    endpointStatus: 200,
    endpointCalls: 0,
    ...initial,
  };
  return new Promise((resolve) => {
    const server = http.createServer((req, res) => {
      const url = new URL(req.url ?? '/', 'http://localhost');
      if (url.pathname === '/healthz/leader') {
        res.writeHead(state.leaderStatus, { 'Content-Type': 'application/json' });
        res.end(state.leaderStatus === 200 ? '{"leader":true}' : '');
        return;
      }
      if (url.pathname === '/tenant/t-acme/api/v1/auth/cache/invalidate' && req.method === 'POST') {
        state.endpointCalls += 1;
        state.auth = req.headers.authorization;
        state.requestPath = req.url;
        const headers: Record<string, string> = { 'Content-Type': 'application/json' };
        if (state.traceId !== undefined) headers['X-Hydra-Trace-Id'] = state.traceId;
        res.writeHead(state.endpointStatus, headers);
        res.end(state.endpointBody ?? defaultBody(state.endpointStatus));
        return;
      }
      res.writeHead(404);
      res.end();
    });
    server.listen(0, '127.0.0.1', () => {
      const { port } = server.address() as AddressInfo;
      nodes.set(`http://127.0.0.1:${port}`, state);
      servers.push(server);
      resolve(`http://127.0.0.1:${port}`);
    });
  });
}

function makeClient(urls: string[], extra: Partial<HydraClientConfig> = {}) {
  return new HydraClient({
    token: 'sk-tenant-token',
    nodes: urls,
    probeTimeoutMs: 1000,
    requestTimeoutMs: 1000,
    disableBackgroundRecheck: true,
    ...extra,
  });
}

/** Awaits a promise and returns whatever it threw, or undefined on success. */
async function captureError(promise: Promise<unknown>): Promise<unknown> {
  try {
    await promise;
    return undefined;
  } catch (err) {
    return err;
  }
}

describe('HydraClient', () => {
  after(() => {
    for (const server of servers) server.close();
  });

  it('uses the leader even when it is not first', async () => {
    const standby = await startNode({ leaderStatus: 503 });
    const leader = await startNode({ leaderStatus: 200 });
    const client = makeClient([standby, leader]);
    client.close();
    await client.invalidateTenantAuthCache('t-acme');
    assert.equal(nodes.get(leader)!.endpointCalls, 1);
    assert.equal(nodes.get(standby)!.endpointCalls, 0);
  });

  it('fails over and removes dead nodes', async () => {
    const dead = await startNode({ leaderStatus: 200, endpointStatus: 500 });
    const healthy = await startNode({ leaderStatus: 503 });
    const client = makeClient([dead, healthy]);
    client.close();
    await client.invalidateTenantAuthCache('t-acme');
    assert.equal(nodes.get(dead)!.endpointCalls, 1);
    assert.equal(nodes.get(healthy)!.endpointCalls, 1);
    assert.deepEqual(client.nodes, [healthy]);
    assert.deepEqual(client.removedNodes, [dead]);
  });

  it('probeRemovedNodes restores reachable nodes', async () => {
    const dead = await startNode({ leaderStatus: 200, endpointStatus: 500 });
    const healthy = await startNode({ leaderStatus: 503 });
    const client = makeClient([dead, healthy]);
    client.close();
    await client.invalidateTenantAuthCache('t-acme');
    assert.equal(client.removedNodes.length, 1);
    nodes.get(dead)!.endpointStatus = 200;
    await client.probeRemovedNodes();
    assert.deepEqual(new Set(client.nodes), new Set([dead, healthy]));
    assert.deepEqual(client.removedNodes, []);
  });

  it('supports single node without leader probe', async () => {
    const node = await startNode({ leaderStatus: 404 });
    const client = makeClient([node]);
    client.close();
    await client.invalidateTenantAuthCache('t-acme');
    assert.equal(nodes.get(node)!.endpointCalls, 1);
  });

  it('falls back to a direct send when the leader probe is 404', async () => {
    // `/healthz/leader` is an ADMIN-port route, so a node list pointing at
    // data-plane ports (the documented configuration) gets 404 from the probe
    // while being perfectly able to serve the invalidation.
    const first = await startNode({ leaderStatus: 404 });
    const second = await startNode({ leaderStatus: 404 });
    const client = makeClient([first, second]);
    client.close();
    await client.invalidateTenantAuthCache('t-acme');
    assert.equal(nodes.get(first)!.endpointCalls, 1);
    assert.equal(nodes.get(second)!.endpointCalls, 0);
    // A 404 probe is not evidence of a dead node: nothing may be quarantined,
    // or a data-plane node list would empty itself out.
    assert.deepEqual(client.removedNodes, []);
    assert.deepEqual(client.nodes, [first, second]);
  });

  it('targets the tenant data-plane path', async () => {
    // Regression for the 2026-09-17 route move: the endpoint lives under the
    // data-plane reserved prefix, NOT on the management API at
    // `/api/v1/tenants/{id}/...` (which no longer exists, and which is served
    // on the admin port this SDK is not pointed at).
    const node = await startNode({ leaderStatus: 404 });
    const client = makeClient([node]);
    client.close();
    await client.invalidateTenantAuthCache('t-acme');
    const seen = nodes.get(node)!.requestPath;
    assert.equal(seen, '/tenant/t-acme/api/v1/auth/cache/invalidate');
    assert.ok(!seen!.includes('/api/v1/tenants/'), `must not target the removed route: ${seen}`);
    assert.ok(!seen!.includes('tenants/t-acme'), `must not target the removed route: ${seen}`);
  });

  it('path-escapes the tenant id', async () => {
    const seen: { path?: string } = {};
    const server = http.createServer((req, res) => {
      seen.path = req.url;
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(fleetBody());
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    servers.push(server);
    const { port } = server.address() as AddressInfo;
    const client = makeClient([`http://127.0.0.1:${port}`]);
    client.close();
    await client.invalidateTenantAuthCache('t/acme');
    assert.equal(seen.path, '/tenant/t%2Facme/api/v1/auth/cache/invalidate');
  });

  it('does not remove nodes on 401', async () => {
    const node = await startNode({ leaderStatus: 200, endpointStatus: 401 });
    const client = makeClient([node]);
    client.close();
    await assert.rejects(() => client.invalidateTenantAuthCache('t-acme'), /401/);
    assert.deepEqual(client.removedNodes, []);
    assert.deepEqual(client.nodes, [node]);
  });

  it('sends Authorization and api_keys body', async () => {
    let seenAuth: string | undefined;
    let seenBody = '';
    const server = http.createServer((req, res) => {
      if (req.url === '/healthz/leader') {
        res.writeHead(200);
        res.end();
        return;
      }
      let body = '';
      req.on('data', (chunk) => { body += chunk; });
      req.on('end', () => {
        if (req.url === '/tenant/t-acme/api/v1/auth/cache/invalidate' && req.method === 'POST') {
          seenAuth = req.headers.authorization;
          seenBody = body;
        }
        res.writeHead(200, { 'Content-Type': 'application/json' });
        res.end(fleetBody());
      });
    });
    await new Promise<void>((resolve) => server.listen(0, '127.0.0.1', resolve));
    servers.push(server);
    const { port } = server.address() as AddressInfo;
    const client = makeClient([`http://127.0.0.1:${port}`]);
    client.close();
    await client.invalidateTenantAuthCacheKeys('t-acme', ['key1', 'key2']);
    assert.equal(seenAuth, 'Bearer sk-tenant-token');
    assert.match(seenBody, /key1/);
    assert.match(seenBody, /key2/);
  });

  it('background recheck restores removed nodes', async () => {
    const dead = await startNode({ leaderStatus: 200, endpointStatus: 500 });
    const healthy = await startNode({ leaderStatus: 503 });
    const client = new HydraClient({
      token: 'sk-tenant-token',
      nodes: [dead, healthy],
      probeTimeoutMs: 1000,
      requestTimeoutMs: 1000,
      recheckIntervalMs: 10,
    });
    try {
      await client.invalidateTenantAuthCache('t-acme');
      nodes.get(dead)!.endpointStatus = 200;
      const deadline = Date.now() + 2000;
      while (Date.now() < deadline) {
        if (client.nodes.length === 2 && client.removedNodes.length === 0) return;
        await new Promise((resolve) => setTimeout(resolve, 10));
      }
      assert.fail(`node not restored: nodes=${client.nodes} removed=${client.removedNodes}`);
    } finally {
      client.close();
    }
  });

  it('does not quarantine a node on request timeout', async () => {
    const node = 'http://127.0.0.1:1';
    // The leader probe succeeds so the node is considered alive; the
    // invalidation request then hangs until the client's request timeout
    // aborts it (producing an AbortError).
    const fetchImpl = ((url: string, init: RequestInit) => {
      if (url.endsWith('/healthz/leader')) {
        return Promise.resolve(new Response('{"leader":true}', { status: 200 }));
      }
      return new Promise<Response>((_resolve, reject) => {
        const signal = init.signal;
        if (signal) {
          signal.addEventListener('abort', () => {
            reject(new DOMException('The operation was aborted.', 'AbortError'));
          });
        }
      });
    }) as typeof fetch;
    const client = makeClient([node], { fetchImpl, requestTimeoutMs: 100 });
    client.close();
    await assert.rejects(() => client.invalidateTenantAuthCache('t-acme'), /failed/);
    assert.deepEqual(client.removedNodes, []);
    assert.deepEqual(client.nodes, [node]);
  });

  it('quarantines a node on connection refused', async () => {
    const node = 'http://127.0.0.1:1';
    const fetchImpl = ((url: string, _init: RequestInit) => {
      if (url.endsWith('/healthz/leader')) {
        return Promise.resolve(new Response('{"leader":true}', { status: 200 }));
      }
      const err = new TypeError('fetch failed');
      (err as { cause?: unknown }).cause = new Error('ECONNREFUSED 127.0.0.1:1');
      return Promise.reject(err);
    }) as typeof fetch;
    const client = makeClient([node], { fetchImpl });
    client.close();
    await assert.rejects(() => client.invalidateTenantAuthCache('t-acme'), /failed/);
    assert.deepEqual(client.removedNodes, [node]);
    assert.deepEqual(client.nodes, []);
  });

  it('wraps an unparseable node URL in a friendly hydra error', () => {
    assert.throws(
      () => new HydraClient({ token: 'sk-tenant-token', nodes: ['not a url'] }),
      /hydra: invalid node URL/,
    );
  });

  // -------------------------------------------------------------------------
  // §5.2 three-state contract: HTTP 200 applied / single_node
  // -------------------------------------------------------------------------

  it('reports HTTP 200 applied as done, with the full result', async () => {
    const node = await startNode({
      leaderStatus: 404,
      endpointStatus: 200,
      traceId: 'trace-applied-1',
    });
    const client = makeClient([node]);
    client.close();

    // The wrapper accepts it...
    await client.invalidateTenantAuthCache('t-acme');

    // ...and the structured method surfaces every documented field.
    const result = await client.invalidateWithResult('t-acme', ['key1', 'key2']);
    assert.equal(result.httpStatus, 200);
    assert.equal(result.state, 'applied');
    assert.equal(result.nodesApplied, 3);
    assert.equal(result.nodesTotal, 3);
    assert.equal(result.eventId, '1737-0');
    assert.equal(result.waitedMs, 41);
    assert.equal(result.traceId, 'trace-applied-1');
    assert.equal(result.node, node);
    assert.equal(result.invalidated, 2);
    assert.equal(result.checked, 2);
    assert.equal(result.scope, 'keys');
    assert.deepEqual(result.lagging, []);
    assert.equal(isDoneState(result.state), true);
    assert.equal(isRetryableState(result.state), false);
    assert.deepEqual(client.removedNodes, []);
  });

  it('reports HTTP 200 single_node as done', async () => {
    const node = await startNode({
      leaderStatus: 404,
      endpointStatus: 200,
      endpointBody: fleetBody({
        state: 'single_node',
        nodesTotal: 1,
        nodesApplied: 1,
        eventId: null,
        waitedMs: 0,
        scope: 'tenant',
        checked: 0,
        invalidated: 7,
      }),
      traceId: 'trace-single-1',
    });
    const client = makeClient([node]);
    client.close();

    await client.invalidateTenantAuthCache('t-acme');
    const result = await client.invalidateWithResult('t-acme');
    assert.equal(result.state, 'single_node');
    assert.equal(isDoneState(result.state), true);
    assert.equal(isRetryableState(result.state), false);
    assert.equal(result.nodesApplied, 1);
    assert.equal(result.nodesTotal, 1);
    // The server reports no event id for a single node.
    assert.equal(result.eventId, undefined);
    assert.equal(result.waitedMs, 0);
    assert.equal(result.scope, 'tenant');
    assert.equal(result.invalidated, 7);
    assert.deepEqual(result.lagging, []);
    assert.deepEqual(client.removedNodes, []);
  });

  // -------------------------------------------------------------------------
  // §5.2 three-state contract: HTTP 202 pending
  // -------------------------------------------------------------------------

  it('reports HTTP 202 pending as a result and rejects the wrapper with InvalidatePendingError', async () => {
    const first = await startNode({
      leaderStatus: 404,
      endpointStatus: 202,
      endpointBody: fleetBody({
        state: 'pending',
        nodesTotal: 3,
        nodesApplied: 1,
        lagging: ['hydra-2', 'hydra-3'],
        eventId: '1738-0',
        waitedMs: 2000,
      }),
      traceId: 'trace-pending-1',
    });
    // Would answer `applied` — it must never be reached: a fleet-aware answer is
    // the node's verdict about the CLUSTER, so rotating would only ask the same
    // question again.
    const second = await startNode({ leaderStatus: 404 });
    const client = makeClient([first, second]);
    client.close();

    const result = await client.invalidateWithResult('t-acme', ['key1', 'key2']);
    assert.equal(result.httpStatus, 202);
    assert.equal(result.state, 'pending');
    assert.equal(isDoneState(result.state), false);
    assert.equal(isRetryableState(result.state), true);
    assert.deepEqual(result.lagging, ['hydra-2', 'hydra-3']);
    assert.equal(result.eventId, '1738-0');
    assert.equal(result.waitedMs, 2000);
    assert.equal(result.traceId, 'trace-pending-1');
    assert.equal(result.nodesApplied, 1);
    assert.equal(result.nodesTotal, 3);
    // No rotation and no quarantine: `pending` is not an error at the node level.
    assert.equal(nodes.get(first)!.endpointCalls, 1);
    assert.equal(nodes.get(second)!.endpointCalls, 0);
    assert.deepEqual(client.nodes, [first, second]);
    assert.deepEqual(client.removedNodes, []);

    const err = await captureError(client.invalidateTenantAuthCache('t-acme'));
    assert.ok(err instanceof InvalidatePendingError, `want InvalidatePendingError, got ${String(err)}`);
    assert.ok(isInvalidatePendingError(err));
    assert.ok(!(err instanceof InvalidateUnavailableError));
    assert.ok(!isInvalidateUnavailableError(err));
    assert.equal(err.result.state, 'pending');
    assert.equal(err.result.traceId, 'trace-pending-1');
    // The message names the state, the node, the counts, the event id and the trace id.
    assert.match(err.message, /hydra: auth cache invalidation is pending/);
    assert.match(err.message, /"pending"/);
    assert.match(err.message, new RegExp(first.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
    assert.match(err.message, /1\/3 nodes applied/);
    assert.match(err.message, /"1738-0"/);
    assert.match(err.message, /trace-pending-1/);

    assert.equal(nodes.get(second)!.endpointCalls, 0);
    assert.deepEqual(client.removedNodes, []);
    assert.deepEqual(client.nodes, [first, second]);
  });

  it('treats a wait=none 202 as pending too', async () => {
    // `wait=none` answers 202 without looking, so it is still not "done": the
    // caller must reconcile with `eventId` or retry.
    const node = await startNode({
      leaderStatus: 404,
      endpointStatus: 202,
      endpointBody: fleetBody({
        state: 'pending',
        nodesTotal: 2,
        nodesApplied: 0,
        lagging: ['hydra-1', 'hydra-2'],
        eventId: '1740-0',
        waitedMs: 0,
      }),
      traceId: 'trace-waitnone-1',
    });
    const client = makeClient([node], { waitMode: 'none' });
    client.close();

    const result = await client.invalidateWithResult('t-acme');
    assert.equal(result.state, 'pending');
    assert.equal(isDoneState(result.state), false);
    assert.deepEqual(result.lagging, ['hydra-1', 'hydra-2']);
    assert.equal(result.eventId, '1740-0');

    const err = await captureError(client.invalidateTenantAuthCache('t-acme'));
    assert.ok(err instanceof InvalidatePendingError);
  });

  // -------------------------------------------------------------------------
  // §5.2 three-state contract: HTTP 503 unavailable
  // -------------------------------------------------------------------------

  it('reports HTTP 503 unavailable as a result without quarantining or rotating', async () => {
    const unavailable = await startNode({
      leaderStatus: 200,
      endpointStatus: 503,
      endpointBody: fleetBody({
        state: 'unavailable',
        nodesTotal: 3,
        nodesApplied: 0,
        lagging: ['hydra-1', 'hydra-2', 'hydra-3'],
        eventId: '1739-0',
        waitedMs: 7,
      }),
      traceId: 'trace-unavailable-1',
    });
    // The node the client used to rotate to, losing the signal. It must not be
    // asked: the 503 node ANSWERED, so a rotation is not a fix.
    const second = await startNode({ leaderStatus: 503 });
    const client = makeClient([unavailable, second]);
    client.close();

    const result = await client.invalidateWithResult('t-acme');
    assert.equal(result.httpStatus, 503);
    assert.equal(result.state, 'unavailable');
    assert.equal(isDoneState(result.state), false);
    assert.equal(isRetryableState(result.state), true);
    assert.equal(result.traceId, 'trace-unavailable-1');
    assert.equal(result.eventId, '1739-0');
    assert.equal(result.nodesApplied, 0);
    assert.equal(result.nodesTotal, 3);
    assert.equal(result.node, unavailable);

    const err = await captureError(client.invalidateTenantAuthCache('t-acme'));
    assert.ok(err instanceof InvalidateUnavailableError, `want InvalidateUnavailableError, got ${String(err)}`);
    assert.ok(isInvalidateUnavailableError(err));
    // Distinguishable from the other non-done outcome in both directions.
    assert.ok(!(err instanceof InvalidatePendingError));
    assert.ok(!isInvalidatePendingError(err));
    assert.equal(err.result.state, 'unavailable');
    assert.match(err.message, /hydra: auth cache invalidation is unavailable/);
    assert.match(err.message, /"unavailable"/);
    assert.match(err.message, new RegExp(unavailable.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')));
    assert.match(err.message, /0\/3 nodes applied/);
    assert.match(err.message, /"1739-0"/);
    assert.match(err.message, /trace-unavailable-1/);

    // Neither node was quarantined, and the second node was never asked.
    assert.equal(nodes.get(unavailable)!.endpointCalls, 2);
    assert.equal(nodes.get(second)!.endpointCalls, 0);
    assert.deepEqual(client.removedNodes, []);
    assert.deepEqual(client.nodes, [unavailable, second]);
  });

  it('still treats a NON-fleet-aware 503 as a node failure and rotates', async () => {
    // The classification is about the body, not the status: a `503 not_ready`
    // has no fleet report, so it is an ordinary node failure.
    const notReady = await startNode({
      leaderStatus: 200,
      endpointStatus: 503,
      endpointBody: '{"error":{"code":"not_ready","message":"warming up"}}',
      traceId: 'trace-notready-1',
    });
    const healthy = await startNode({ leaderStatus: 503 });
    const client = makeClient([notReady, healthy]);
    client.close();

    const result = await client.invalidateWithResult('t-acme');
    assert.equal(result.node, healthy);
    assert.equal(result.state, 'applied');
    assert.equal(nodes.get(notReady)!.endpointCalls, 1);
    assert.equal(nodes.get(healthy)!.endpointCalls, 1);
    assert.deepEqual(client.removedNodes, [notReady]);
    assert.deepEqual(client.nodes, [healthy]);
  });

  // -------------------------------------------------------------------------
  // Unchanged failover behaviour for genuinely non-fleet-aware failures
  // -------------------------------------------------------------------------

  it('still fails over and quarantines a node on a bare 500', async () => {
    const broken = await startNode({
      leaderStatus: 200,
      endpointStatus: 500,
      traceId: 'trace-500-1',
    });
    const healthy = await startNode({ leaderStatus: 503 });
    const client = makeClient([broken, healthy]);
    client.close();

    const result = await client.invalidateWithResult('t-acme');
    assert.equal(result.node, healthy);
    assert.equal(result.state, 'applied');
    assert.equal(nodes.get(broken)!.endpointCalls, 1);
    assert.equal(nodes.get(healthy)!.endpointCalls, 1);
    assert.deepEqual(client.removedNodes, [broken]);
    assert.deepEqual(client.nodes, [healthy]);
  });

  it('fails with the underlying HTTP error when every node answers a bare 500', async () => {
    const broken = await startNode({ leaderStatus: 404, endpointStatus: 500, traceId: 'trace-500-2' });
    const client = makeClient([broken]);
    client.close();

    await assert.rejects(
      () => client.invalidateWithResult('t-acme'),
      /hydra: all 1 node\(s\) failed: .*unexpected HTTP 500/,
    );
    assert.deepEqual(client.removedNodes, [broken]);
  });

  // -------------------------------------------------------------------------
  // Undecodable bodies: never invent a state
  // -------------------------------------------------------------------------

  it('never reports a 2xx with an undecodable body as success', async () => {
    const legacy = await startNode({
      leaderStatus: 404,
      endpointStatus: 200,
      endpointBody: '{"invalidated":1}',
      traceId: 'trace-legacy-1',
    });
    const client = makeClient([legacy]);
    client.close();

    const result = await client.invalidateWithResult('t-acme');
    assert.equal(result.httpStatus, 200);
    assert.equal(result.state, undefined);
    assert.equal(isDoneState(result.state), false);
    assert.equal(isRetryableState(result.state), false);
    assert.deepEqual(result.lagging, []);
    assert.equal(result.traceId, 'trace-legacy-1');
    // An unreadable body is not a node failure, so nothing is quarantined.
    assert.deepEqual(client.removedNodes, []);

    const err = await captureError(client.invalidateTenantAuthCache('t-acme'));
    assert.ok(err instanceof Error);
    assert.ok(!(err instanceof InvalidatePendingError));
    assert.ok(!(err instanceof InvalidateUnavailableError));
    assert.match(err.message, /unrecognised fleet\.state/);
  });

  it('treats an undocumented fleet.state as undecodable', async () => {
    const node = await startNode({
      leaderStatus: 404,
      endpointStatus: 200,
      endpointBody: '{"invalidated":1,"checked":1,"scope":"keys","fleet":{"state":"converging"}}',
    });
    const client = makeClient([node]);
    client.close();

    const result = await client.invalidateWithResult('t-acme');
    assert.equal(result.state, undefined);
    await assert.rejects(() => client.invalidateTenantAuthCache('t-acme'), /unrecognised fleet\.state/);
  });

  // -------------------------------------------------------------------------
  // wait / timeout_ms plumbing
  // -------------------------------------------------------------------------

  it('sends NEITHER wait nor timeout_ms when they are not configured', async () => {
    const node = await startNode({ leaderStatus: 404 });
    const client = makeClient([node]);
    client.close();
    await client.invalidateTenantAuthCache('t-acme');

    const raw = nodes.get(node)!.requestPath!;
    assert.equal(new URL(raw, 'http://localhost').search, '');
    assert.ok(!raw.includes('wait'), `no wait parameter expected: ${raw}`);
    assert.ok(!raw.includes('timeout_ms'), `no timeout_ms parameter expected: ${raw}`);
  });

  it('appends wait and timeout_ms when configured', async () => {
    const node = await startNode({ leaderStatus: 404 });
    const client = makeClient([node], { waitMode: 'converged', timeoutMs: 2500 });
    client.close();
    await client.invalidateWithResult('t-acme');

    const raw = nodes.get(node)!.requestPath!;
    assert.equal(raw, '/tenant/t-acme/api/v1/auth/cache/invalidate?wait=converged&timeout_ms=2500');
    const params = new URL(raw, 'http://localhost').searchParams;
    assert.equal(params.get('wait'), 'converged');
    assert.equal(params.get('timeout_ms'), '2500');
  });

  it('sends wait=none on its own when timeoutMs is unset', async () => {
    const node = await startNode({
      leaderStatus: 404,
      endpointStatus: 202,
      traceId: 'trace-none-1',
    });
    const client = makeClient([node], { waitMode: 'none' });
    client.close();
    const result = await client.invalidateWithResult('t-acme');
    assert.equal(result.state, 'pending');

    const raw = nodes.get(node)!.requestPath!;
    assert.equal(raw, '/tenant/t-acme/api/v1/auth/cache/invalidate?wait=none');
    assert.ok(!raw.includes('timeout_ms'), `no timeout_ms parameter expected: ${raw}`);
  });

  it('validates waitMode and timeoutMs at construction', () => {
    const base: HydraClientConfig = {
      token: 'sk-tenant-token',
      nodes: ['http://127.0.0.1:8080'],
      disableBackgroundRecheck: true,
    };

    for (const waitMode of ['converged ', ' converged', 'Converged', 'none ', 'wait', '']) {
      assert.throws(
        () => new HydraClient({ ...base, waitMode: waitMode as WaitMode }),
        /hydra: invalid waitMode/,
        `waitMode ${JSON.stringify(waitMode)} must be refused (the server does not trim it)`,
      );
    }

    for (const timeoutMs of [0, -1, 60001, 1.5, Number.NaN, Number.POSITIVE_INFINITY]) {
      assert.throws(
        () => new HydraClient({ ...base, timeoutMs }),
        /hydra: invalid timeoutMs/,
        `timeoutMs ${String(timeoutMs)} must be refused`,
      );
    }

    // The bounds themselves are accepted, and an unset timeoutMs is the
    // documented "send no parameter" case rather than an error.
    assert.equal(MAX_TIMEOUT_MS, 60000);
    const lower = new HydraClient({ ...base, waitMode: 'none', timeoutMs: 1 });
    lower.close();
    const upper = new HydraClient({ ...base, waitMode: 'converged', timeoutMs: MAX_TIMEOUT_MS });
    upper.close();
    const unset = new HydraClient(base);
    unset.close();
  });

  // -------------------------------------------------------------------------
  // Trace id on the error path (§4.2: quote it when reporting a problem)
  // -------------------------------------------------------------------------

  it('captures X-Hydra-Trace-Id on an HTTP error path', async () => {
    const node = await startNode({
      leaderStatus: 200,
      endpointStatus: 401,
      traceId: 'trace-401-1',
    });
    const client = makeClient([node]);
    client.close();

    const err = await captureError(client.invalidateTenantAuthCache('t-acme'));
    assert.ok(err instanceof HTTPError, `want HTTPError, got ${String(err)}`);
    assert.equal(err.status, 401);
    assert.equal(err.traceId, 'trace-401-1');
    assert.match(err.message, /X-Hydra-Trace-Id: trace-401-1/);
    // A 401 is a client-side problem: nothing is quarantined.
    assert.deepEqual(client.removedNodes, []);
    assert.deepEqual(client.nodes, [node]);
  });
});
