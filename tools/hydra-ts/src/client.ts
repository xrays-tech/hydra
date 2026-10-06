/**
 * Cluster-aware Hydra tenant SDK.
 *
 * The client accepts one or more Hydra **data-plane** node base URLs (the
 * address that serves `/v1/*`, default port 8080 — not the admin port 8081).
 *
 * The invalidation endpoint is answered **locally by every data-plane node**: it
 * clears that node's cache synchronously and fans the invalidation out over the
 * shared bus, so it does NOT need to reach the cluster leader (design
 * `dev-docs/design-tenant-api.md` §6.4 decision A-1). Leader preference via the
 * `/healthz/leader` probe is therefore a legacy optimization, not a requirement.
 *
 * Before each invalidation the client still probes `/healthz/leader` and tries
 * nodes that report themselves leader first. That probe is an **admin-port**
 * route: a node list pointing at data-plane ports gets a 404, which counts as
 * "alive, not the leader", so the request is sent directly. If the chosen node
 * fails, the client automatically rotates to the next available node and
 * temporarily removes the dead node from the active pool. A background timer
 * periodically probes removed nodes and adds them back once they become
 * reachable again.
 *
 * # The endpoint's three-state contract
 *
 * A 2xx is NOT the same as "done". Per `dev-docs/tenant-api-integration.md` §5.2
 * the fleet state in the response body is what the caller must branch on, and
 * {@link InvalidateTenantAuthCacheResult} surfaces it verbatim:
 *
 * | HTTP | `fleet.state` | meaning                              | caller must                      |
 * |------|---------------|--------------------------------------|----------------------------------|
 * | 200  | `applied`     | every live node applied it           | done                             |
 * | 200  | `single_node` | this node IS the whole data plane    | done                             |
 * | 202  | `pending`     | published, not confirmed everywhere  | retry (idempotent) / report lagging |
 * | 503  | `unavailable` | the cluster was NOT notified         | retry / escalate, quoting traceId |
 *
 * Two consequences worth stating because the old behaviour got both wrong:
 *
 * - `202 pending` is a *retryable* outcome, never silent success. The wrapper
 *   methods return an error for it ({@link InvalidatePendingError}); callers
 *   that want to branch on it themselves should call
 *   {@link HydraClient.invalidateWithResult}.
 * - `503 unavailable` is NOT a node failure. The node answered — it is alive and
 *   its own cache was cleared — so it is neither quarantined nor rotated away
 *   from, and the "the cluster was not notified" signal is preserved as
 *   {@link InvalidateUnavailableError}. Only transport failures and
 *   non-fleet-aware server errors still trigger failover.
 */

/**
 * Largest accepted `timeout_ms`, mirroring the server's own ceiling
 * (`crates/hydra-server/src/tenant_api/handlers.rs`: `MAX_CONVERGE_MS = 60_000`).
 * A larger value would be a remote `400 invalid_timeout_ms`, so it is refused
 * here instead.
 */
export const MAX_TIMEOUT_MS = 60_000;

/** Smallest accepted `timeout_ms` (0 would mean "do not wait" while claiming to). */
export const MIN_TIMEOUT_MS = 1;

/**
 * The `wait` query parameter (tenant API §5.2).
 *
 * `converged` blocks until every live node has applied the event; `none`
 * publishes and answers `202` immediately, which is the bulk/script mode. The
 * server accepts nothing else — and it does NOT trim the value, so
 * `' converged'` is a `400 invalid_wait` rather than a default.
 */
export type WaitMode = 'converged' | 'none';

/**
 * A `fleet.state` token from the invalidation response body.
 *
 * These are the server's own four tokens (`crates/hydra-server/src/cluster/
 * events.rs`, `FleetReport.state`) exposed verbatim. There is deliberately no
 * mapping layer: a mapping is a second owner of the contract, and two owners
 * always eventually disagree.
 */
export type FleetState = 'applied' | 'single_node' | 'pending' | 'unavailable';

/**
 * Reports whether a fleet state means the invalidation is actually complete:
 * only `applied` and `single_node` qualify. `pending` is in flight and
 * `unavailable` never reached the fleet. An unknown state (the body could not be
 * decoded) is not done either — an unreadable answer is never "success".
 */
export function isDoneState(state: FleetState | undefined): boolean {
  return state === 'applied' || state === 'single_node';
}

