# Design: Per-Provider Bounded Admission Queue (Concurrency Valve)

**Status:** Approved for implementation (P0 opt-in).
**Author:** Architecture review (@oracle) — 2026-08-09.
**Supersedes / relates to:** `dev-docs/design.md` §rate-limit, §breaker (this fills the concurrency gap those sections do not address).

---

## 0. TL;DR

Hydra today governs **rate-over-time** (sliding window), **health** (circuit breaker), and
**distribution** (SWRR) — but **nothing limits in-flight concurrency** to an upstream. Real LLM
upstreams (except the largest providers) have weak concurrency capacity and choke under modest
parallel load, which is a root cause of the 502/503 storms observed at c≥100 in load tests.

This design adds a **per-provider, opt-in bounded admission queue** (the Envoy
`max_requests` + `max_pending_requests` pattern): when a provider is at its concurrency cap,
incoming requests **wait** in a bounded FIFO queue up to `max_queue_depth`; only when the queue
is also full does the request **fail over** to the next SWRR candidate, or return 503 +
`Retry-After` when all candidates are exhausted.

**Verdict:** DO IT. Industry-standard pattern, fits the existing architecture with no stretch,
**opt-in per provider (`None` ⇒ unchanged behavior)** for a zero-regression rollout.

---

## 1. Current-state map — what governs concurrency today?

**Nothing.** A grep for `concurren|semaphore|inflight|admission|burst` across all `.rs`/`.md`
returns only test scaffolding and graceful-shutdown commentary — never a live concurrency limiter.

| Mechanism                 | File                                                          | What it limits                              | Limits in-flight concurrency?                                            |
| ------------------------- | ------------------------------------------------------------- | ------------------------------------------- | ------------------------------------------------------------------------ |
| Sliding-window rate limit | `hydra-core/src/limit.rs:75-164`, `proxy/limiter.rs:35-37`    | Requests/tokens **per unit time**           | **No** — a window can have 0 admitted-in-60s while 200 are simultaneously in flight. |
| Circuit breaker           | `hydra-core/src/breaker.rs:73-116`, `proxy/breaker_wrap.rs:29-33` | **Consecutive failures** → dead-set         | **No** — health gate, not load gate. A healthy provider with capacity=5 can be handed 500 concurrent requests. |
| SWRR                      | `hydra-core/src/swrr.rs:52-91`                                 | Weighted **candidate ordering**             | **No** — decides *which* provider, never *how many at once*.                |

**Lifecycle / where a slot naturally lives:** the gateway job runs in `request_filter`
(`proxy.rs:207-571`). The failover loop (`proxy.rs:419-534`) is the natural acquire/release site:

- **Acquire:** inside the loop, per-candidate attempt (at `proxy.rs:440`, just before
  `build_request`/`send`).
- **Release:** on scope-exit of whichever iteration succeeds. Because streaming
  (`stream_response`, `proxy.rs:734-803`) holds the upstream connection for the entire SSE
  duration, the permit must live in the outer scope and drop only when `request_filter` returns.

**Failure path today (`proxy.rs:457-525`):**

- `Ok(resp)` + 2xx → `breaker.on_success` (481) → `stream_response` → `Ok(true)`.
- `Ok(resp)` + non-2xx → `breaker.on_failure` (503) → `continue`.
- `Err` (connect/transport) → `breaker.on_failure` (517) → `continue`.
- All candidates exhausted → map last status, write error body, return (539-570).

The breaker is the *only* thing fed on the failure path. Under 100+ in-flight, every request
past the rate-limit pre-gate (340-348) fires at the upstream simultaneously → weak upstreams choke.

---

## 2. Gate placement & granularity

**Recommendation: per-`provider_id`.**

