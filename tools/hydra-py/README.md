# Hydra Python SDK (tenant auth-cache invalidation)

A small Python SDK for Hydra tenants to invalidate their own auth cache through:

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

Requires Python >= 3.9. No third-party runtime dependencies.

## A 2xx is not the same as "done"

This is the most important thing on this page. The contract gives the endpoint
**three** outcomes, and all three tell you to do something different:

| HTTP | `fleet.state` | Meaning | What you must do |
|---|---|---|---|
| `200` | `applied` | Every live node has applied the invalidation | **Done.** Nothing to do. |
| `200` | `single_node` | This node IS the whole data plane | **Done.** Nothing to do. |
| `202` | `pending` | Published, but not every node has confirmed it | **Not done, not an error.** Retry (the call is idempotent), or hand `lagging` to the operator. The banned key may still be served elsewhere until then. |
| `503` | `unavailable` | There is an invalidation channel but it could not do its job, so **the cluster was NOT notified** | **Retry or escalate.** Quote `trace_id`. The node is fine; the fleet was never told. |

The state tokens are the server's own and are exposed verbatim as
`FleetState.APPLIED` / `.SINGLE_NODE` / `.PENDING` / `.UNAVAILABLE`; there is no
mapping layer, because a mapping would be a second owner of the contract.

Two consequences the SDK now enforces:

- **`202 pending` is never reported as success.** `invalidate_tenant_auth_cache`
  raises `InvalidatePendingError` for it, with the full fleet report attached.
- **`503 unavailable` is not a node failure.** The node answered, it is alive,
  and its own cache was cleared — so it is neither quarantined nor rotated away
  from, and the "the cluster was not notified" signal survives as
  `InvalidateUnavailableError`.

Use `result.done` (or `is_done_state(...)`) to decide "done", never the HTTP
status alone.

## Features

- **Structured invalidation result** — `InvalidateTenantAuthCacheResult` carries
  the HTTP status, the fleet state, the node counts, `event_id`, `waited_ms`,
  `lagging` and the `X-Hydra-Trace-Id` header. Nothing is discarded.
- **Correct three-state semantics** — `applied`/`single_node` are success;
  `pending` and `unavailable` are distinguishable, retryable outcomes.
- **`wait` / `timeout_ms` support** — reach the documented bulk mode
  (`wait_mode="none"`) and set your own convergence budget.
- **Trace id on every path** — success (`result.trace_id`) and error
  (`HTTPError.trace_id`) alike, so you can always quote the id the contract
  asks for.
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

## Usage

```python
from hydra_sdk import FleetState, HydraClient

client = HydraClient(
    token="sk-tenant-self-service-token",
    nodes=[
        "http://hydra-1:8080",
        "http://hydra-2:8080",
        "http://hydra-3:8080",
    ],
    probe_timeout=2.0,
    request_timeout=10.0,
    recheck_interval=30.0,
)

# Preferred: branch on the fleet state yourself.
res = client.invalidate_with_result("tenant-123")
if res.done:
    print(f"done: {res.state} ({res.nodes_applied}/{res.nodes_total} nodes, "
          f"event {res.event_id}, {res.waited_ms}ms)")
elif res.state == FleetState.PENDING:
    print(f"still converging: lagging={res.lagging} event={res.event_id} "
          f"trace={res.trace_id} (retry is safe)")
elif res.state == FleetState.UNAVAILABLE:
    print(f"the cluster was NOT notified (trace={res.trace_id}); "
          f"retry or page an operator")
else:
    # A 2xx whose body carried no documented state: the SDK refuses to guess.
    print(f"cannot tell whether the fleet applied it; HTTP {res.http_status}")

client.close()
```

Optional api-key scoped invalidation:

```python
client.invalidate_tenant_auth_cache_keys("tenant-123", ["key-1", "key-2"])
res = client.invalidate_with_result("tenant-123", ["key-1", "key-2"])
```

The error-only wrapper, with the two non-done outcomes told apart:

```python
from hydra_sdk import InvalidatePendingError, InvalidateUnavailableError

try:
    client.invalidate_tenant_auth_cache("tenant-123")
except InvalidatePendingError as exc:
    # 202: the fleet has not confirmed. Retry, or report exc.result.lagging.
    print(f"lagging={exc.result.lagging} trace={exc.result.trace_id}")
except InvalidateUnavailableError as exc:
    # 503: the cluster was NOT notified. Retry or escalate.
    print(f"trace={exc.result.trace_id}")
```

Both subclass a common base, so `except InvalidateOutcomeError` catches either
while `isinstance` still tells them apart. Every message names the state, the
node, the node counts, `event_id` and the trace id.

### Bulk mode: do not wait for the fleet