/**
 * Reports whether the caller should try again. `pending` and `unavailable` are
 * both retryable — the call is idempotent (contract §4.5) — while a done state
 * has nothing left to retry.
 */
export function isRetryableState(state: FleetState | undefined): boolean {
  return state === 'pending' || state === 'unavailable';
}

export interface HydraClientConfig {
  /** Tenant self-service access token sent as Authorization: Bearer <token>. */
  token: string;
  /**
   * Hydra **data-plane** node base URLs — the same address clients use for
   * `/v1/*`, default port 8080, e.g. "http://127.0.0.1:8080". Not the admin
   * port 8081.
   */
  nodes: string[];
  /** Optional fetch implementation (defaults to global fetch). */
  fetchImpl?: typeof fetch;
  /** Per /healthz/leader probe timeout in milliseconds. Default 2000. */
  probeTimeoutMs?: number;
  /** Per invalidation request timeout in milliseconds. Default 10000. */
  requestTimeoutMs?: number;
  /** Interval for rechecking removed nodes in milliseconds. Default 30000. */
  recheckIntervalMs?: number;
  /** Disable the automatic background rechecker. Default false. */
  disableBackgroundRecheck?: boolean;
  /**
   * The `wait` query parameter sent on every invalidation request. Accepts
   * exactly `'converged'` or `'none'`; any other value is refused by the
   * constructor rather than silently ignored (the server treats a bad value as a
   * hard `400 invalid_wait`, and it does not trim it).
   *
   * Leaving it **unset** (the default) sends NO `wait` parameter at all, so the
   * server applies its own default, which is `converged` — i.e. the request
   * blocks for up to `timeoutMs` (or the server's configured budget, default
   * 2000 ms) waiting for the whole fleet to confirm.
   */
  waitMode?: WaitMode;
  /**
   * The `timeout_ms` query parameter: this request's budget in milliseconds for
   * waiting on fleet confirmation. Must be an integer in
   * [{@link MIN_TIMEOUT_MS}, {@link MAX_TIMEOUT_MS}] (1..60000) when set.
   *
   * Leaving it **unset** (the default) sends NO `timeout_ms` parameter, so the
   * server uses its configured budget, which defaults to 2000 ms. Note the
   * difference between "unset" (server default) and `waitMode: 'none'` (do not
   * wait at all): the latter is a statement about behaviour, the former is not.
   * Unlike Go's zero value there is no in-band "unset" integer here, so `0` is
   * out of range and rejected.
   */
  timeoutMs?: number;
}

/**
 * Returned when the server responds with a non-2xx status that is NOT one of the
 * documented invalidate outcomes.
 *
 * Note that `503 unavailable` on the invalidate endpoint does NOT produce one:
 * that answer carries a normal fleet body, so it is reported as a result (or as
 * {@link InvalidateUnavailableError} by the wrappers). A `503 not_ready` or a
 * `500` still does.
 */
export class HTTPError extends Error {
  readonly method: string;
  readonly url: string;
  readonly status: number;
  readonly statusText: string;
  readonly body: string;
  /**
   * The `X-Hydra-Trace-Id` response header, carried on the error itself because
   * the tenant contract (§4.2) tells callers to quote it when reporting a
   * problem — and an HTTP error is a problem.
   */
  readonly traceId?: string;

  constructor(
    method: string,
    url: string,
    status: number,
    statusText: string,
    body: string,
    traceId?: string,
  ) {
    const excerpt = body.trim().slice(0, 300);
    const trace = traceId ? ` (X-Hydra-Trace-Id: ${traceId})` : '';
    super(
      excerpt
        ? `${method} ${url}: unexpected HTTP ${status} ${statusText}: ${excerpt}${trace}`
        : `${method} ${url}: unexpected HTTP ${status} ${statusText}${trace}`,
    );
    this.name = 'HTTPError';
    this.method = method;
    this.url = url;
    this.status = status;
    this.statusText = statusText;
    this.body = body;
    this.traceId = traceId;
  }
}

/**
 * The full outcome of one invalidation.
 *
 * It is deliberately not a boolean: the endpoint has a three-state contract and
 * every one of the three tells the caller to do something different.
 */