| Option                  | Config volume        | Correctness reasoning                                                                                                                  | Verdict         |
| ----------------------- | -------------------- | -------------------------------------------------------------------------------------------------------------------------------------- | --------------- |
| per-`provider_id`         | O(providers) — small | Upstream concurrency capacity is a **physical property of the host**, not of who's asking.                                                | ✅ **P0**           |
| per-`(provider, tenant)`  | large                | Wrong dimension — the upstream doesn't care about the tenant; the bottleneck is its own pool/worker count.                              | ✗               |
| per-`(provider, model)`   | medium               | Theoretically justified (big model saturates GPU mem, small saturates worker threads) but Hydra has no per-model capacity data and it fragments the pool (HoL under-utilization). | P2, conditional |
| per-`(provider, key)`     | medium               | Only matters if the provider enforces per-key concurrency (some do).                                                                     | P2              |

Envoy, nginx, and every major API gateway key concurrency limits on the upstream cluster/host,
not the downstream tenant. Per-provider also composes cleanly with the existing failover loop
(each candidate already carries `provider_id`) and keeps config trivial.

**Lifecycle placement:** acquire inside the failover loop, per-candidate attempt (`proxy.rs:440`),
so a failover *to* a new candidate re-acquires under that candidate's limit — each provider is
independently protected.

---

## 3. Data structure

**Recommendation: `tokio::sync::Semaphore` + `acquire_owned()` wrapped in `tokio::time::timeout`.**

| Option                                       | Fairness                                        | Eviction-on-disconnect                                              | Complexity                                             | Verdict          |
| -------------------------------------------- | ----------------------------------------------- | ------------------------------------------------------------------- | ------------------------------------------------------ | -------------- |
| **`Arc<Semaphore>` + `acquire_owned()` + `timeout`** | Tokio wait queue is FIFO-fair (waiters woken in order). | Needs explicit `select!` with a disconnect signal; `queue_wait_timeout` is the floor. | Lowest. Permit = RAII handle, releases on drop.       | ✅ **Recommended** |
| Explicit `tokio::sync::mpsc` + worker pool     | FIFO by construction                            | Same problem + a dispatcher task per provider.                       | High — reimplements what Semaphore gives.              | ✗               |
| Custom `VecDeque<Waker>` + `AtomicUsize`         | Tunable                                         | Full control                                                        | Reinventing `Semaphore` poorly.                          | ✗               |

**Why `Semaphore`:**

- Standard Rust primitive for "at most N things hold a resource".
- `acquire_owned()` returns a `SemaphorePermit` that is `'static + Send` — lives in `request_filter`
  across the long streaming call, releases automatically on drop / early-return / `?` / `continue`.
- `tokio::time::timeout(dur, semaphore.acquire_owned())` gives the bounded wait for free.
- Already transitively available (`hydra-server/Cargo.toml`: `tokio = { features = ["full"] }`).

**Fairness:** Tokio's `Semaphore` documents FIFO wake-up ordering — under contention the oldest
request wins the next permit (random would starve long-waiters and inflate tail latency).

**Waiter eviction on client disconnect:** a queued `acquire_owned()` future completes even after
the client hangs up, then wastes a permit on a dead downstream socket. Mitigations ranked:

1. **`queue_wait_timeout_ms`** (P0 floor) — bounds wasted-wait to one timeout window.
2. **`select!` against a disconnect signal** (P1) — Pingora doesn't expose a clean per-request
   cancellation token in terminate mode (only `select!` in the proxy tree is `sink.rs:88`), so this
   needs a `pingora::server::Context`-tied cancellation or periodic liveness check.

---

## 4. What holds a slot — and the streaming question

**The permit is held for the FULL request lifetime** — from just before `provider_client.send()`
(`proxy.rs:455`) through the entire `stream_response` loop (`proxy.rs:787-799`), released on drop
at function exit.

There is no realistic alternative: the upstream is genuinely occupied for the whole SSE stream
(worker thread generating tokens, connection-pool slot taken, concurrent-request budget consumed).
Releasing at TTFT would let Hydra admit N more requests to an upstream still grinding out N active
streams — defeating the entire purpose.

**Consequences (real):**

- Permit hold time = generation time. LLM streams can hold a permit for *minutes*.
- **Memory:** queued requests hold their full `Bytes` body (already read at `proxy.rs:300-316`).
  `max_queue_depth=50` × 32 MiB bodies = up to 1.6 GiB retained *per provider*. The hard cap
  (`max_request_body_hard`, `proxy.rs:307`) bounds each body; the *aggregate* is the concern.
  **`max_queue_depth × max_request_body_hard` is the per-provider RSS ceiling.**
