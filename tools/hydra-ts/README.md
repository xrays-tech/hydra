# Hydra TypeScript SDK (tenant auth-cache invalidation)

A small TypeScript SDK for Hydra tenants to invalidate their own auth cache
through:

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
| `503` | `unavailable` | There is an invalidation channel but it could not do its job, so **the cluster was NOT notified** | **Retry or escalate.** Quote `traceId`. The node is fine; the fleet was never told. |

Two consequences the SDK now enforces:

- **`202 pending` is never reported as success.** `invalidateTenantAuthCache`
  throws an `InvalidatePendingError` for it, with the full fleet report attached.
- **`503 unavailable` is not a node failure.** The node answered, it is alive,
  and its own cache was cleared — so it is neither quarantined nor rotated away
  from, and "the cluster was not notified" survives as an
  `InvalidateUnavailableError`.

Use `isDoneState(result.state)` to decide "done", never the HTTP status alone.

## Features

- **Structured invalidation result** — `invalidateWithResult()` returns an
  `InvalidateTenantAuthCacheResult` carrying the HTTP status, the fleet state,
  the node counts, `eventId`, `waitedMs`, `lagging` and the `X-Hydra-Trace-Id`
  header. Nothing is discarded.
- **Correct three-state semantics** — `applied`/`single_node` are success;
  `pending` and `unavailable` are distinguishable, retryable outcomes.
- **`wait` / `timeout_ms` support** — reach the documented bulk mode
  (`waitMode: 'none'`) and set your own convergence budget (`timeoutMs`).
- **Trace id on every path** — success and error alike, so you can always quote
  the id the contract asks for (§4.2).
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
- **Automatic recovery** — a background timer periodically probes removed nodes
  and re-adds them as soon as they become reachable again.
- **Single-node support** — when `/healthz/leader` is not available (single-node
  mode, or a data-plane-only node list), the SDK falls back to sending the
  request directly.
- **No runtime dependencies** (uses global `fetch`).

## Usage

```ts
import { HydraClient, isDoneState } from 'hydra-tenant-sdk';

const client = new HydraClient({
  token: 'sk-tenant-self-service-token',
  nodes: [
    'http://hydra-1:8080',
    'http://hydra-2:8080',
    'http://hydra-3:8080',
  ],
  probeTimeoutMs: 2000,
  requestTimeoutMs: 10000,
  recheckIntervalMs: 30000,
});

// Preferred: branch on the fleet state yourself.
const res = await client.invalidateWithResult('tenant-123', ['key-1', 'key-2']);
if (isDoneState(res.state)) {
  console.log(`done: ${res.state} (${res.nodesApplied}/${res.nodesTotal} nodes, ` +
    `event ${res.eventId}, ${res.waitedMs}ms)`);
} else if (res.state === 'pending') {
  console.log(`still converging: lagging=${res.lagging} event=${res.eventId} ` +
    `trace=${res.traceId} (retry is safe)`);
} else if (res.state === 'unavailable') {
  console.log(`the cluster was NOT notified (trace=${res.traceId}); retry or page an operator`);
}

client.close();
```

You can also use the positional constructor:

```ts
const client = new HydraClient('sk-tenant-self-service-token', [
  'http://hydra-1:8080',
  'http://hydra-2:8080',
]);
```

Optional api-key scoped invalidation (`apiKeys` omitted or empty clears the whole
tenant):

```ts
await client.invalidateWithResult('tenant-123', ['key-1', 'key-2']);
await client.invalidateTenantAuthCacheKeys('tenant-123', ['key-1', 'key-2']);
```

The error-only wrapper, with the two non-done outcomes told apart:

```ts
import {
  InvalidatePendingError,
  InvalidateUnavailableError,
} from 'hydra-tenant-sdk';

try {
  await client.invalidateTenantAuthCache('tenant-123');
  // applied or single_node
} catch (err) {
  if (err instanceof InvalidatePendingError) {
    // 202: the fleet has not confirmed. Retry, or report err.result.lagging.
    console.log(`lagging=${err.result.lagging} trace=${err.result.traceId}`);
  } else if (err instanceof InvalidateUnavailableError) {
    // 503: the cluster was NOT notified. Retry or escalate.
    console.log(`trace=${err.result.traceId}`);
  } else {
    throw err;
  }
}
```

`isInvalidatePendingError(err)` / `isInvalidateUnavailableError(err)` are exported
type guards for the same two branches, and each error also carries a literal
`state` discriminant (`'pending'` / `'unavailable'`).

