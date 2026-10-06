# Hydra Go SDK (tenant auth-cache invalidation)

A small Go SDK for Hydra tenants to invalidate their own auth cache through:

```
POST {data_plane_node}/tenant/{tenant_id}/api/v1/auth/cache/invalidate[?wait=converged|none][&timeout_ms=1..60000]
Authorization: Bearer <tenant access token>
```

`{data_plane_node}` is the address your clients already use for `/v1/*` — the
**data plane**, default port **8080**. It is **not** the admin port `8081`: the
invalidation endpoint was moved off the management API on 2026-09-17
(`POST /api/v1/tenants/{tenant_id}/auth/cache/invalidate` no longer exists).

The endpoint is answered **locally by every data-plane node**, so the request
does not need to reach the cluster leader. It clears that node's cache
synchronously and fans the invalidation out over the shared bus; the response's
`fleet.state` tells you whether the whole cluster converged. See the tenant
contract in `dev-docs/tenant-api-integration.md` §5.2 and the topology decision
in `dev-docs/design-tenant-api.md` §6.4 (A-1).

## A 2xx is not the same as "done"

This is the most important thing on this page. The contract gives the endpoint
**three** outcomes, and all three tell you to do something different:

| HTTP | `fleet.state` | Meaning | What you must do |
|---|---|---|---|
| `200` | `applied` | Every live node has applied the invalidation | **Done.** Nothing to do. |
| `200` | `single_node` | This node IS the whole data plane | **Done.** Nothing to do. |
| `202` | `pending` | Published, but not every node has confirmed it | **Not done, not an error.** Retry (the call is idempotent), or hand `lagging` to the operator. The banned key may still be served elsewhere until then. |
| `503` | `unavailable` | There is an invalidation channel but it could not do its job, so **the cluster was NOT notified** | **Retry or escalate.** Quote `trace_id`. The node is fine; the fleet was never told. |

Two consequences the SDK now enforces:

- **`202 pending` is never reported as success.** `InvalidateTenantAuthCache`
  returns an `*InvalidatePendingError` for it, with the full fleet report
  attached.
- **`503 unavailable` is not a node failure.** The node answered, it is alive,
  and its own cache was cleared — so it is neither quarantined nor rotated away
  from, and the "the cluster was not notified" signal survives as an
  `*InvalidateUnavailableError`.