- **Cross-tenant fairness:** a single tenant's burst can exhaust the queue (P1 mitigation: a tiny
  per-tenant reservation, Envoy-style priority levels).

### Should streaming have a separate, larger permit pool?

**No — in P0, do not split.** Reasoning:

1. The upstream doesn't know which pool you drew from. Splitting helps only if streaming consumes
   *less* upstream capacity than non-streaming — for LLMs the opposite is true (streaming holds the
   worker *longer*). A larger streaming pool would *worsen* upstream saturation.
2. Detection requires body parsing (`"stream": true`) — a new JSON concern on the hot path the
   current memchr-based extraction deliberately avoids.
3. Config explosion (two pools × per-provider × tuning guidance).

The correct lever is sizing `max_concurrency` to the upstream's **SSE** capacity, which is the
binding number for LLM upstreams anyway.

**Conditional P2 exception:** if telemetry later shows streaming/non-streaming competing on the
same weak provider with wildly different SLOs, revisit as a priority-level scheme (2 priority
bands sharing one pool, bounded preemption). Not day-one.

---

## 5. Config schema

**Architectural constraint:** `hydra-core/Cargo.toml:11` documents that CI forbids `tokio`/`pingora`/`sqlx`
in core. So the **pure policy data** lives in core; the `Semaphore` wrapper lives in hydra-server.

```rust
// crates/hydra-core/src/model.rs — extend Provider
pub struct Provider {
    // ...existing fields...
    /// Max in-flight requests to this provider. None ⇒ use ProxyConfig default (or unlimited).
    #[serde(default)]
    pub max_concurrency: Option<u32>,
    /// Max requests waiting for a permit. None ⇒ default. 0 ⇒ fail-fast (no queue, 503/failover on cap).
    #[serde(default)]
    pub max_queue_depth: Option<u32>,
    /// Max wait in the queue before failover/503. None ⇒ default.
    #[serde(default)]
    pub queue_wait_timeout_ms: Option<u64>,
}
```

```rust
// crates/hydra-server/src/proxy/config.rs — global defaults
pub struct ConcurrencyPolicy {
    pub default_max_concurrency: u32,       // weak-upstream default: 8
    pub default_max_queue_depth: u32,       // 16
    pub default_queue_wait_timeout_ms: u64, // 2000
}
```

**Resolution at admit time:** `provider.max_concurrency.unwrap_or(policy.default_max_concurrency)`.

**Sane defaults by provider class:**

| Provider class                                   | `max_concurrency`               | `max_queue_depth` | `queue_wait_timeout_ms` |
| ------------------------------------------------ | ------------------------------- | ----------------- | ----------------------- |
| Weak upstream (small lab / OSS model host)       | **4–8**                         | 8–16              | 1000–2000               |
| Mid-tier (Together / Groq / Mistral)             | 16–32                           | 32                | 2000                    |
| Big provider (OpenAI / Anthropic / Azure OpenAI) | 64–128 (or `None` = unlimited)  | 64                | 5000                    |

`None` on every field is valid ("do not gate this provider" — e.g. OpenAI absorbs anything). This
makes the feature **opt-in per provider** — critical for safe rollout (no behavior change for
providers without configured limits).

**Home:** `Provider` is the right place because (a) natural per-provider config row, (b) already
mirrored from DB and hot-reloaded via `ArcSwap` (`ConfigData`), (c) admin UI already edits
per-provider rows. The DB schema gains three nullable columns (migration in P0.4).

---

## 6. Queue-full behavior

**Recommendation: option (c) — try the next SWRR candidate; emit 503 + `Retry-After` only when ALL
candidates' queues are full or the candidate list is exhausted.**

Layers cleanly onto the existing failover loop (`proxy.rs:419-534`):

```
for cand in &candidates {                       // existing loop
    match admission.acquire(cand.provider_id, policy).await {
        Ok(permit) => { /* proceed as today; hold `permit` in scope */ }
        Err(QueueFull)   => { continue }   // NO breaker trip
        Err(WaitTimeout) => { continue }   // NO breaker trip
    }
    // ... build_request / send / stream_response ...
}
// After loop: if every candidate failed with admission error → 503 + Retry-After
```