export interface InvalidateTenantAuthCacheResult {
  /** The status the winning node returned: 200 (applied / single_node), 202 (pending) or 503 (unavailable). */
  httpStatus: number;
  /**
   * The fleet state, verbatim from the response body. `undefined` only when the
   * body could not be decoded into the documented shape — this SDK never guesses
   * a state, because guessing is how a response nobody understood becomes "done".
   */
  state: FleetState | undefined;
  /**
   * How many entries THIS node removed from its own cache. Not a fleet count.
   * `invalidated === 0 && checked > 0` means those keys were not cached on this
   * node, which is not a failure.
   */
  invalidated: number;
  /** How many keys the request named (0 for a whole-tenant clear). */
  checked: number;
  /** `'keys'` or `'tenant'`; `''` when the body could not be decoded. */
  scope: string;
  /** Confirmed node count. */
  nodesApplied: number;
  /** Live node count. */
  nodesTotal: number;
  /** The nodes that did not confirm — always an array, never undefined. */
  lagging: string[];
  /** This invalidation's id on the internal bus, for later reconciliation. */
  eventId: string | undefined;
  /** How long the server actually spent waiting for the fleet. */
  waitedMs: number | undefined;
  /**
   * The `X-Hydra-Trace-Id` response header — quote it in a support request
   * (§4.2). `undefined` when the node did not send one.
   */
  traceId: string | undefined;
  /**
   * The base URL of the node that answered. `traceId` plus `node` is what makes
   * a partial failure diagnosable.
   */
  node: string;
}

/**
 * Reports HTTP 202 `pending`: the invalidation was published but the whole fleet
 * has not confirmed it. It is NOT a failure of the request — it is an incomplete
 * outcome, and the caller must retry or hand the lagging nodes to the operator.
 *
 * It exists as a distinct class so that callers who previously treated "no error"
 * as "done" can no longer do so by accident, while still being able to tell it
 * apart from a hard error (and from {@link InvalidateUnavailableError}).
 */
export class InvalidatePendingError extends Error {
  readonly result: InvalidateTenantAuthCacheResult;
  /**
   * Discriminant, always `'pending'`: it mirrors `result.state`, and — because
   * the two non-done classes would otherwise be structurally identical — it is
   * what lets TypeScript narrow an `instanceof` check instead of collapsing it.
   */
  readonly state: 'pending' = 'pending';

  constructor(result: InvalidateTenantAuthCacheResult) {
    super(
      'hydra: auth cache invalidation is pending, not complete: ' +
        `node ${result.node} answered HTTP ${result.httpStatus} fleet.state=${JSON.stringify(result.state)} ` +
        `(${result.nodesApplied}/${result.nodesTotal} nodes applied, awaited ${String(result.waitedMs)}ms, ` +
        `event_id ${JSON.stringify(result.eventId)}, trace_id ${JSON.stringify(result.traceId)})`,
    );
    this.name = 'InvalidatePendingError';
    this.result = result;
  }
}

/**
 * Reports HTTP 503 `unavailable`: an invalidation channel exists on that node but
 * could not do its job, so THE CLUSTER WAS NOT NOTIFIED. The node itself
 * answered, which is why this is not a node failure: the node is not quarantined
 * and the client does NOT rotate away from it.
 */
export class InvalidateUnavailableError extends Error {
  readonly result: InvalidateTenantAuthCacheResult;
  /** Discriminant, always `'unavailable'`; see {@link InvalidatePendingError.state}. */
  readonly state: 'unavailable' = 'unavailable';

  constructor(result: InvalidateTenantAuthCacheResult) {
    super(
      'hydra: auth cache invalidation is unavailable: ' +
        `node ${result.node} answered HTTP ${result.httpStatus} fleet.state=${JSON.stringify(result.state)}, ` +
        `so the cluster was NOT notified (${result.nodesApplied}/${result.nodesTotal} nodes applied, ` +
        `awaited ${String(result.waitedMs)}ms, event_id ${JSON.stringify(result.eventId)}, ` +
        `trace_id ${JSON.stringify(result.traceId)}); retry or escalate to an operator`,
    );
    this.name = 'InvalidateUnavailableError';
    this.result = result;
  }
}

/**
 * Reports whether err is an {@link InvalidatePendingError}. Provided alongside
 * `instanceof` for callers that prefer a function, and so the two non-done
 * outcomes are trivially distinguishable.
 */
export function isInvalidatePendingError(err: unknown): err is InvalidatePendingError {
  return err instanceof InvalidatePendingError;
}