### Bulk mode: do not wait for the fleet

`wait=none` publishes and answers `202` immediately with the `event_id`, which is
what a bulk/batch script wants. Reconcile afterwards, or simply retry —
invalidation is idempotent (contract §4.5).

```ts
const client = new HydraClient({
  token: 'sk-tenant-self-service-token',
  nodes: ['http://hydra-1:8080'],
  waitMode: 'none', // sends ?wait=none
  // timeoutMs is irrelevant when waitMode is 'none'.
});

const res = await client.invalidateWithResult('tenant-123');
// res.state is 'pending' and res.eventId is your reconciliation handle.
```

### Giving the fleet more time

```ts
const client = new HydraClient({
  token: 'sk-tenant-self-service-token',
  nodes: ['http://hydra-1:8080'],
  timeoutMs: 5000, // sends ?timeout_ms=5000
});
```

### The `wait` / `timeout_ms` defaults

| Config | Query parameter sent | Server behaviour |
|---|---|---|
| `waitMode` unset (**default**) | **nothing** | The server's own default, `wait=converged` |
| `waitMode: 'converged'` | `?wait=converged` | Blocks until every live node has applied the event, up to `timeoutMs` |
| `waitMode: 'none'` | `?wait=none` | Publishes and answers `202` immediately; `timeoutMs` has no effect |
| `timeoutMs` unset (**default**) | **nothing** | The server's configured budget, default **2000 ms** |
| `timeoutMs: n` | `&timeout_ms=n` | This request's budget, `n` ∈ `1..60000` |

An unset field is omitted rather than sent as its default, so a caller can tell
"I did not ask" from "I asked for the default". Both are validated **at
construction** instead of being clamped or ignored — the server treats a bad
value as a hard `400 invalid_wait` / `400 invalid_timeout_ms`:

- `waitMode` must be exactly `'converged'` or `'none'`. The server does **not**
  trim the value, so `' converged'` is rejected here rather than remotely.
