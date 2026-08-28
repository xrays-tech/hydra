# Hydra Python SDK (tenant auth-cache invalidation)

A small Python SDK for Hydra tenants to invalidate their own auth cache through:

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
- Background rechecker that re-adds recovered nodes
- Single-node mode fallback
- No third-party runtime dependencies

## Usage

```python
from hydra_sdk import HydraClient

client = HydraClient(
    token="sk-tenant-self-service-token",
    nodes=[
        "http://hydra-1:8081",
        "http://hydra-2:8081",
        "http://hydra-3:8081",
    ],
    probe_timeout=2.0,
    request_timeout=10.0,
    recheck_interval=30.0,
)
client.invalidate_tenant_auth_cache("tenant-123")
client.close()
```

Optional api-key scoped invalidation:

```python
client.invalidate_tenant_auth_cache_keys("tenant-123", ["key-1", "key-2"])
```

## API

- `HydraClient(token, nodes, ...)`
- `invalidate_tenant_auth_cache(tenant_id)`
- `invalidate_tenant_auth_cache_keys(tenant_id, api_keys)` / `invalidate_cache_keys(tenant_id, api_keys)`
- `invalidate(tenant_id)` / `invalidate_tenant_cache(tenant_id)` / `invalidate_cache(tenant_id)`
- `nodes` / `removed_nodes`
- `probe_removed_nodes()`
- `close()`