/** Reports whether err is an {@link InvalidateUnavailableError}. */
export function isInvalidateUnavailableError(err: unknown): err is InvalidateUnavailableError {
  return err instanceof InvalidateUnavailableError;
}

interface RawResponse {
  status: number;
  statusText: string;
  body: string;
  /** Response headers, so `X-Hydra-Trace-Id` can be captured on every path. */
  headers: Headers | undefined;
}

/** The subset of the response body this SDK relies on. */
interface FleetOutcome {
  state: FleetState;
  invalidated: number;
  checked: number;
  scope: string;
  nodesApplied: number;
  nodesTotal: number;
  lagging: string[];
  eventId: string | undefined;
  waitedMs: number | undefined;
}

type ConstructorArg = HydraClientConfig | string;

/**
 * Reports whether err is a request timeout / caller-cancellation error. In
 * this SDK such errors surface as an AbortError raised by the fetch
 * implementation once the client's AbortController is triggered. A timeout is
 * not evidence that a node is dead (it may simply be slow), so it must not
 * cause the node to be quarantined.
 */
function isTimeoutError(err: unknown): boolean {
  if (typeof err !== 'object' || err === null) return false;
  return (err as { name?: unknown }).name === 'AbortError';
}

/**
 * Reads one response header, tolerating a fetch implementation whose Response
 * object is a minimal stub without a `headers` map.
 */
function readHeader(headers: Headers | undefined, name: string): string | undefined {
  if (!headers || typeof headers.get !== 'function') return undefined;
  const value = headers.get(name);
  if (value === null || value === undefined) return undefined;
  return value;
}

/** Reads a documented integer field: wrong JSON type ⇒ the body is not decodable. */
function readInteger(value: unknown): number | null | { error: true } {
  if (value === undefined || value === null) return null;
  if (typeof value !== 'number' || !Number.isInteger(value)) return { error: true };
  return value;
}

/** Reads a documented string field: wrong JSON type ⇒ the body is not decodable. */
function readString(value: unknown): string | null | { error: true } {
  if (value === undefined || value === null) return null;
  if (typeof value !== 'string') return { error: true };
  return value;
}

function isDecodeError(value: unknown): value is { error: true } {
  return typeof value === 'object' && value !== null && 'error' in value;
}

/**
 * Decodes the documented invalidate response envelope.
 *
 * Returns `undefined` when the body is not JSON, has no `fleet` object, carries a
 * state outside the documented four, or types a documented field in a way the
 * contract does not allow. Callers then treat the answer as undecodable rather
 * than guessing a state.
 */
function parseFleetOutcome(body: string): FleetOutcome | undefined {
  let decoded: unknown;
  try {
    decoded = JSON.parse(body);
  } catch {
    return undefined;
  }
  if (typeof decoded !== 'object' || decoded === null || Array.isArray(decoded)) {
    return undefined;
  }
  const root = decoded as Record<string, unknown>;
  const rawFleet = root['fleet'];
  if (typeof rawFleet !== 'object' || rawFleet === null || Array.isArray(rawFleet)) {
    return undefined;
  }
  const fleet = rawFleet as Record<string, unknown>;

  const state = fleet['state'];
  if (
    state !== 'applied' &&
    state !== 'single_node' &&
    state !== 'pending' &&
    state !== 'unavailable'
  ) {
    return undefined;
  }

  const invalidated = readInteger(root['invalidated']);
  const checked = readInteger(root['checked']);
  const scope = readString(root['scope']);
  const nodesApplied = readInteger(fleet['nodes_applied']);
  const nodesTotal = readInteger(fleet['nodes_total']);
  const eventId = readString(fleet['event_id']);
  const waitedMs = readInteger(fleet['waited_ms']);
  if (
    isDecodeError(invalidated) ||
    isDecodeError(checked) ||
    isDecodeError(scope) ||
    isDecodeError(nodesApplied) ||
    isDecodeError(nodesTotal) ||
    isDecodeError(eventId) ||
    isDecodeError(waitedMs)
  ) {
    return undefined;
  }

  // `lagging` is a JSON array in the contract; a missing or null value reads as
  // "none". Normalised to an array so callers can iterate unconditionally.
  const rawLagging = fleet['lagging'];
  let lagging: string[] = [];
  if (rawLagging !== undefined && rawLagging !== null) {
    if (!Array.isArray(rawLagging) || rawLagging.some((item) => typeof item !== 'string')) {
      return undefined;
    }
    lagging = rawLagging as string[];
  }

  return {
    state,
    invalidated: invalidated ?? 0,
    checked: checked ?? 0,
    scope: scope ?? '',
    nodesApplied: nodesApplied ?? 0,
    nodesTotal: nodesTotal ?? 0,
    lagging,
    eventId: eventId ?? undefined,
    waitedMs: waitedMs ?? undefined,
  };
}