- `timeoutMs` must be an integer in `1..60000` inclusive
  (`MAX_TIMEOUT_MS = 60_000`, mirroring the server's `MAX_CONVERGE_MS`). Unlike
  Go's zero value, `0` is out of range here; only *unset* means "no parameter".

A violation throws `hydra: invalid waitMode …` / `hydra: invalid timeoutMs …`
from the constructor.

## API

### Types and helpers

- `HydraClientConfig` — client configuration (`token`, `nodes`, timeouts,
  `waitMode`, `timeoutMs`, …).
- `FleetState` — `'applied' | 'single_node' | 'pending' | 'unavailable'`: the
  server's own tokens, exposed verbatim.
- `WaitMode` — `'converged' | 'none'`.
- `isDoneState(state): boolean` — true only for `applied` and `single_node`.
  **Use this, not the HTTP status.** An undecodable state is not done either.
- `isRetryableState(state): boolean` — true for `pending` and `unavailable`.
- `InvalidateTenantAuthCacheResult` — the structured outcome.
- `HTTPError` — a non-fleet-aware HTTP failure (`401`, `413`, `429`, a bare
  `500`, …). Carries `traceId` when the node sent the header.
- `InvalidatePendingError` / `InvalidateUnavailableError` — the two non-done
  outcomes, each carrying `.result`.
- `MIN_TIMEOUT_MS` = `1`, `MAX_TIMEOUT_MS` = `60000`.

### `InvalidateTenantAuthCacheResult` fields

| Field | Meaning |
|---|---|
| `httpStatus` | `200` (applied / single_node), `202` (pending) or `503` (unavailable) |
| `state` | The `fleet.state` token verbatim. `undefined` only when the body could not be decoded into the documented shape — the SDK never guesses a state. |
| `invalidated` | How many entries **this node** removed from its own cache. Not a fleet count. `invalidated === 0 && checked > 0` means those keys were not cached here, which is **not** a failure. |
| `checked` | How many keys you named (`0` for a whole-tenant clear) |
| `scope` | `'keys'` or `'tenant'` |
| `nodesApplied` / `nodesTotal` | Confirmed / live node counts |
| `lagging` | Nodes that did not confirm. **Always an array**, so you can iterate unconditionally. |
| `eventId` | This invalidation's id on the internal bus, for later reconciliation |
| `waitedMs` | How long the server actually spent waiting for the fleet |
| `traceId` | The `X-Hydra-Trace-Id` response header. **Quote it in a support request.** |
| `node` | Base URL of the node that answered |

### Methods

- `new HydraClient(config)` / `new HydraClient(token, nodes)` — create a client.
  Validates `waitMode` and `timeoutMs`.
- `invalidateWithResult(tenantId, apiKeys?): Promise<InvalidateTenantAuthCacheResult>`
  — **the structured call.** `apiKeys` may be omitted or empty for a
  whole-tenant clear. Resolves for `202`/`503`; rejects only when no fleet report
  could be obtained at all (transport failure, non-fleet-aware HTTP error, all
  nodes dead).
- `invalidateTenantAuthCache(tenantId): Promise<void>` — thin wrapper; succeeds
  only for `applied`/`single_node`.
- `invalidateTenantAuthCacheKeys(tenantId, apiKeys): Promise<void>` — thin
  wrapper for api-key scoped invalidation.
- `invalidateCacheKeys(tenantId, apiKeys)` — alias of the above.
- `invalidate(tenantId)` / `invalidateTenantCache(tenantId)` /
  `invalidateCache(tenantId)` — aliases.
- `nodes` / `removedNodes` — active / currently quarantined node base URLs.
- `probeRemovedNodes(): Promise<void>` — manually recheck removed nodes.
- `close(): void` — stop the background rechecker.

## Behaviour change in the existing wrappers

`invalidateTenantAuthCache` (and `invalidateTenantAuthCacheKeys`, and the
aliases) previously resolved for **any** 2xx. It now resolves only when the fleet
state is `applied` or `single_node`:

- A caller that used to see silent success on a `202 pending` now gets an
  `InvalidatePendingError`. The invalidation was accepted but the fleet is not
  converged, so treating it as done was a bug — the previously-banned key could
  still be served by a node that had not applied the event.
- A `503 unavailable` used to be reported as an `HTTPError` and additionally
  caused the node to be quarantined and the call rotated to another node. It is
  now reported as an `InvalidateUnavailableError` and the node stays in the pool:
  it answered, it is alive, and rotating away from it hid the fact that **the
  cluster was never notified**.
- A 2xx whose body does not carry a documented `fleet.state` is no longer success
  either. The SDK refuses to guess a state and throws `hydra: auth cache
  invalidation returned HTTP <status> with unrecognised fleet.state=…`.

If you were relying on "no error means done", migrate to `invalidateWithResult()`
and branch on `isDoneState(res.state)`.

## Failover semantics

| Answer | Quarantine the node? | Rotate to another node? |
|---|---|---|
| Fleet-aware response (`200` applied/single_node, `202` pending, `503` unavailable with a decodable `fleet` object) | **No** — the node answered | **No** — the answer is about the *cluster*, and every data-plane node answers that question the same way |
| `401` / `403` (bad or mismatched tenant token) | No | No — abort immediately; another node will not fix a client-side error |
| Transport failure (connection refused, DNS, …) | Yes | Yes |
| Non-fleet-aware HTTP error (`404`, `405`, bare `5xx`, …) | Yes | Yes |
| Request timeout / caller cancellation | No — the node may simply be slow | Yes (if another node is available) |

## How this contract is verified against a REAL node

The suite in `test/` is a mock suite. `integration/test_sdk_ts_live.py` (CI `integration`
job for the single-node legs, `live-deps` for the cluster legs; also in the local gate,
which rebuilds `dist/` first) drives this SDK against a real gateway and pins:

- the documented usage returns `200 single_node` with a trace id, and the call **really
  clears the node's auth cache** (measured as: allow → the auth service flips to deny →
  the request is still served from cache → SDK invalidate → the next request is `401`);
- another tenant's token is refused as `HTTPError` (never as a "done" state), and the
  pre-2026-09-17 management paths are `404`;
- on a real bus: `200 applied` with `nodesApplied/nodesTotal`, `202 pending` via
  `waitMode: 'none'` (the raising API throws `InvalidatePendingError`, `isDoneState` is
  false), and `503 unavailable` when the bus is cut under a running node (throws
  `InvalidateUnavailableError`). A dead Redis URL cannot produce that last state — the
  node refuses to START — so the drill cuts a TCP relay in front of Redis instead;
- **parity with `tools/hydra-py`**: both SDKs are pointed at the SAME node and must report
  the same `state`, `nodesApplied` and `nodesTotal`. Two implementations of one documented
  contract is exactly where a silent divergence would live, and no single-SDK test can see
  it.