`wait=none` publishes and answers `202` immediately with the `event_id`, which is
what a bulk/batch script wants. Reconcile afterwards, or simply retry —
invalidation is idempotent (contract §4.5).

Note that `202` is the *normal* outcome here, not a guaranteed one: a node built
without a cluster backbone has no fleet to notify and always answers
`200 single_node`, and in that case `event_id` is `None` because no bus event was
ever published. Branch on `state`, not on the expectation of a status.

```python
client = HydraClient(
    token="sk-tenant-self-service-token",
    nodes=["http://hydra-1:8080"],
    wait_mode="none",  # sends ?wait=none
)
res = client.invalidate_with_result("tenant-123")
# res.state is "pending" and res.event_id is your reconciliation handle.
```

### Giving the fleet more time

```python
client = HydraClient(
    token="sk-tenant-self-service-token",
    nodes=["http://hydra-1:8080"],
    wait_mode="converged",
    timeout_ms=5000,  # sends ?timeout_ms=5000
)
```

## API

### Types

- `HydraClient` (alias `Client`) — the client.
- `FleetState` with `APPLIED`, `SINGLE_NODE`, `PENDING`, `UNAVAILABLE` — the
  server's own tokens, exposed verbatim.
- `WaitMode` with `CONVERGED`, `NONE` — the `wait` values the server accepts.
- `InvalidateTenantAuthCacheResult` — the structured outcome.
- `HTTPError` — a non-fleet-aware HTTP failure (`401`, `403`, `413`, `429`, a
  bare `500`, …), carrying `.trace_id`.
- `InvalidateOutcomeError` — base class of the two non-done outcomes.
- `InvalidatePendingError` / `InvalidateUnavailableError` — the two non-done
  outcomes, each carrying `.result`.

### Helpers

- `is_done_state(state) -> bool` — true only for `applied` and `single_node`.
  **Use this, not the HTTP status.**
- `is_retryable_state(state) -> bool` — true for `pending` and `unavailable`.

### Constants

- `MIN_TIMEOUT_MS = 1`, `MAX_TIMEOUT_MS = 60000` — the server's accepted range
  for `timeout_ms`. Values outside it are rejected by the constructor.

### `HydraClient(token, nodes, *, ...)`

| Keyword argument | Default | Meaning |
|---|---|---|
| `probe_timeout` | `2.0` | Per-`/healthz/leader` probe timeout (seconds) |
| `request_timeout` | `10.0` | Per-invalidation request timeout (seconds) |
| `recheck_interval` | `30.0` | How often quarantined nodes are re-probed |
| `disable_background_recheck` | `False` | Set `True` to drive `probe_removed_nodes()` yourself |
| `urlopen` | `urllib.request.urlopen` | Injection point for tests |
| `wait_mode` | `None` | The `wait` query parameter. `"converged"` or `"none"` — nothing else (the server does not trim this value, so `"converged "` is a `400`) |
| `timeout_ms` | `None` | The `timeout_ms` query parameter: this request's fleet-confirmation budget, an `int` in `1..60000` inclusive |

**`wait_mode` / `timeout_ms` default to `None`, which sends NEITHER parameter.**
The server then applies its own default: `wait=converged` with a **2000 ms**
budget (`HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS`, default 2000 —
`crates/hydra-server/src/tenant_api/mod.rs`). Sending nothing is how you can tell
"I did not ask" from "I asked for the default"; a distinct statement is
`wait_mode="none"`, which means *do not wait at all* and returns `202 pending`
with the `event_id`. A bad value raises `ValueError` locally instead of becoming
a remote `400 invalid_wait` / `400 invalid_timeout_ms`. Note that
`isinstance(True, int)` is `True` in Python, so a `bool` is rejected explicitly.

### `InvalidateTenantAuthCacheResult` fields

| Field | Meaning |
|---|---|
| `http_status` | `200` (applied / single_node), `202` (pending) or `503` (unavailable) |
| `state` | The `fleet.state` token verbatim. `None` only when the body could not be decoded into the documented shape — the SDK never guesses a state |
| `invalidated` | How many entries **this node** removed from its own cache. Not a fleet count. `invalidated == 0` with `checked > 0` means those keys were not cached here, which is **not** a failure |
| `checked` | How many keys you named (`0` for a whole-tenant clear) |
| `scope` | `"keys"` or `"tenant"` |
| `nodes_applied` / `nodes_total` | Confirmed / live node counts |
| `lagging` | Nodes that did not confirm. Always a list, never `None`, so you can always iterate it. Non-empty only for `pending`, or for an `unavailable` whose convergence barrier (rather than whose publish) failed — and even then only when the server actually looked: with `wait=none` nobody looked and every live node is listed, so treat it as "unverified", not "behind" |
| `event_id` | This invalidation's id on the internal bus, for later reconciliation. `None` when the server reported none |
| `waited_ms` | How long the server actually spent waiting for the fleet |
| `trace_id` | The `X-Hydra-Trace-Id` response header. **Quote it in a support request.** `None` when the node sent none |
| `node` | Base URL of the node that answered |
| `done` | Property: `state` is `applied` or `single_node` |
| `retryable` | Property: `state` is `pending` or `unavailable` |