- **Why not (a) immediate 503:** failover is *already* the recovery mechanism. A 503 when a healthy
  second candidate has capacity is wrong.
- **Why not (b) always-failover without queueing:** that's option (c) with `max_queue_depth=0` —
  expressible via config. Don't bake it in.
- **`Retry-After`:** when all candidates are exhausted, set `Retry-After: <min of all candidate
  queue_wait_timeout_ms>` — a conservative hint that a retry might find capacity (hint, not guarantee).

**Key invariant:** `QueueFull` and `WaitTimeout` must **not** call `breaker.on_failure` (see §7).
They are capacity signals, not health signals — the loop treats them like "candidate unavailable"
(same as a missing api-key, `proxy.rs:430-434`).

---

## 7. Breaker interaction — the critical boundary

**A queue-wait-timeout is NOT an upstream failure and MUST NOT trip the breaker. A real upstream
error after permit acquisition SHOULD.**

The distinction maps onto where in the flow the error originates:

```
                 ┌─ pre-permit (queue) ──────────┐   ─► NO breaker.on_failure
   request ─────►│ wait for permit (timeout)     │
                 └───────────────────────────────┘
                            │ permit acquired
                            ▼
                 ┌─ post-permit (upstream) ──────┐   ─► breaker.on_failure on err
                 │ send → stream                 │     breaker.on_success on 2xx
                 └───────────────────────────────┘
```

| Event                                                | Where (today)            | `breaker.on_failure`?      |
| ---------------------------------------------------- | ------------------------ | -------------------------- |
| `acquire_owned()` returns `Err(AcquireError)` (closed) | NEW, admission module    | **No**                     |
| `timeout(...)` fires before a permit (WaitTimeout)     | NEW                      | **No**                     |
| `QueueFull` (queue at capacity)                        | NEW                      | **No**                     |
| `provider_client.send()` returns `Err`                 | `proxy.rs:515-524`       | **Yes** (already)          |
| Non-2xx response                                       | `proxy.rs:499-513`       | **Yes** (already)          |
| Mid-stream write error                                 | `proxy.rs:483-497`       | No today (no retry, close) — keep |

**Why this matters:** the breaker answers "is this provider dead?". A *busy* provider (queue full)
is not *dead* — five minutes later it may be idle. Conflating capacity with health causes cascading
mis-trips: every burst marks healthy providers dead, the probe task hammers them, they revive, the
next burst re-trips them — oscillation.

**Implementation:** the admission call returns a distinct `AdmissionError` enum that the failover
loop matches *separately* from the `send_result` `Err` arm — no temptation to run them through the
same breaker-touching path.

---

## 8. Precedents

| System                              | Mechanism                                                                                       | Relevant knob                       | What Hydra takes                                                                                                       |
| ----------------------------------- | ----------------------------------------------------------------------------------------------- | ----------------------------------- | --------------------------------------------------------------------------------------------------------------------- |
| **nginx `limit_req`**               | Token-bucket + `burst` queue. `nodelay` serves bursts immediately; `delay=X` holds excess.       | `burst`, `nodelay`/`delay`           | The "bounded queue with optional delay" concept. But nginx rate-limits *over time*, not *in flight* — closer to Hydra's existing `SlidingWindow`. |
| **Envoy circuit breaker**           | Per-cluster priority levels with **`max_requests`** (concurrent) **+ `max_pending_requests`** (queue). | `max_requests`, `max_pending_requests` | **Closest analog — adopt this pattern.** Envoy distinguishes "active" (in-flight) from "pending" (queued) exactly as proposed. |
| **AWS API Gateway / admission ctrl** | Token bucket + concurrency semaphore, returns 429 + `Retry-After`.                               | burst + rate, concurrency            | `Retry-After` semantics + 429-vs-503 split.                                                                            |
| **gRPC `max_concurrent_streams`**    | HTTP/2 SETTINGS frame, hard cap.                                                                 | Per-connection.                     | Less relevant — Hydra's upstream is HTTP/1.1 reqwest.                                                                  |