/**
 * Builds the empty result for a node that answered with `httpStatus`: the
 * undecoded shape the wrappers refuse to call success.
 */
function undecodedResult(
  httpStatus: number,
  node: string,
  traceId: string | undefined,
): InvalidateTenantAuthCacheResult {
  return {
    httpStatus,
    state: undefined,
    invalidated: 0,
    checked: 0,
    scope: '',
    nodesApplied: 0,
    nodesTotal: 0,
    lagging: [],
    eventId: undefined,
    waitedMs: undefined,
    traceId,
    node,
  };
}

export class HydraClient {
  private readonly token: string;
  private readonly fetchImpl: typeof fetch;
  private readonly probeTimeoutMs: number;
  private readonly requestTimeoutMs: number;
  private readonly recheckIntervalMs: number;
  private readonly waitMode: WaitMode | undefined;
  private readonly timeoutMs: number | undefined;
  private active: string[];
  private removed: string[];
  private timer?: ReturnType<typeof setInterval>;

  constructor(config: HydraClientConfig);
  constructor(token: string, nodes: string[]);
  constructor(configOrToken: ConstructorArg, nodes?: string[]) {
    let config: HydraClientConfig;
    if (typeof configOrToken === 'string') {
      if (!nodes || nodes.length === 0) {
        throw new Error('hydra: at least one node is required');
      }
      config = { token: configOrToken, nodes };
    } else {
      config = configOrToken;
    }

    if (!config.token?.trim()) {
      throw new Error('hydra: token is required');
    }
    if (!config.nodes || config.nodes.length === 0) {
      throw new Error('hydra: at least one node is required');
    }

    // The wait/timeout_ms pair is validated rather than clamped or ignored: the
    // server treats a bad value as a hard `400`, and silently substituting a
    // default would leave the caller believing it had asked for (or skipped) a
    // wait it never got.
    if (config.waitMode !== undefined && config.waitMode !== 'converged' && config.waitMode !== 'none') {
      throw new Error(
        `hydra: invalid waitMode ${JSON.stringify(config.waitMode)} (want "converged" or "none")`,
      );
    }
    if (
      config.timeoutMs !== undefined &&
      (typeof config.timeoutMs !== 'number' ||
        !Number.isInteger(config.timeoutMs) ||
        config.timeoutMs < MIN_TIMEOUT_MS ||
        config.timeoutMs > MAX_TIMEOUT_MS)
    ) {
      throw new Error(
        `hydra: invalid timeoutMs ${JSON.stringify(config.timeoutMs)} ` +
          `(want an integer in ${MIN_TIMEOUT_MS}..${MAX_TIMEOUT_MS}, or leave it unset to use the server default)`,
      );
    }

    this.token = config.token.trim();
    this.fetchImpl = config.fetchImpl ?? fetch;
    this.probeTimeoutMs = config.probeTimeoutMs ?? 2000;
    this.requestTimeoutMs = config.requestTimeoutMs ?? 10000;
    this.recheckIntervalMs = config.recheckIntervalMs ?? 30000;
    this.waitMode = config.waitMode;
    this.timeoutMs = config.timeoutMs;

    const seen = new Set<string>();
    this.active = [];
    for (const raw of config.nodes) {
      const node = raw.trim().replace(/\/+$/, '');
      if (!node || seen.has(node)) continue;
      let u: URL;
      try {
        u = new URL(node);
      } catch {
        // A bare TypeError from new URL() is not user-friendly; surface the
        // offending node the same way the protocol check below does.
        throw new Error(`hydra: invalid node URL "${node}"`);
      }
      if (u.protocol !== 'http:' && u.protocol !== 'https:') {
        throw new Error(`hydra: invalid node URL "${node}"`);
      }
      seen.add(node);
      this.active.push(node);
    }
    if (this.active.length === 0) {
      throw new Error('hydra: no valid node URLs');
    }

    this.removed = [];

    if (!config.disableBackgroundRecheck) {
      this.timer = setInterval(() => {
        void this.probeRemovedNodes();
      }, this.recheckIntervalMs);
      if (typeof this.timer.unref === 'function') {
        this.timer.unref();
      }
    }
  }

