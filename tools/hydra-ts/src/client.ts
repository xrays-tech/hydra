/**
 * Cluster-aware Hydra tenant SDK.
 *
 * The client accepts one or more Hydra cluster node base URLs. Before each
 * invalidation it probes /healthz/leader to discover the current active
 * leader. If the chosen node fails, the client automatically rotates to the
 * next available node and temporarily removes the dead node from the active
 * pool. A background timer periodically probes removed nodes and adds them
 * back once they become reachable again.
 */

export interface HydraClientConfig {
  /** Tenant self-service access token sent as Authorization: Bearer <token>. */
  token: string;
  /** Hydra cluster node base URLs, e.g. "http://127.0.0.1:8081". */
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
}

export class HTTPError extends Error {
  readonly method: string;
  readonly url: string;
  readonly status: number;
  readonly statusText: string;
  readonly body: string;

  constructor(method: string, url: string, status: number, statusText: string, body: string) {
    const excerpt = body.trim().slice(0, 300);
    super(
      excerpt
        ? `${method} ${url}: unexpected HTTP ${status} ${statusText}: ${excerpt}`
        : `${method} ${url}: unexpected HTTP ${status} ${statusText}`,
    );
    this.name = 'HTTPError';
    this.method = method;
    this.url = url;
    this.status = status;
    this.statusText = statusText;
    this.body = body;
  }
}

interface RawResponse {
  status: number;
  statusText: string;
  body: string;
}

type ConstructorArg = HydraClientConfig | string;

export class HydraClient {
  private readonly token: string;
  private readonly fetchImpl: typeof fetch;
  private readonly probeTimeoutMs: number;
  private readonly requestTimeoutMs: number;
  private readonly recheckIntervalMs: number;
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

    this.token = config.token.trim();
    this.fetchImpl = config.fetchImpl ?? fetch;
    this.probeTimeoutMs = config.probeTimeoutMs ?? 2000;
    this.requestTimeoutMs = config.requestTimeoutMs ?? 10000;
    this.recheckIntervalMs = config.recheckIntervalMs ?? 30000;

    const seen = new Set<string>();
    this.active = [];
    for (const raw of config.nodes) {
      const node = raw.trim().replace(/\/+$/, '');
      if (!node || seen.has(node)) continue;
      const u = new URL(node);
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

  async invalidateTenantAuthCache(tenantId: string): Promise<void> {
    await this.invalidateInternal(tenantId, undefined);
  }

  async invalidateTenantAuthCacheKeys(tenantId: string, apiKeys: string[]): Promise<void> {
    if (apiKeys.length === 0) {
      await this.invalidateTenantAuthCache(tenantId);
      return;
    }
    await this.invalidateInternal(tenantId, { api_keys: apiKeys });
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

  private async invalidateInternal(tenantId: string, body: Record<string, unknown> | undefined): Promise<void> {
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
        this.removeNode(node);
        continue;
      }
      if (seen.has(node)) continue;
      seen.add(node);
      alive.push(node);
      if (result.leader) leaders.push(node);
    }

    const attempts = [...leaders];
    for (const node of alive) {
      if (!attempts.includes(node)) attempts.push(node);
    }
    if (attempts.length === 0) {
      throw new Error(`hydra: no reachable nodes (removed: ${this.removedNodes.join(', ')})`);
    }

    const errors: unknown[] = [];
    for (const node of attempts) {
      try {
        await this.doInvalidate(node, tenantId, body);
        return;
      } catch (err) {
        errors.push(err);
        if (err instanceof HTTPError && (err.status === 401 || err.status === 403)) {
          throw err;
        }
        if (this.isNodeFailure(err)) {
          this.removeNode(node);
        }
      }
    }
    throw new Error(`hydra: all ${attempts.length} node(s) failed: ${errors.map(String).join('; ')}`);
  }

  private async doInvalidate(
    node: string,
    tenantId: string,
    body: Record<string, unknown> | undefined,
  ): Promise<void> {
    const endpoint = `${node}/api/v1/tenants/${encodeURIComponent(tenantId)}/auth/cache/invalidate`;
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
    if (response.status < 200 || response.status >= 300) {
      throw new HTTPError(
        'POST',
        endpoint,
        response.status,
        response.statusText,
        response.body,
      );
    }
  }

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
      };
    } finally {
      clearTimeout(timer);
    }
  }

  private removeNode(node: string): void {
    this.active = this.active.filter((item) => item !== node);
    if (!this.removed.includes(node)) this.removed.push(node);
  }

  private isNodeFailure(err: unknown): boolean {
    if (err instanceof HTTPError) {
      return err.status >= 500 || err.status === 404 || err.status === 405;
    }
    return true;
  }
}