**Adopt Envoy.** Purpose-built for this problem, well-understood by ops, vocabulary maps directly
onto `max_concurrency` / `max_queue_depth`.

---

## 9. Trade-offs (honest)

**Pros**
- ✅ **Upstream protection** — the core motivation; a weak upstream sees at most `max_concurrency` simultaneous requests.
- ✅ **Burst smoothing** — spikes that today cause 502 storms become bounded queue waits, raising goodput.
- ✅ **Graceful degradation** — when capacity is genuinely exhausted, SWRR failover routes to the next provider.
- ✅ **Observability** — first real visibility into upstream saturation (today: nothing).
- ✅ **Composability** — fits the pure-core / concurrent-shell split with no architectural stretch.

**Cons**
- ❌ **Added tail latency.** A queued request pays `queue_wait_ms` before TTFT. Intended under saturation (slow > 502); under moderate load a queue can form even when a fast attempt would have served fine. Mitigation: generous `default_max_concurrency`, short `queue_wait_timeout_ms` (2s).
- ❌ **Head-of-line blocking.** If candidate A's queue is full and candidate B would serve immediately, the wait happens before failover. Mitigation: small `max_queue_depth` (failover triggers quickly) + the loop's `continue` on `QueueFull`.
- ❌ **Memory of queued bodies.** Each waiter holds its full `Bytes` body; deep queues + large prompts swell RSS. Mitigation: size `max_queue_depth` against `max_request_body_hard`; document the product as the per-provider RSS ceiling.
- ❌ **Client-timeout interaction.** A client with a 30s timeout whose request waits 25s in queue gets 5s of upstream budget. Mitigation: `queue_wait_timeout_ms` ≤ ~10% of typical client timeouts; emit a metric.
- ❌ **Compounding with long SSE holds (the big one).** Permits held for the whole stream means saturated streaming-heavy workloads drain queues *slowly* — each permit frees only when a generation completes (seconds to minutes). **Inherent to LLM gateways** — cannot be designed away; only mitigated by honest sizing of `max_concurrency` to the upstream's real SSE concurrency, and accepting the queue acts as a slow-draining buffer under heavy load.
- ❌ **Cross-tenant fairness.** A single noisy tenant can monopolize the queue. Mitigation (P1): per-tenant reservation (Envoy priority levels).
- ❌ **Client-disconnect-during-wait waste.** A queued request whose client left still consumes a permit when one frees. Mitigation floor: `queue_wait_timeout_ms`; proper fix: `select!` against disconnect (P1).
- ❌ **Single-instance only.** Like the existing `SlidingWindow` and `Breaker`, the semaphore is in-process. Multi-instance Hydra needs Redis or a shared counter (`dev-docs/design.md` §10.4 already calls this out for rate limits). Same constraint, same future workaround.

---

## 10. Metrics

Add to `crates/hydra-server/src/admin/metrics.rs` (existing `Metrics` struct + `OnceLock`
registration, `metrics.rs:49-67`):

| Metric                          | Type      | Labels                                                | Recorded where                                                                        |
| ------------------------------- | --------- | ----------------------------------------------------- | ------------------------------------------------------------------------------------- |
| `hydra_permit_inflight`           | gauge     | `provider`                                              | admission module (set on acquire/release)                                             |
| `hydra_permit_available`          | gauge     | `provider`                                              | admission module (capacity − inflight)                                                |
| `hydra_queue_depth`               | gauge     | `provider`                                              | admission module (current waiters)                                                    |
| `hydra_queue_wait_seconds`        | histogram | `provider`                                              | admission module, on permit-acquired (reuse `LATENCY_BUCKETS`, `metrics.rs:43-45`)      |
| `hydra_queue_drops_total`         | counter   | `provider`, `reason` (`full`, `timeout`, `client_gone`) | admission module, on denied acquire                                                   |
| `hydra_admission_decisions_total` | counter   | `provider`, `outcome` (`acquired`, `queued`, `dropped`) | admission module                                                                      |

Follow the existing `Option<&Metrics>` no-op-on-failure discipline (`metrics.rs:74-164`) so
instrumentation never breaks the hot path.