  get nodes(): string[] {
    return [...this.active];
  }

  get removedNodes(): string[] {
    return [...this.removed];
  }

  close(): void {
    if (this.timer) {
      clearInterval(this.timer);
      this.timer = undefined;
    }
  }

  /**
   * Invalidates the tenant's auth cache and returns the full, structured
   * outcome.
   *
   * Prefer this method: it is the only one that can report the endpoint's middle
   * state. Use it when you need to branch on the fleet state, to reconcile later
   * with `eventId`, or to quote `traceId` in a support request.
   *
   * It sends `POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate` to a
   * data-plane node, preferring any node that reports itself leader, and fails
   * over to other nodes when the chosen node is unreachable or returns a
   * non-fleet-aware server error.
   *
   * Unlike the thin wrappers below it never turns a non-done outcome into an
   * error: `202 pending` and `503 unavailable` come back as results. A rejected
   * promise means the request itself could not produce a fleet report at all
   * (transport failure, non-fleet-aware HTTP error, or every node dead).
   */
  async invalidateWithResult(
    tenantId: string,
    apiKeys?: string[],
  ): Promise<InvalidateTenantAuthCacheResult> {
    const body = apiKeys && apiKeys.length > 0 ? { api_keys: apiKeys } : undefined;
    return this.invalidateInternal(tenantId, body);
  }

  /**
   * Invalidates the tenant's auth cache, failing unless the fleet actually
   * converged.
   *
   * Thin wrapper over {@link HydraClient.invalidateWithResult}: it returns
   * successfully only for `applied` / `single_node` — a 2xx is NOT sufficient.
   * Use `instanceof` (or {@link isInvalidatePendingError} /
   * {@link isInvalidateUnavailableError}) to tell the two non-done outcomes
   * apart, or call {@link HydraClient.invalidateWithResult} directly to branch on
   * the state.
   */
  async invalidateTenantAuthCache(tenantId: string): Promise<void> {
    await this.invalidateAndRequireDone(tenantId, undefined);
  }

  /**
   * Invalidates only the supplied api-keys for the tenant. When apiKeys is empty
   * this is the same as {@link HydraClient.invalidateTenantAuthCache}; success
   * likewise means the fleet state was `applied` or `single_node`.
   */
  async invalidateTenantAuthCacheKeys(tenantId: string, apiKeys: string[]): Promise<void> {
    if (apiKeys.length === 0) {
      await this.invalidateTenantAuthCache(tenantId);
      return;
    }
    await this.invalidateAndRequireDone(tenantId, { api_keys: apiKeys });
  }

  async invalidate(tenantId: string): Promise<void> {
    await this.invalidateTenantAuthCache(tenantId);
  }

  async invalidateTenantCache(tenantId: string): Promise<void> {
    await this.invalidateTenantAuthCache(tenantId);
  }

  async invalidateCache(tenantId: string): Promise<void> {
    await this.invalidateTenantAuthCache(tenantId);
  }

  async invalidateCacheKeys(tenantId: string, apiKeys: string[]): Promise<void> {
    await this.invalidateTenantAuthCacheKeys(tenantId, apiKeys);
  }

  async probeRemovedNodes(): Promise<void> {
    const removed = [...this.removed];
    if (removed.length === 0) return;

    const restored: string[] = [];
    for (const node of removed) {
      const result = await this.probeLeader(node);
      if (result.alive) restored.push(node);
    }
    if (restored.length === 0) return;

    for (const node of restored) {
      if (!this.active.includes(node)) this.active.push(node);
      this.removed = this.removed.filter((item) => item !== node);
    }
  }

