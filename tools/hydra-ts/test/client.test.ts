import { describe, it, before, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import type { AddressInfo } from 'node:net';
import { HydraClient, type HydraClientConfig } from '../src/client.js';

interface TestNode {
  leaderStatus: number;
  endpointStatus: number;
  endpointCalls: number;
  auth?: string;
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
      if (url.pathname === '/api/v1/tenants/t-acme/auth/cache/invalidate' && req.method === 'POST') {
        state.endpointCalls += 1;
        state.auth = req.headers.authorization;
        res.writeHead(state.endpointStatus, { 'Content-Type': 'application/json' });
        res.end(state.endpointStatus >= 200 && state.endpointStatus < 300 ? '{"invalidated":1}' : '');
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

describe('HydraClient', () => {
  before(async () => {
    // no-op; servers are created per test and tracked below
  });
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
        if (req.url === '/api/v1/tenants/t-acme/auth/cache/invalidate' && req.method === 'POST') {
          seenAuth = req.headers.authorization;
          seenBody = body;
        }
        res.writeHead(200);
        res.end('{"invalidated":1}');
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
});