The dataclass is frozen. Fields the server did not report read as `0` / `None` /
`""`; a field present with the *wrong* type makes the whole body undecodable
rather than being coerced.

### Methods

- `invalidate_with_result(tenant_id, api_keys=None) -> InvalidateTenantAuthCacheResult`
  — **the structured call.** `api_keys` may be `None` or empty for a
  whole-tenant clear. Returns a result for `202`/`503`; raises only when no
  fleet report could be obtained at all.
- `invalidate_tenant_auth_cache(tenant_id) -> None` — thin wrapper; succeeds
  only for `applied`/`single_node`.
- `invalidate_tenant_auth_cache_keys(tenant_id, api_keys) -> None` — thin
  wrapper for api-key scoped invalidation.
- `invalidate(tenant_id)` / `invalidate_tenant_cache(tenant_id)` /
  `invalidate_cache(tenant_id)` — aliases.
- `invalidate_cache_keys(tenant_id, api_keys)` — alias.
- `nodes` — active node base URLs.
- `removed_nodes` — currently quarantined node base URLs.
- `probe_removed_nodes()` — manually recheck quarantined nodes.
- `close()` — stop the background rechecker. Also usable as a context manager.

## Behaviour change in the existing wrappers

`invalidate_tenant_auth_cache` (and `invalidate_tenant_auth_cache_keys`, and the
aliases) **previously returned `None` for any 2xx**. It now returns `None` only
when the fleet state is `applied` or `single_node`:

- A caller that used to see silent success on a `202 pending` now gets an
  `InvalidatePendingError`. The invalidation was accepted but the fleet is not
  converged, so treating it as done was a bug — the previously-banned key could
  still be served by a node that had not applied the event.
- A `503 unavailable` used to be reported as an `HTTPError` **and** caused the
  node to be quarantined and the call rotated to another node. It is now
  reported as an `InvalidateUnavailableError` and the node stays in the pool: it
  answered, it is alive, and rotating away from it hid the fact that **the
  cluster was never notified**.
- A 2xx whose body does not carry a documented `fleet.state` is no longer
  success either. The SDK refuses to guess a state and raises
  `InvalidateOutcomeError` (the common base) for it.

If you were relying on "no error means done", migrate to
`invalidate_with_result` and branch on `result.done`.

## Failover semantics

| Answer | Quarantine the node? | Rotate to another node? |
|---|---|---|
| Fleet-aware response (`200` applied/single_node, `202` pending, `503` unavailable with a decodable `fleet` object) | **No** — the node answered | **No** — the answer is about the *cluster*, and every data-plane node answers that question the same way |
| `401` / `403` (bad or mismatched tenant token) | No | No — abort immediately; another node will not fix a client-side error |
| Transport failure (connection refused, DNS, …) | Yes | Yes |
| Non-fleet-aware HTTP error (`404`, `405`, bare `5xx`, …) | Yes | Yes |
| Request timeout | No — the node may simply be slow | Yes (if another node is available) |
| 2xx with an undecodable body | No — the node answered | No — raise `InvalidateOutcomeError` |

## How this contract is verified against a REAL node

`integration/test_sdk_live.py` (CI: the `integration` job for the single-node legs,
the `live-deps` job for the cluster legs; also in the local gate) points this SDK at
a real gateway instead of a mock, and pins:

- the documented usage above returns `200 single_node` with a trace id, and the call
  **really clears the node's auth cache** (measured as: allow → the auth service flips
  to deny → the request is still served from cache → SDK invalidate → the next request
  is `401`);
- the pre-2026-09-17 management paths (`/api/v1/tenants/{id}/auth/cache/invalidate`
  and the tenant-scoped one) are **404**, and pointing the SDK at the admin port fails
  — the endpoint lives on the DATA plane, as this page says;
- on a real bus: `200 applied` with `nodes_applied/nodes_total`, `202 pending` via
  `wait=none` (the raising API raises `InvalidatePendingError`), and `503 unavailable`
  when the bus is cut under a running node (raises `InvalidateUnavailableError`). A
  dead Redis URL cannot produce that last state — the node refuses to START
  (`redis pool initialise` fails), which is why the drill cuts a relay instead;
- an **edge** answers the tenant API locally too: it serves no admin API
  (`/api/v1/health` is 404 there) but the invalidation request reaches the publish
  step, so "answered locally by every data-plane node" holds for edges.