---

## 11. Implementation plan (P0, ~3–4.5 engineer-days), sliced

### P0.1 — core model + validation + tests  *(0.5 day, lowest risk, do first)*
- Add `max_concurrency: Option<u32>`, `max_queue_depth: Option<u32>`, `queue_wait_timeout_ms: Option<u64>`
  to `Provider` (`model.rs:21-32`) with `#[serde(default)]`.
- Add pure `ConcurrencyPolicy` struct + `resolve_policy(provider, defaults) -> ResolvedPolicy` helper.
- Add validation in `config.rs:140-198` (`validate`): e.g. `max_queue_depth > 0` requires `max_concurrency > 0`;
  `queue_wait_timeout_ms` bounded to a sane max.
- Unit tests in `hydra-core/tests/admission.rs` (policy resolution, defaults, validation errors).
- Handle the `Provider` construction ripple (`db.rs` ProviderRow → Provider sets the new fields to
  `None` until P0.4; `config.example.json` still deserializes via `#[serde(default)]`).

### P0.2 — `admission.rs` shell + unit tests  *(1–1.5 days)*
- New module `crates/hydra-server/src/proxy/admission.rs`:
  - `AdmissionControl: DashMap<String /* provider_id */, Arc<ProviderGate>>`.
  - `ProviderGate { semaphore: Arc<Semaphore>, queue_depth: AtomicUsize, policy: ResolvedPolicy }`.
  - `async fn acquire(&self, provider_id, policy) -> Result<Permit, AdmissionError>` where `Permit`
    is a newtype over `OwnedSemaphorePermit` + an `OnDrop` that decrements `queue_depth`.
  - Body: `queue_depth.fetch_add(1)`; `match timeout(wait_ms, semaphore.acquire_owned()).await { Ok(Ok(p)) => Ok(Permit), Ok(Err(_)) => Err(Closed), Err(_) => Err(WaitTimeout) }`;
    early `QueueFull` check when `queue_depth >= max_queue_depth`.
- Unit tests: timeout, queue-full, fairness (N+1 vs N), permit release on drop, concurrent stress.

### P0.3 — failover-loop wiring + 503/Retry-After  *(0.5 day)*
- Before `build_request` (`proxy.rs:440`), call `admission.acquire(...)`. Hold permit in scope through
  `send` + `stream_response`. On `Err(QueueFull | WaitTimeout)`, `continue` **without**
  `breaker.on_failure`; bump `hydra_queue_drops_total`.
- After loop: if every failure was an admission error → emit 503 + `Retry-After: <min queue_wait_timeout_ms>`.

### P0.4 — config plumbing (migration + loader)  *(0.5 day)*
- Migration `0004_provider_concurrency.sql`: add nullable `max_concurrency`, `max_queue_depth`,
  `queue_wait_timeout_ms` columns to `provider`. Re-run `cargo sqlx prepare` to refresh `.sqlx/`.
- Wire columns through `ProviderRow` → `Provider` (replace the `None` placeholders from P0.1).
- Admin API + admin UI expose the three fields (nullable).

### P0.5 — metrics  *(0.5 day)*
- The six metrics in §10, following the existing `OnceLock` / no-op pattern.

### P0.6 — integration tests  *(0.5–1 day)*
- Extend `crates/hydra-server/tests/load_breaker_swrr.rs` with a concurrency-saturation scenario:
  fire N+1 requests against `max_concurrency=N` mock upstream; assert the (N+1)th waits, then
  completes after a release. Assert failover-on-queue-full. Assert no breaker trip on queue timeout.

### P1 (after P0 stabilizes)
- Client-disconnect cancellation in the queue wait (`select!` against a Pingora-side cancellation).
- Per-tenant queue reservation (Envoy priority levels) for cross-tenant fairness.
- Admin API `/api/v1/concurrency` GET to inspect live `inflight` / `queue_depth` per provider.

### P2 (conditional, defer)
- Per-`(provider, model)` granularity if telemetry shows divergent model capacities.
- Per-`(provider, key)` if a provider enforces per-key limits.
- Redis-backed shared counter for multi-instance deployments.

---