Use [`FleetState.Done`](#fleetstate) to decide "done", never the HTTP status
alone.

## Features

- **Structured invalidation result** — `InvalidateTenantAuthCacheResult` carries
  the HTTP status, the fleet state, the node counts, `event_id`, `waited_ms`,
  `lagging` and the `X-Hydra-Trace-Id` header. Nothing is discarded.
- **Correct three-state semantics** — `applied`/`single_node` are success;
  `pending` and `unavailable` are distinguishable, retryable outcomes.
- **`wait` / `timeout_ms` support** — reach the documented bulk mode
  (`WaitNone`) and set your own convergence budget.
- **Trace id on every path** — success and error alike, so you can always quote
  the id the contract asks for.
- **Leader preference (optional)** — probes `/healthz/leader` and tries the
  nodes that report themselves leader first. This is a **legacy optimization,
  not a requirement**: the invalidation is served locally by every data-plane
  node, so any reachable node works. Note that `/healthz/leader` is an
  **admin-port** route — a node list pointing at data-plane ports gets a `404`
  from the probe, which is treated as "alive, not the leader" so the request is
  still sent directly.
- **Automatic failover** — if the selected node is unreachable (or returns an
  error that is *not* a fleet-aware answer), the SDK rotates to the next
  reachable node and temporarily removes the failed node from the active pool.
- **Automatic recovery** — a background rechecker periodically probes removed
  nodes and re-adds them as soon as they become reachable again.
- **Single-node support** — when `/healthz/leader` is not available (single-node
  mode, or a data-plane-only node list), the SDK falls back to sending the
  request directly.
- **Tenant access token via constructor** — the token is supplied once in
  `Config` and attached as `Authorization: Bearer <token>` to every
  invalidation request.

## Usage

```go
package main

import (
    "context"
    "errors"
    "log"
    "time"

    hydra "github.com/ipconfiger/hydra/tools/hydra-go"
)

func main() {
    client, err := hydra.New(hydra.Config{
        Token: "sk-tenant-self-service-token",
        Nodes: []string{
            "http://hydra-1:8080",
            "http://hydra-2:8080",
            "http://hydra-3:8080",
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

    // Preferred: branch on the fleet state yourself.
    res, err := client.InvalidateTenantAuthCacheResult(ctx, "tenant-123", nil)
    if err != nil {
        log.Fatalf("no fleet report: %v", err)
    }
    switch {
    case res.State.Done():
        log.Printf("done: %s (%d/%d nodes, event %s, %dms)",
            res.State, res.NodesApplied, res.NodesTotal, res.EventID, res.WaitedMS)
    case res.State == hydra.FleetPending:
        log.Printf("still converging: lagging=%v event=%s trace=%s (retry is safe)",
            res.Lagging, res.EventID, res.TraceID)
    case res.State == hydra.FleetUnavailable:
        log.Printf("the cluster was NOT notified (trace=%s); retry or page an operator", res.TraceID)
    }
}
```

Optional api-key scoped invalidation:

```go
err := client.InvalidateTenantAuthCacheKeys(ctx, "tenant-123", []string{"key-1", "key-2"})
```

The error-only wrapper, with the two non-done outcomes told apart:

```go
err := client.InvalidateTenantAuthCache(ctx, "tenant-123")
var pending *hydra.InvalidatePendingError
var unavailable *hydra.InvalidateUnavailableError
switch {
case err == nil:
    // applied or single_node
case errors.As(err, &pending):
    // 202: the fleet has not confirmed. Retry, or report pending.Result.Lagging.
    log.Printf("lagging=%v trace=%s", pending.Result.Lagging, pending.Result.TraceID)
case errors.As(err, &unavailable):
    // 503: the cluster was NOT notified. Retry or escalate.
    log.Printf("trace=%s", unavailable.Result.TraceID)
default:
    log.Fatalf("invalidte failed: %v", err)
}
```

### Bulk mode: do not wait for the fleet

`wait=none` publishes and answers `202` immediately with the `event_id`, which
is what a bulk/batch script wants. Reconcile afterwards, or simply retry —
invalidation is idempotent (contract §4.5).

```go
client, _ := hydra.New(hydra.Config{
    Token:    "sk-tenant-self-service-token",
    Nodes:    []string{"http://hydra-1:8080"},
    WaitMode: hydra.WaitNone, // sends ?wait=none
    // TimeoutMS is irrelevant when WaitMode is WaitNone.
})

res, err := client.InvalidateTenantAuthCacheResult(ctx, "tenant-123", nil)
// res.State is `pending` and res.EventID is your reconciliation handle.
```

### Giving the fleet more time

```go
client, _ := hydra.New(hydra.Config{
    Token:     "sk-tenant-self-service-token",
    Nodes:     []string{"http://hydra-1:8080"},
    TimeoutMS: 5000, // sends ?timeout_ms=5000
})
```

## API

### Types

- `Config` — client configuration.
- `FleetState string` with `FleetApplied`, `FleetSingleNode`, `FleetPending`,
  `FleetUnavailable` — the server's own tokens, exposed verbatim.
- `WaitMode string` with `WaitConverged`, `WaitNone`.
- `InvalidateTenantAuthCacheResult` — the structured outcome.
- `HTTPError` — a non-fleet-aware HTTP failure (`401`, `413`, `429`, a bare
  `500`, …).
- `InvalidatePendingError` / `InvalidateUnavailableError` — the two non-done
  outcomes, each carrying `.Result`.

### `FleetState`

- `Done() bool` — true only for `applied` and `single_node`. **Use this, not the
  HTTP status.**
- `Retryable() bool` — true for `pending` and `unavailable`.

### `InvalidateTenantAuthCacheResult` fields

| Field | Meaning |
|---|---|
| `HTTPStatus` | `200` (applied / single_node), `202` (pending) or `503` (unavailable) |
| `State` | The `fleet.state` token verbatim. Empty only when the body could not be decoded into the documented shape — the SDK never guesses a state. |
| `Invalidated` | How many entries **this node** removed from its own cache. Not a fleet count. `Invalidated == 0 && Checked > 0` means those keys were not cached here, which is **not** a failure. |
| `Checked` | How many keys you named (`0` for a whole-tenant clear) |
| `Scope` | `keys` or `tenant` |
| `NodesApplied` / `NodesTotal` | Confirmed / live node counts |
| `Lagging` | Nodes that did not confirm. Never nil, so you can always `range` it. See the warning below — the contract prose and the server disagree here. |
| `EventID` | This invalidation's id on the internal bus, for later reconciliation |
| `WaitedMS` | How long the server actually spent waiting for the fleet |
| `TraceID` | The `X-Hydra-Trace-Id` response header. **Quote it in a support request.** |
| `Node` | Base URL of the node that answered |

### Methods

- `New(Config) (*Client, error)` — create a client and start background node
  recovery. Validates `WaitMode` and `TimeoutMS`.
- `NewWithToken(token string, nodes []string) (*Client, error)` — convenience
  constructor for callers that prefer plain initialization arguments.
- `(*Client).InvalidateTenantAuthCacheResult(ctx, tenantID, apiKeys) (InvalidateTenantAuthCacheResult, error)`
  — **the structured call.** `apiKeys` may be `nil`/empty for a whole-tenant
  clear. Returns a result for `202`/`503`; an error only when no fleet report
  could be obtained.
- `(*Client).InvalidateTenantAuthCache(ctx, tenantID) error` — thin wrapper;
  succeeds only for `applied`/`single_node`.
- `(*Client).InvalidateTenantAuthCacheKeys(ctx, tenantID, apiKeys) error` —
  thin wrapper for api-key scoped invalidation.
- `(*Client).Invalidate(ctx, tenantID) error` — alias.
- `(*Client).InvalidateTenantCache(ctx, tenantID) error` — alias.
- `(*Client).InvalidateCache(ctx, tenantID) error` — alias.
- `(*Client).Nodes() []string` — active nodes.
- `(*Client).RemovedNodes() []string` — currently quarantined nodes.
- `(*Client).ProbeRemovedNodes(ctx)` — manually recheck removed nodes.
- `(*Client).Close() error` — stop the background rechecker.

### Constants

- `MinTimeoutMS` = `1`, `MaxTimeoutMS` = `60000` — the server's accepted range
  for `timeout_ms`. Values outside it are rejected by `New`.

## Read `Lagging` carefully (contract prose vs server code)

`dev-docs/tenant-api-integration.md` §5.2 says `fleet.lagging` is non-empty
"only when `state=pending` and nodes were actually checked and found behind".
**The server does not behave that way for every state**, and the code wins
(§5.2's own prose contradicts itself on this point):

- With `wait=none` (i.e. `WaitMode: hydra.WaitNone`) nobody looks at the fleet,
  and the server names **every live node** as lagging —
  `crates/hydra-server/src/cluster/events.rs:212-223` sets
  `lagging: live_nodes` with `nodes_applied: 0`. The server's own cluster test
  asserts this on purpose: *"nobody was checked, so nobody confirmed"*
  (`crates/hydra-server/tests/tenant_api_cluster.rs:294-320`).
- A `503 unavailable` also carries a non-empty `lagging` when the failure was
  the **convergence barrier** rather than the publish (`events.rs:252-263`); only
  the publish-failure path leaves it empty (`events.rs:200-209`).

So treat `Lagging` as *"nodes not confirmed"*, not as *"nodes proven behind"*:
when you sent `wait=none`, it is unverified, not evidence. The SDK surfaces the
server's list verbatim and does not paper over the difference.

## Behaviour change in the existing wrapper

`InvalidateTenantAuthCache` (and `InvalidateTenantAuthCacheKeys`, and the
aliases) previously returned `nil` for **any** 2xx. It now returns `nil` only
when the fleet state is `applied` or `single_node`:

- A caller that used to see silent success on a `202 pending` now gets an
  `*InvalidatePendingError`. The invalidation was accepted but the fleet is not
  converged, so treating it as done was a bug — the previously-banned key could
  still be served by a node that had not applied the event.
- A `503 unavailable` used to be reported as an `*HTTPError` and additionally
  caused the node to be quarantined and the call rotated to another node. It is
  now reported as an `*InvalidateUnavailableError` and the node stays in the
  pool: it answered, it is alive, and rotating away from it hid the fact that
  **the cluster was never notified**.
- A 2xx whose body does not carry a documented `fleet.state` is no longer
  success either. The SDK refuses to guess a state.

If you were relying on "no error means done", migrate to
`InvalidateTenantAuthCacheResult` and branch on `res.State.Done()`.

## Failover semantics

| Answer | Quarantine the node? | Rotate to another node? |
|---|---|---|
| Fleet-aware response (`200` applied/single_node, `202` pending, `503` unavailable with a decodable `fleet` object) | **No** — the node answered | **No** — the answer is about the *cluster*, and every data-plane node answers that question the same way |
| `401` / `403` (bad or mismatched tenant token) | No | No — abort immediately; another node will not fix a client-side error |
| Transport failure (connection refused, DNS, …) | Yes | Yes |
| Non-fleet-aware HTTP error (`404`, `405`, bare `5xx`, …) | Yes | Yes |
| Request timeout / caller cancellation | No — the node may simply be slow | Yes (if another node is available) |

## How this contract is verified against a REAL node

`go test ./...` (CI `sdks` job) runs against mocks. `integration/test_sdk_go_live.py`
(CI `integration` job for the single-node legs, `live-deps` for the cluster legs; also in
the local gate) compiles a small driver **against this package** — a sidecar `go.mod` with
`replace github.com/ipconfiger/hydra/tools/hydra-go => <repo>/tools/hydra-go`, built with
`GOWORK=off` because the repository has a root `go.work` — and points it at a real
gateway. It pins:

- the documented usage returns `200 single_node` with a trace id, and the call **really
  clears the node's auth cache** (allow → the auth service flips to deny → the request is
  still served from cache → SDK invalidate → the next request is `401`);
- another tenant's token comes back as `*HTTPError` (never as a "done" state), and the
  pre-2026-09-17 management paths are `404`;
- on a real bus: `200 applied` with `NodesApplied/NodesTotal`, `202 pending` via
  `WaitNone` (the raising API returns `*InvalidatePendingError`, `State.Done()` is false),
  and `503 unavailable` when the bus is cut under a running node (returns
  `*InvalidateUnavailableError`). A dead Redis URL cannot produce that state — the node
  refuses to START — so the drill cuts a TCP relay in front of Redis instead;
- **three-way parity**: this SDK, `tools/hydra-py` and `tools/hydra-ts` are all pointed at
  the SAME node and must agree on `state`, `nodesApplied` and `nodesTotal`. Three
  implementations of one documented contract is where a silent divergence would live, and
  no single-SDK test can see it.
