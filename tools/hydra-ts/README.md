# Hydra TypeScript SDK (tenant auth-cache invalidation)

A small TypeScript SDK for Hydra tenants to invalidate their own auth cache
through:

```
POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate
```

It connects to a Hydra cluster, automatically discovers the current leader by
probing `/healthz/leader`, sends the invalidation to the active leader, and
fails over to the next reachable node when a node becomes unreachable or
returns a server error.

## Features

- Leader discovery via token-free `/healthz/leader`
- Automatic failover and quarantine of dead nodes
- Background timer that re-adds recovered nodes
- Single-node mode fallback
- No runtime dependencies (uses global `fetch`)

## Usage

```ts
import { HydraClient } from 'hydra-tenant-sdk';

const client = new HydraClient({
  token: 'sk-tenant-self-service-token',
  nodes: [
    'http://hydra-1:8081',
    'http://hydra-2:8081',
    'http://hydra-3:8081',
  ],
  probeTimeoutMs: 2000,
  requestTimeoutMs: 10000,
  recheckIntervalMs: 30000,
});

await client.invalidateTenantAuthCache('tenant-123');
client.close();
```

You can also use the positional constructor:

```ts
const client = new HydraClient('sk-tenant-self-service-token', [
  'http://hydra-1:8081',
  'http://hydra-2:8081',
]);
```

Optional api-key scoped invalidation:

```ts
await client.invalidateTenantAuthCacheKeys('tenant-123', ['key-1', 'key-2']);
```

## API

- `new HydraClient(config)` / `new HydraClient(token, nodes)`
- `invalidateTenantAuthCache(tenantId): Promise<void>`
- `invalidateTenantAuthCacheKeys(tenantId, apiKeys): Promise<void>` / `invalidateCacheKeys(tenantId, apiKeys)`
- `invalidate(tenantId)` / `invalidateTenantCache(tenantId)` / `invalidateCache(tenantId)`
- `nodes` / `removedNodes`
- `probeRemovedNodes(): Promise<void>`
- `close(): void`