## 12. Layering into the failover loop (sequence diagram)

```
 client request
      │
      ▼
 ┌──────────────────────────────────────────────────────────────┐
 │ request_filter (proxy.rs:207)                                │
 │  tenant, auth, body read, model extract, rate-limit pre-gate │
 │  router::resolve ─► candidates [pA, pB, pC] (SWRR-ordered)   │
 └──────────────────────────────────────────────────────────────┘
      │
      ▼  for cand in candidates:        ── existing loop (proxy.rs:419)
 ┌──────────────────────────────────────────────────────────────┐
 │ admission.acquire(cand.provider_id)            ◄── NEW       │
 │   ├─ Ok(permit) ─────────────────────────────┐               │
 │   ├─ Err(QueueFull)   ─► continue ────────────┐ (no breaker) │
 │   └─ Err(WaitTimeout) ─► continue ────────────┘              │
 │                                               │              │
 │   build_request + send (proxy.rs:440-455)     ▼              │
 │      │  ┌─────────────────────────────────────────────────┐  │
 │      │  │ send_result                                       │  │
 │      │  │  Ok(2xx) ─► breaker.on_success ─► stream_response │ │
 │      │  │              │              (holds permit!)      │ │
 │      │  │              │              ...SSE chunks...     │ │
 │      │  │              ▼                                    │ │
 │      │  │          return Ok(true) ◄── permit drops ───────┘ │
 │      │  │  Ok(non-2xx) ─► breaker.on_failure ─► continue      │
 │      │  │  Err(transport) ─► breaker.on_failure ─► continue   │
 │      │  └─────────────────────────────────────────────────┘  │
 │      │     ▲                                                  │
 │      └─────┘ permit released on each continue via drop        │
 └──────────────────────────────────────────────────────────────┘
      │
      ▼  all candidates exhausted
 ┌──────────────────────────────────────────────────────────────┐
 │ if last failure was admission error:                          │
 │   503 + Retry-After: <min queue_wait_timeout_ms>              │
 │ else (real upstream error): existing behavior (proxy.rs:539)  │
 └──────────────────────────────────────────────────────────────┘
```

The two distinct non-trip paths (admission) vs trip paths (upstream errors) make the §7 boundary visual.

---

## 13. Top 3 risks & mitigations

| #   | Risk                                                                                          | Mitigation                                                                                                                                                                  |
| --- | --------------------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **1**   | **Default-on rollout accidentally gates providers that were working fine → regressions.**        | Every field `Option<>`, `None` ⇒ no-op. Ship with **no defaults applied unless explicitly configured**. Document "set `max_concurrency` only for providers you've seen oversubscribe." |
| **2**   | **Long SSE streams drain the queue glacially → queue feels useless under streaming load** (§4/§9). | (a) Honest ops docs: queue is a slow-draining buffer for streaming, not magic smoothness. (b) Size `max_concurrency` to the upstream's *measured* SSE concurrency via load tests. (c) Short `queue_wait_timeout_ms` so requests fail over rather than rot in queue. |
| **3**   | **Permit leak on a code path that forgets to drop** (e.g. a `return Ok(true)` added between acquire and scope end). | RAII via `OwnedSemaphorePermit` held in a single local binding in the loop body — every `return`/`?`/`continue` drops it. Integration test (P0.6): fire N+1 vs `max_concurrency=N`, assert the (N+1)th waits then completes — a leaked permit hangs the test. |

---

## 14. Verdict & first commit

**DO IT (P0, opt-in per provider).** Fills the only remaining gap in Hydra's traffic-shaping story
(rate-over-time ✓, health ✓, distribution ✓, **concurrency ✗** → ✓), adopts the industry-standard
Envoy pattern, composes cleanly with the existing architecture and SWRR failover loop, and the
opt-in `Option`-field design means zero behavior change for unconfigured providers.

**First commit (lowest risk, highest value):** P0.1 — add the three `Option` fields to `Provider` in
`crates/hydra-core/src/model.rs:21-32` plus the pure `ConcurrencyPolicy` resolution helper and its
unit tests in `crates/hydra-core/tests/`. Independently testable, touches no hot path.
