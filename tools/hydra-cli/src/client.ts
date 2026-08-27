import type { HydraConfig } from './config.js';

/** Error thrown for any non-2xx response or transport failure. */
export class HydraApiError extends Error {
  readonly status: number;
  readonly body: string;
  constructor(message: string, status: number, body: string) {
    super(message);
    this.name = 'HydraApiError';
    this.status = status;
    this.body = body;
  }
}

export type Json = unknown;
type Body = Record<string, unknown> | unknown[] | null;

/**
 * Thin fetch-based HTTP client for the Hydra admin REST API.
 *
 * Every `/api/v1/*` request carries `Authorization: Bearer <token>`. JSON
 * endpoints are parsed automatically; {@link metrics} returns the raw
 * Prometheus text verbatim.
 */
export class HydraClient {
  constructor(private readonly config: HydraConfig) {}

  async health(): Promise<Json> {
    return this.jsonRequest('GET', '/api/v1/health');
  }

  async reload(): Promise<Json> {
    return this.jsonRequest('POST', '/api/v1/reload');
  }

  /** Raw Prometheus text — returned unchanged (NOT parsed as JSON). */
  async metrics(): Promise<string> {
    return this.doRequest('GET', '/metrics', undefined, true);
  }

  async concurrency(): Promise<Json> {
    return this.jsonRequest('GET', '/api/v1/concurrency');
  }

  async breakerList(): Promise<Json> {
    return this.jsonRequest('GET', '/api/v1/breaker');
  }

  async breakerReset(providerId: string): Promise<Json> {
    return this.jsonRequest('DELETE', `/api/v1/breaker/${encodeURIComponent(providerId)}`);
  }

  async authCacheInvalidate(body: Body = {}): Promise<Json> {
    return this.jsonRequest('DELETE', '/api/v1/auth/cache', body);
  }

  async statsUsage(): Promise<Json> {
    return this.jsonRequest('GET', '/api/v1/stats/usage');
  }

  async clusterStatus(): Promise<Json> {
    return this.jsonRequest('GET', '/api/v1/cluster/status');
  }

  async tenantAuthTest(body: Body): Promise<Json> {
    return this.jsonRequest('POST', '/api/v1/tenants/auth/test', body);
  }

  async list(entity: string): Promise<Json> {
    return this.jsonRequest('GET', `/api/v1/${entity}`);
  }

  async get(entity: string, id: string): Promise<Json> {
    return this.jsonRequest('GET', `/api/v1/${entity}/${encodeURIComponent(id)}`);
  }

  async create(entity: string, body: Body): Promise<Json> {
    return this.jsonRequest('POST', `/api/v1/${entity}`, body);
  }

  async update(entity: string, id: string, body: Body): Promise<Json> {
    return this.jsonRequest('PUT', `/api/v1/${entity}/${encodeURIComponent(id)}`, body);
  }

  async delete(entity: string, id: string): Promise<Json> {
    return this.jsonRequest('DELETE', `/api/v1/${entity}/${encodeURIComponent(id)}`);
  }

  // ---- internals -----------------------------------------------------------

  private async jsonRequest(method: string, path: string, body?: Body): Promise<Json> {
    const text = await this.doRequest(method, path, body, false);
    if (text === '') return null;
    try {
      return JSON.parse(text) as Json;
    } catch {
      throw new HydraApiError(
        `Response was not valid JSON: ${text.slice(0, 200)}`,
        0,
        text,
      );
    }
  }

  /**
   * Perform a single request.
   * @param rawText when true the Accept header is text/plain and the raw body is returned.
   */
  private async doRequest(
    method: string,
    path: string,
    body: Body | undefined,
    rawText: boolean,
  ): Promise<string> {
    const url = this.config.baseUrl + path;
    if (this.config.verbose) {
      process.stderr.write(`> ${method} ${url}\n`);
    }

    const headers: Record<string, string> = {
      Authorization: `Bearer ${this.config.token}`,
      Accept: rawText ? 'text/plain' : 'application/json',
    };
    const init: RequestInit = { method, headers };
    if (body !== undefined) {
      headers['Content-Type'] = 'application/json';
      init.body = JSON.stringify(body);
    }

    let res: Response;
    try {
      res = await fetch(url, init);
    } catch (e) {
      const msg = e instanceof Error ? e.message : String(e);
      throw new HydraApiError(`Request failed: ${msg} (${method} ${url})`, 0, '');
    }

    const text = await res.text();
    if (!res.ok) {
      const excerpt = text.length > 300 ? `${text.slice(0, 300)}…` : text;
      const detail = excerpt ? `: ${excerpt}` : '';
      throw new HydraApiError(
        `HTTP ${res.status} ${res.statusText}${detail} (${method} ${url})`,
        res.status,
        text,
      );
    }
    return text;
  }
}