  /**
   * Adapts a structured result to the wrapper methods' error-only signature:
   * success only for a state that actually means "done", with the two
   * distinguishable non-done outcomes surfaced as their own error classes.
   */
  private async invalidateAndRequireDone(
    tenantId: string,
    body: Record<string, unknown> | undefined,
  ): Promise<void> {
    const result = await this.invalidateInternal(tenantId, body);
    if (isDoneState(result.state)) return;
    switch (result.state) {
      case 'pending':
        throw new InvalidatePendingError(result);
      case 'unavailable':
        throw new InvalidateUnavailableError(result);
      default:
        // A 2xx whose body did not carry a documented fleet state: we cannot
        // claim the invalidation is complete, so we must not report success.
        throw new Error(
          `hydra: auth cache invalidation returned HTTP ${result.httpStatus} with unrecognised ` +
            `fleet.state=${JSON.stringify(result.state)} (node ${result.node}, ` +
            `trace_id ${JSON.stringify(result.traceId)})`,
        );
    }
  }

  private async invalidateInternal(
    tenantId: string,
    body: Record<string, unknown> | undefined,
  ): Promise<InvalidateTenantAuthCacheResult> {
    if (!tenantId?.trim()) {
      throw new Error('hydra: tenantID is required');
    }
    tenantId = tenantId.trim();

    const nodes = [...this.active];
    if (nodes.length === 0) {
      throw new Error(`hydra: no available nodes (removed: ${this.removedNodes.join(', ')})`);
    }

    const leaders: string[] = [];
    const alive: string[] = [];
    const seen = new Set<string>();
    for (const node of nodes) {
      const result = await this.probeLeader(node);
      if (!result.alive) {
        // Only a transport failure quarantines a node. An HTTP answer
        // (including the 404 a data-plane port gives for the admin-only
        // /healthz/leader route) is "alive, not the leader", which is what
        // probeLeader reports as { alive: true, leader: false }.
        this.removeNode(node);
        continue;
      }
      if (seen.has(node)) continue;
      seen.add(node);
      alive.push(node);
      if (result.leader) leaders.push(node);
    }

    // Invalidation is served LOCALLY by every data-plane node (design §6.4
    // decision A-1: deliberately NOT forwarded to the leader's admin API), so
    // leader preference is only a legacy ordering optimization — any alive node
    // can serve it. Leaders are tried first, then the rest.
    const attempts = [...leaders];
    for (const node of alive) {
      if (!attempts.includes(node)) attempts.push(node);
    }
    if (attempts.length === 0) {
      throw new Error(`hydra: no reachable nodes (removed: ${this.removedNodes.join(', ')})`);
    }

    const errors: unknown[] = [];
    for (const node of attempts) {
      let result: InvalidateTenantAuthCacheResult;
      try {
        result = await this.performInvalidate(node, tenantId, body);
      } catch (err) {
        errors.push(err);
        if (err instanceof HTTPError && (err.status === 401 || err.status === 403)) {
          // Invalid/forbidden tenant token is a client-side error; rotating to
          // another node will not fix it.
          throw err;
        }
        if (this.isNodeFailure(err)) {
          this.removeNode(node);
        }
        continue;
      }
      // A fleet-aware answer — including `503 unavailable` — is the node's
      // verdict about the CLUSTER, and the endpoint is answered locally by
      // whichever node received it, so another node would answer the same
      // question the same way. Return it rather than rotating: rotating here was
      // the defect that made "the cluster was not notified" indistinguishable
      // from "this node is broken".
      //
      // `503 unavailable` is deliberately NOT run through isNodeFailure: the node
      // answered, it is alive, and its own cache was cleared, so it must not be
      // quarantined.
      return result;
    }
    throw new Error(
      `hydra: all ${attempts.length} node(s) failed: ${errors.map(String).join('; ')}`,
    );
  }

