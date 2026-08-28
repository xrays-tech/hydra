# Hydra Go SDK (tenant auth-cache invalidation)

A small Go SDK for Hydra tenants to invalidate their own auth cache through:

```
POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate
```

It connects to a Hydra cluster, automatically discovers the current leader by
probing `/healthz/leader`, sends the invalidation to the active leader, and
fails over to the next reachable node when a node becomes unreachable or
returns a server error.

## Features

- **Leader discovery** — probes all configured nodes via the token-free
  `/healthz/leader` endpoint and prefers the current active leader.
- **Automatic failover** — if the selected leader fails, the SDK rotates to the
  next reachable node and temporarily removes the failed node from the active
  pool.
- **Automatic recovery** — a background rechecker periodically probes removed
  nodes and re-adds them as soon as they become reachable again.
- **Single-node support** — when `/healthz/leader` is not available (single-node
  mode), the SDK falls back to sending the request directly.
- **Tenant access token via constructor** — the token is supplied once in
  `Config` and attached as `Authorization: Bearer <token>` to every
  invalidation request.

## Usage

```go
package main

import (
    "context"
    "log"
    "time"

    hydra "github.com/ipconfiger/hydra/tools/hydra-go"
)

func main() {
    client, err := hydra.New(hydra.Config{
        Token: "sk-tenant-self-service-token",
        Nodes: []string{
            "http://hydra-1:8081",
            "http://hydra-2:8081",
            "http://hydra-3:8081",
        },
        ProbeTimeout:    2 * time.Second,
        RequestTimeout:  10 * time.Second,
        RecheckInterval: 30 * time.Second,
    })
    if err != nil {
        log.Fatal(err)
    }
    defer client.Close()

    ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
    defer cancel()

    if err := client.InvalidateTenantAuthCache(ctx, "tenant-123"); err != nil {
        log.Fatal(err)
    }
}
```

Optional api-key scoped invalidation:

```go
err := client.InvalidateTenantAuthCacheKeys(ctx, "tenant-123", []string{"key-1", "key-2"})
```

## API

- `New(Config) (*Client, error)` — create a client and start background node
  recovery.
- `NewWithToken(token string, nodes []string) (*Client, error)` — convenience
  constructor for callers that prefer plain initialization arguments.
- `(*Client).InvalidateTenantAuthCache(ctx, tenantID) error`
- `(*Client).InvalidateTenantAuthCacheKeys(ctx, tenantID, apiKeys) error`
- `(*Client).Invalidate(ctx, tenantID) error` — alias.
- `(*Client).InvalidateTenantCache(ctx, tenantID) error` — alias.
- `(*Client).InvalidateCache(ctx, tenantID) error` — alias.
- `(*Client).Nodes() []string` — active nodes.
- `(*Client).RemovedNodes() []string` — currently quarantined nodes.
- `(*Client).ProbeRemovedNodes(ctx)` — manually recheck removed nodes.
- `(*Client).Close() error` — stop the background rechecker.