  /**
   * Performs one invalidation request against one node.
   *
   * Rejects only with a transport failure or a non-fleet-aware HTTP error; a
   * fleet-aware answer (2xx, or the documented `503 unavailable`) resolves with
   * the decoded result. A 2xx whose body is not decodable also resolves, with
   * `state: undefined`, so the wrappers refuse to call it success.
   */
  private async performInvalidate(
    node: string,
    tenantId: string,
    body: Record<string, unknown> | undefined,
  ): Promise<InvalidateTenantAuthCacheResult> {
    let endpoint = `${node}/tenant/${encodeURIComponent(tenantId)}/api/v1/auth/cache/invalidate`;
    const query = this.queryParams();
    if (query) endpoint += `?${query}`;

    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.token}`,
      Accept: 'application/json',
    };
    let payload: string | undefined;
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      payload = JSON.stringify(body);
    }

    const response = await this.rawFetch(
      endpoint,
      { method: 'POST', headers, body: payload },
      this.requestTimeoutMs,
    );

    // The trace id is captured on BOTH paths: §4.2 tells the tenant to quote
    // `X-Hydra-Trace-Id` when reporting a problem, and a problem is exactly what
    // the error paths are.
    const traceId = readHeader(response.headers, 'X-Hydra-Trace-Id');

    if (response.status < 200 || response.status >= 300) {
      // A 503 `unavailable` carries a NORMAL body — the documented fleet
      // structure with no `error.code` — so decode it before deciding this is an
      // HTTP failure. Reporting it as a bare HTTPError is what lost the "the
      // cluster was not notified" signal.
      const fleet = parseFleetOutcome(response.body);
      if (fleet) {
        return { ...undecodedResult(response.status, node, traceId), ...fleet };
      }
      throw new HTTPError(
        'POST',
        endpoint,
        response.status,
        response.statusText,
        response.body,
        traceId,
      );
    }

    const fleet = parseFleetOutcome(response.body);
    if (!fleet) {
      // A 2xx we cannot read: do not invent a state. The caller gets a result
      // with an undefined state, which the wrapper refuses to report as success.
      return undecodedResult(response.status, node, traceId);
    }
    return { ...undecodedResult(response.status, node, traceId), ...fleet };
  }

  /**
   * Renders the optional `wait` / `timeout_ms` parameters.
   *
   * An unset field is OMITTED rather than sent as its default, so the server
   * applies its own configured default (`wait=converged`, `timeout_ms` =
   * `HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS`, default 2000 ms) — and so a caller
   * can tell "I did not ask" from "I asked for the default". Both were validated
   * in the constructor, so no escaping is needed beyond the fixed token and an
   * integer.
   *
   * Note that the server validates `timeout_ms` (and answers `400
   * invalid_timeout_ms`) even when `wait=none` makes it have no effect.
   */
  private queryParams(): string {
    const parts: string[] = [];
    if (this.waitMode !== undefined) {
      parts.push(`wait=${this.waitMode}`);
    }
    if (this.timeoutMs !== undefined) {
      parts.push(`timeout_ms=${this.timeoutMs}`);
    }
    return parts.join('&');
  }

  /**
   * Probe `/healthz/leader`.
   *
   * That route lives on the **admin port**, not the data plane (200 active /
   * 503 standby / 404 on non-candidate nodes). Only a transport failure means
   * the node is unreachable; any HTTP answer — including the 404 a data-plane
   * port gives — is `{ alive: true, leader: false }`. Quarantining on a
   * non-200 would make the documented data-plane node list empty itself out.
   */
  private async probeLeader(node: string): Promise<{ alive: boolean; leader: boolean }> {
    try {
      const response = await this.rawFetch(
        `${node}/healthz/leader`,
        { method: 'GET' },
        this.probeTimeoutMs,
      );
      return { alive: true, leader: response.status === 200 };
    } catch {
      return { alive: false, leader: false };
    }
  }

  private async rawFetch(
    url: string,
    init: RequestInit,
    timeoutMs: number,
  ): Promise<RawResponse> {
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    try {
      const response = await this.fetchImpl(url, { ...init, signal: controller.signal });
      const body = await response.text();
      return {
        status: response.status,
        statusText: response.statusText ?? '',
        body,
        headers: response.headers,
      };
    } finally {
      clearTimeout(timer);
    }
  }

  private removeNode(node: string): void {
    this.active = this.active.filter((item) => item !== node);
    if (!this.removed.includes(node)) this.removed.push(node);
  }

  /**
   * Reports whether an invalidation error should cause the node to be
   * quarantined. HTTP 4xx errors (other than 401/403, handled by the caller) are
   * treated as node/API incompatibility errors and also cause rotation.
   *
   * A fleet-aware `503 unavailable` never reaches here — it is a result, not an
   * error — which is the point: the node answered and is alive.
   */
  private isNodeFailure(err: unknown): boolean {
    if (err instanceof HTTPError) {
      return err.status >= 500 || err.status === 404 || err.status === 405;
    }
    // A request timeout (or caller cancellation) is not a node failure: the
    // node may be healthy but merely slow. This aligns with the Go SDK, which
    // treats context.DeadlineExceeded/Canceled as non-failures.
    if (isTimeoutError(err)) return false;
    return true;
  }
}
