/* ===========================================================================
 * API Reference (OpenAPI-style) — every admin REST endpoint with curl /
 * Python / TypeScript examples.
 *
 * Plain ES2017+, no build step, no external deps. Rendered by
 * renderApiDocs() (registered in app.js as the "api-docs" custom page).
 * ======================================================================== */

// Placeholder admin base shown in examples — replace with your host.
const API_HOST = "<host>:8081";

// ---------------------------------------------------------------------------
// Endpoint specs (OpenAPI-ish: method / path / parameters / requestBody /
// responses / errors). auth: true = admin token, false = token-free,
// "cluster" = shared cluster token.
// ---------------------------------------------------------------------------
const API_DOCS = [
  { tag: "System", endpoints: [
    { method: "GET", path: "/metrics", summary: "Prometheus metrics (token-free)",
      desc: "Self-hosted Prometheus text exposition on the admin port. Not admin-token gated — scrapers must reach it without the UI token.",
      auth: false,
      resp: ["200 text/plain — Prometheus metrics exposition (0.0.4)"],
      errors: [] },
    { method: "GET", path: "/healthz/leader", summary: "Leader-lease probe (token-free)",
      desc: "200 while this node holds the leader lease, 503 on a standby, 404 on a non-candidate. Token-free so load balancers / orchestrators can route to the active leader (cluster mode).",
      auth: false,
      resp: ["200 — this node is the active leader", "503 — standby (not the lease holder)", "404 — non-candidate / single-node"],
      errors: [] },
    { method: "GET", path: "/api/v1/health", summary: "Service health + snapshot counts",
      desc: "Liveness probe with the live config snapshot counts (providers, tenants, dead breaker set, DB status).",
      auth: true,
      resp: ["200 — {\"status\":\"ok\",\"db\":\"ok\",\"breaker_dead\":0,\"tenants\":1,\"providers\":2}"],
      errors: [{ status: 401, code: "unauthorized", desc: "missing or invalid admin token" }] },
    { method: "POST", path: "/api/v1/reload", summary: "Hot-reload config from the DB",
      desc: "Rebuilds the in-memory config snapshot from SQLite (and re-resolves TLS certs). Returns the loaded snapshot counts; a fatal validation error keeps the old snapshot and returns 400.",
      auth: true,
      body: {},
      resp: ["200 — {\"status\":\"reloaded\",\"tenants\":1,\"providers\":2,\"models\":3,\"keys\":2,\"certs\":0}"],
      errors: [{ status: 400, code: "reload_failed", desc: "config reload failed (old snapshot retained)" }] },
    { method: "DELETE", path: "/api/v1/auth/cache", summary: "Invalidate the auth cache",
      desc: "Force re-authentication. Body is optional: {} (or empty body) is a no-op; tenant_id clears one tenant; api_keys clears specific keys (across all tenants when tenant_id is omitted). In cluster mode the invalidation is broadcast to every node.",
      auth: true,
      body: { tenant_id: "acme", api_keys: ["sk_abc123"] },
      resp: ["200 — {\"invalidated\":1,\"tenant_id\":\"acme\"}"],
      errors: [{ status: 400, code: "invalid_json", desc: "request body is not valid JSON" }] },
    { method: "GET", path: "/api/v1/breaker", summary: "List dead providers",
      desc: "The circuit-breaker dead-set: providers currently excluded from routing (consecutive failures).",
      auth: true,
      resp: ["200 — {\"dead\":[\"provider-a\"]}"],
      errors: [] },
    { method: "DELETE", path: "/api/v1/breaker/{provider_id}", summary: "Reset a provider's breaker",
      desc: "Force-clears the circuit breaker for one provider (marks it healthy again).",
      auth: true, exId: "provider-a",
      pathParams: [{ name: "provider_id", type: "string", desc: "Provider id to reset" }],
      resp: ["200 — {\"reset\":\"provider-a\",\"was_dead\":true,\"dead\":[]}"],
      errors: [] },
    { method: "GET", path: "/api/v1/concurrency", summary: "Admission gate snapshot",
      desc: "Live per-provider concurrency gates: configured cap, in-flight requests, free permits and queue depth (only providers with max_concurrency set).",
      auth: true,
      resp: ["200 — {\"providers\":[{\"provider_id\":\"p1\",\"max_concurrency\":10,\"inflight\":2,\"available\":8,\"queue_depth\":0}]}"],
      errors: [] },
    { method: "GET", path: "/api/v1/stats/usage", summary: "Usage stats by tenant & provider (Admin UI Stats page)",
      desc: "Aggregates the live prometheus counters (hydra_requests_total / hydra_tokens_total) into per-tenant and per-provider rows: request counts and token totals (prompt / completion split) plus gateway-wide totals. Cumulative since process start (not time-windowed). Rows sorted by tokens desc, then requests desc.",
      auth: true,
      resp: ["200 — {\"generated_at\":\"2025-01-01T00:00:00+00:00\",\"totals\":{\"requests\":42,\"tokens\":1000,\"tokens_prompt\":600,\"tokens_completion\":400,\"tenants\":2,\"providers\":3},\"by_tenant\":[{\"name\":\"acme\",\"requests\":40,\"tokens\":900,\"tokens_prompt\":500,\"tokens_completion\":400}],\"by_provider\":[{\"name\":\"openai\",\"requests\":40,\"tokens\":900,\"tokens_prompt\":500,\"tokens_completion\":400}]}"],
      errors: [] },
    { method: "GET", path: "/api/v1/cluster/status", summary: "Whole-cluster fleet status",
      desc: "Cluster P4 view for the Health page: fleet nodes from the registry (role + control URL + heartbeat liveness) plus the leader-lease holder. Single-node mode reports cluster=false.",
      auth: true,
      resp: ["200 — {\"cluster\":true,\"mode\":\"leader\",\"node_id\":\"control-a\",\"this_node_leader\":true,\"lease_holder\":\"control-a\",\"nodes\":[{\"node_id\":\"control-b\",\"role\":\"leader\",\"control_url\":\"http://control-b:8081\",\"alive\":true,\"is_lease_holder\":false,\"is_self\":false}]}"],
      errors: [{ status: 502, code: "cluster_unavailable", desc: "registry unreadable (Redis unreachable?)" }] },
    { method: "GET", path: "/api/v1/internal/control", summary: "Config snapshot (cluster control plane)",
      desc: "Internal snapshot channel used by edge/standby nodes to sync config (secrets sealed). Gated by the SHARED cluster token, not the admin token. snapshot is null when already current.",
      auth: "cluster",
      query: [{ name: "since", type: "integer", desc: "Local config version; the leader only sends a snapshot when newer" }],
      resp: ["200 — {\"version\":7,\"snapshot\":null}", "200 — {\"version\":7,\"snapshot\":{...sealed config...}}"],
      errors: [{ status: 401, code: "unauthorized", desc: "invalid cluster token" }] },
    { method: "POST", path: "/api/v1/tenants/auth/test", summary: "Probe a tenant auth URL (Admin UI Test button)",
      desc: "Sends a simulated auth request (same headers + body as the real auth path, with a clearly fake api-key) to the given auth_url and reports reachability / protocol / verdict. A fake key MUST be rejected, so 401/403 or an explicit denial flag in a 2xx body ({status:false}/{allowed:false}) is a PASS; an allow, 404/405, 422, 5xx or an unreachable URL is a FAIL. Non-mutating.",
      auth: true,
      body: { auth_url: "https://auth.acme.com/v1/verify", tenant_id: "acme" },
      resp: ["200 — {\"ok\":true,\"reachable\":true,\"status\":401,\"protocol_ok\":true,\"verdict\":\"denied\",\"detail\":\"auth service rejected the simulated api-key (expected: key not found / auth failed)\",\"duration_ms\":42}", "200 — {\"ok\":false,\"reachable\":false,\"status\":null,\"protocol_ok\":false,\"verdict\":\"unreachable\",\"detail\":\"URL not reachable: ...\",\"duration_ms\":2000}"],
      errors: [{ status: 400, code: "missing_auth_url", desc: "auth_url is required" }] },
    { method: "POST", path: "/api/v1/tenants/{tenant_id}/auth/cache/invalidate", summary: "Tenant self-service: invalidate own auth cache",
      desc: "Gated by the TENANT access token (Authorization: Bearer <access_token> — NOT the admin token; set on the tenant via the admin UI/API). Clears the tenant's cached api-key auth decisions, forcing re-authentication on the next request — 欠费停机 / 付费恢复 access scenarios. The URL tenant_id must match the token's tenant (403 otherwise, no cross-tenant spoofing). Body api_keys optional: absent/empty clears ALL cached decisions for the tenant. Cluster: broadcast cluster-wide; a standby forwards to the active leader.",
      auth: "tenant",
      body: { api_keys: ["sk-aaa", "sk-bbb"] },
      resp: ["200 — {\"invalidated\":2,\"tenant_id\":\"acme\"}", "200 — {\"invalidated\":0,\"tenant_id\":\"acme\"} (empty body = clear all)"],
      errors: [
        { status: 401, code: "unauthorized", desc: "missing or invalid tenant access token" },
        { status: 403, code: "forbidden", desc: "token does not match tenant_id" } ] },
  ]},

  { tag: "Providers", endpoints: [
    { method: "GET", path: "/api/v1/providers", summary: "List providers",
      desc: "All upstream LLM providers, ordered by created_at.",
      auth: true,
      resp: ["200 — array of Provider objects"],
      errors: [] },
    { method: "POST", path: "/api/v1/providers", summary: "Create a provider",
      desc: "Registers an upstream provider. id/created_at/updated_at are auto-filled when blank. weight=0 soft-disables the provider. key must be globally unique.",
      auth: true, exId: "openai",
      body: { id: "openai", key: "openai", name: "OpenAI", endpoint: "https://api.openai.com", weight: 1, created_at: "", updated_at: "" },
      resp: ["201 — the created Provider (with filled timestamps)"],
      errors: [
        { status: 400, code: "invalid_json", desc: "request body is not valid JSON" },
        { status: 409, code: "sqlite_constraint_primarykey / _unique", desc: "duplicate id or key" },
      ] },
    { method: "GET", path: "/api/v1/providers/{id}", summary: "Get a provider by id",
      auth: true, exId: "openai",
      pathParams: [{ name: "id", type: "string", desc: "Provider id" }],
      resp: ["200 — the Provider"],
      errors: [{ status: 404, code: "not_found", desc: "provider not found" }] },
    { method: "PUT", path: "/api/v1/providers/{id}", summary: "Update a provider",
      desc: "Replaces the mutable fields (key, name, endpoint, weight, concurrency options). id in the URL wins.",
      auth: true, exId: "openai",
      pathParams: [{ name: "id", type: "string", desc: "Provider id" }],
      body: { id: "openai", key: "openai", name: "OpenAI", endpoint: "https://api.openai.com", weight: 2, created_at: "", updated_at: "" },
      resp: ["200 — the updated Provider"],
      errors: [{ status: 404, code: "not_found", desc: "provider not found" }] },
    { method: "DELETE", path: "/api/v1/providers/{id}", summary: "Delete a provider",
      desc: "Removes the provider (CASCADE deletes its models/keys and tenant access links).",
      auth: true, exId: "openai",
      pathParams: [{ name: "id", type: "string", desc: "Provider id" }],
      resp: ["204 No Content"],
      errors: [] },
  ]},

  { tag: "Provider Models", endpoints: [
    { method: "GET", path: "/api/v1/provider-models", summary: "List provider models",
      desc: "Models exposed by each provider; key is the routing keyword matched against the request's model.",
      auth: true,
      resp: ["200 — array of ProviderModel objects"],
      errors: [] },
    { method: "POST", path: "/api/v1/provider-models", summary: "Create a provider model",
      desc: "status: 1 online / 0 manually offline / -1 probe-offline. Only status=1 enters routing candidates. UNIQUE(key, provider_id).",
      auth: true, exId: "m1",
      body: { id: "m1", key: "gpt-4o", name: "GPT-4o", provider_id: "openai", status: 1 },
      resp: ["201 — the created ProviderModel"],
      errors: [{ status: 400, code: "invalid_json", desc: "request body is not valid JSON" }] },
    { method: "GET", path: "/api/v1/provider-models/{id}", summary: "Get a provider model",
      auth: true, exId: "m1",
      pathParams: [{ name: "id", type: "string", desc: "Model id" }],
      resp: ["200 — the ProviderModel"],
      errors: [{ status: 404, code: "not_found", desc: "model not found" }] },
    { method: "PUT", path: "/api/v1/provider-models/{id}", summary: "Update a provider model",
      auth: true, exId: "m1",
      pathParams: [{ name: "id", type: "string", desc: "Model id" }],
      body: { id: "m1", key: "gpt-4o", name: "GPT-4o", provider_id: "openai", status: 0 },
      resp: ["200 — the updated ProviderModel"],
      errors: [{ status: 404, code: "not_found", desc: "model not found" }] },
    { method: "DELETE", path: "/api/v1/provider-models/{id}", summary: "Delete a provider model",
      auth: true, exId: "m1",
      pathParams: [{ name: "id", type: "string", desc: "Model id" }],
      resp: ["204 No Content"],
      errors: [] },
  ]},

  { tag: "Provider Keys", endpoints: [
    { method: "GET", path: "/api/v1/provider-keys", summary: "List provider keys (masked)",
      desc: "Upstream API keys per provider. The admin API NEVER returns plaintext keys — api_key is masked (P1-5); the legacy ?reveal=1 param is accepted but is a no-op.",
      auth: true,
      resp: ["200 — array of ProviderKey objects with api_key masked"],
      errors: [] },
    { method: "POST", path: "/api/v1/provider-keys", summary: "Create a provider key",
      desc: "Stores a plaintext api_key from the body; it is AES-256-GCM sealed at rest and never echoed back (the response api_key is masked).",
      auth: true, exId: "k1",
      body: { id: "k1", provider_id: "openai", api_key: "sk-your-real-key", created_at: "" },
      resp: ["201 — the created ProviderKey with api_key masked"],
      errors: [{ status: 400, code: "invalid_json", desc: "request body is not valid JSON" }] },
    { method: "GET", path: "/api/v1/provider-keys/{id}", summary: "Get a provider key (masked)",
      auth: true, exId: "k1",
      pathParams: [{ name: "id", type: "string", desc: "Key id" }],
      resp: ["200 — the ProviderKey with api_key masked"],
      errors: [{ status: 404, code: "not_found", desc: "key not found" }] },
    { method: "PUT", path: "/api/v1/provider-keys/{id}", summary: "Upsert a provider key",
      desc: "No dedicated update: delete + re-insert under the URL id. Send the full api_key (old plaintext is never returned).",
      auth: true, exId: "k1",
      pathParams: [{ name: "id", type: "string", desc: "Key id" }],
      body: { id: "k1", provider_id: "openai", api_key: "sk-rotated-key", created_at: "" },
      resp: ["200 — the upserted ProviderKey with api_key masked"],
      errors: [] },
    { method: "DELETE", path: "/api/v1/provider-keys/{id}", summary: "Delete a provider key",
      auth: true, exId: "k1",
      pathParams: [{ name: "id", type: "string", desc: "Key id" }],
      resp: ["204 No Content"],
      errors: [] },
  ]},

  { tag: "Tenants", endpoints: [
    { method: "GET", path: "/api/v1/tenants", summary: "List tenants",
      desc: "Tenants are identified by domain (lowercased).",
      auth: true,
      resp: ["200 — array of Tenant objects"],
      errors: [] },
    { method: "POST", path: "/api/v1/tenants", summary: "Create a tenant",
      desc: "auth_url is mandatory (empty ⇒ all requests 401). Certificates are optional: leave cert_pem/cert_key_pem blank for no cert (legacy cert_file/cert_key paths still accepted for compat; empty strings are ignored). cert_pem without cert_key_pem ⇒ 400. The private key is sealed at rest and never echoed back.",
      auth: true, exId: "acme",
      body: { id: "acme", name: "ACME", domain: "acme.example.com", auth_url: "https://auth.acme.example.com/v", cert_file: null, cert_key: null, cert_pem: null, cert_key_pem: null, enabled: true, created_at: "", updated_at: "" },
      resp: ["201 — the created Tenant (cert content never included)"],
      errors: [
        { status: 400, code: "missing_required_field", desc: "auth_url required; or cert_key_pem required when cert_pem is set" },
        { status: 400, code: "cert_file_unreadable", desc: "legacy cert paths not readable on this node" },
        { status: 409, code: "sqlite_constraint_unique", desc: "duplicate domain" },
      ] },
    { method: "GET", path: "/api/v1/tenants/{id}", summary: "Get a tenant",
      auth: true, exId: "acme",
      pathParams: [{ name: "id", type: "string", desc: "Tenant id" }],
      resp: ["200 — the Tenant"],
      errors: [{ status: 404, code: "not_found", desc: "tenant not found" }] },
    { method: "PUT", path: "/api/v1/tenants/{id}", summary: "Update a tenant",
      desc: "Same body rules as create. Blank cert fields keep the current cert; cert_pem:\"\" explicitly clears it.",
      auth: true, exId: "acme",
      pathParams: [{ name: "id", type: "string", desc: "Tenant id" }],
      body: { id: "acme", name: "ACME", domain: "acme.example.com", auth_url: "https://auth.acme.example.com/v", cert_file: null, cert_key: null, cert_pem: null, cert_key_pem: null, enabled: false, created_at: "", updated_at: "" },
      resp: ["200 — the updated Tenant"],
      errors: [{ status: 400, code: "missing_required_field", desc: "see create" }] },
    { method: "DELETE", path: "/api/v1/tenants/{id}", summary: "Delete a tenant",
      auth: true, exId: "acme",
      pathParams: [{ name: "id", type: "string", desc: "Tenant id" }],
      resp: ["204 No Content"],
      errors: [] },
  ]},

  { tag: "Tenant Access", endpoints: [
    { method: "GET", path: "/api/v1/tenant-providers", summary: "List tenant→provider grants",
      auth: true,
      resp: ["200 — array of TenantProvider objects"],
      errors: [] },
    { method: "POST", path: "/api/v1/tenant-providers", summary: "Grant a provider to a tenant",
      auth: true, exId: "tp1",
      body: { id: "tp1", tenant_id: "acme", provider_id: "openai" },
      resp: ["201 — the created TenantProvider"],
      errors: [] },
    { method: "GET", path: "/api/v1/tenant-providers/{id}", summary: "Get a tenant-provider grant",
      auth: true, exId: "tp1",
      pathParams: [{ name: "id", type: "string", desc: "Grant id" }],
      resp: ["200 — the TenantProvider"],
      errors: [{ status: 404, code: "not_found", desc: "not found" }] },
    { method: "PUT", path: "/api/v1/tenant-providers/{id}", summary: "Upsert a tenant-provider grant",
      desc: "No dedicated update — upsert by URL id.",
      auth: true, exId: "tp1",
      pathParams: [{ name: "id", type: "string", desc: "Grant id" }],
      body: { id: "tp1", tenant_id: "acme", provider_id: "anthropic" },
      resp: ["200 — the upserted TenantProvider"],
      errors: [] },
    { method: "DELETE", path: "/api/v1/tenant-providers/{id}", summary: "Revoke a tenant-provider grant",
      auth: true, exId: "tp1",
      pathParams: [{ name: "id", type: "string", desc: "Grant id" }],
      resp: ["204 No Content"],
      errors: [] },
    { method: "GET", path: "/api/v1/tenant-models", summary: "List tenant→model mappings",
      desc: "A tenant may only route to models listed here (default-open when no mapping exists).",
      auth: true,
      resp: ["200 — array of TenantModel objects"],
      errors: [] },
    { method: "POST", path: "/api/v1/tenant-models", summary: "Map a model to a tenant",
      auth: true, exId: "tm1",
      body: { id: "tm1", tenant_id: "acme", model_key: "gpt-4o" },
      resp: ["201 — the created TenantModel"],
      errors: [] },
    { method: "GET", path: "/api/v1/tenant-models/{id}", summary: "Get a tenant-model mapping",
      auth: true, exId: "tm1",
      pathParams: [{ name: "id", type: "string", desc: "Mapping id" }],
      resp: ["200 — the TenantModel"],
      errors: [{ status: 404, code: "not_found", desc: "not found" }] },
    { method: "PUT", path: "/api/v1/tenant-models/{id}", summary: "Upsert a tenant-model mapping",
      auth: true, exId: "tm1",
      pathParams: [{ name: "id", type: "string", desc: "Mapping id" }],
      body: { id: "tm1", tenant_id: "acme", model_key: "claude-3-5-sonnet" },
      resp: ["200 — the upserted TenantModel"],
      errors: [] },
    { method: "DELETE", path: "/api/v1/tenant-models/{id}", summary: "Remove a tenant-model mapping",
      auth: true, exId: "tm1",
      pathParams: [{ name: "id", type: "string", desc: "Mapping id" }],
      resp: ["204 No Content"],
      errors: [] },
  ]},

  { tag: "Rate Limit Roles", endpoints: [
    { method: "GET", path: "/api/v1/limit-roles", summary: "List rate-limit roles",
      auth: true,
      resp: ["200 — array of LimitRole objects"],
      errors: [] },
    { method: "POST", path: "/api/v1/limit-roles", summary: "Create a rate-limit role",
      desc: "matching_* fields are optional filters (tenant / key / model / provider); blank = match all. window: m / h / d windows. limit_count / limit_token: 0/absent = unlimited.",
      auth: true, exId: "rl1",
      body: { id: "rl1", name: "basic", matching_key: null, matching_model: null, matching_tenant: null, matching_provider: null, limit_count: 100, limit_token: 100000, window: "1h", enabled: true, created_at: "" },
      resp: ["201 — the created LimitRole"],
      errors: [{ status: 400, code: "invalid_json", desc: "request body is not valid JSON" }] },
    { method: "GET", path: "/api/v1/limit-roles/{id}", summary: "Get a rate-limit role",
      auth: true, exId: "rl1",
      pathParams: [{ name: "id", type: "string", desc: "Role id" }],
      resp: ["200 — the LimitRole"],
      errors: [{ status: 404, code: "not_found", desc: "role not found" }] },
    { method: "PUT", path: "/api/v1/limit-roles/{id}", summary: "Update a rate-limit role",
      auth: true, exId: "rl1",
      pathParams: [{ name: "id", type: "string", desc: "Role id" }],
      body: { id: "rl1", name: "basic", matching_key: null, matching_model: null, matching_tenant: null, matching_provider: null, limit_count: 50, limit_token: 50000, window: "1d", enabled: true, created_at: "" },
      resp: ["200 — the updated LimitRole"],
      errors: [] },
    { method: "DELETE", path: "/api/v1/limit-roles/{id}", summary: "Delete a rate-limit role",
      auth: true, exId: "rl1",
      pathParams: [{ name: "id", type: "string", desc: "Role id" }],
      resp: ["204 No Content"],
      errors: [] },
  ]},

  { tag: "Key Prefix Bindings", endpoints: [
    { method: "GET", path: "/api/v1/provider-key-bindings", summary: "List key-prefix bindings",
      desc: "Client api-keys whose raw value starts with key_prefix are pinned to one provider (longest prefix wins, fail-closed).",
      auth: true,
      resp: ["200 — array of ProviderKeyBinding objects"],
      errors: [] },
    { method: "POST", path: "/api/v1/provider-key-bindings", summary: "Create a key-prefix binding",
      desc: "key_prefix must be a non-empty string.",
      auth: true, exId: "b1",
      body: { id: "b1", key_prefix: "sk_aaa_", provider_id: "openai", enabled: true, created_at: "", updated_at: "" },
      resp: ["201 — the created ProviderKeyBinding"],
      errors: [{ status: 400, code: "empty_key_prefix", desc: "key_prefix must be a non-empty string" }] },
    { method: "GET", path: "/api/v1/provider-key-bindings/{id}", summary: "Get a key-prefix binding",
      auth: true, exId: "b1",
      pathParams: [{ name: "id", type: "string", desc: "Binding id" }],
      resp: ["200 — the ProviderKeyBinding"],
      errors: [{ status: 404, code: "not_found", desc: "provider_key_binding not found" }] },
    { method: "PUT", path: "/api/v1/provider-key-bindings/{id}", summary: "Update a key-prefix binding",
      auth: true, exId: "b1",
      pathParams: [{ name: "id", type: "string", desc: "Binding id" }],
      body: { id: "b1", key_prefix: "sk_aaa_", provider_id: "anthropic", enabled: false, created_at: "", updated_at: "" },
      resp: ["200 — the updated ProviderKeyBinding"],
      errors: [{ status: 400, code: "empty_key_prefix", desc: "key_prefix must be a non-empty string" }] },
    { method: "DELETE", path: "/api/v1/provider-key-bindings/{id}", summary: "Delete a key-prefix binding",
      auth: true, exId: "b1",
      pathParams: [{ name: "id", type: "string", desc: "Binding id" }],
      resp: ["204 No Content"],
      errors: [] },
  ]},
];
// ---------------------------------------------------------------------------
// Example generators (curl / Python / TypeScript)
// ---------------------------------------------------------------------------

function apiExUrl(e) {
  let p = e.path;
  if (e.exId) p = p.replace(/\{[^}]+\}/g, e.exId); // {id}, {provider_id}, ...
  if (e.query && e.query.length) {
    const qs = e.query.map((q) => q.name + "=" + (q.example !== undefined ? q.example : "<value>")).join("&");
    p += (p.includes("?") ? "&" : "?") + qs;
  }
  return "http://" + API_HOST + p;
}
function apiExToken(e) {
  if (e.auth === false) return null;
  return e.auth === "cluster" ? "$CLUSTER_TOKEN" : "$ADMIN_TOKEN";
}
function apiExCurl(e) {
  const url = apiExUrl(e);
  const tok = apiExToken(e);
  const lines = ["curl -X " + e.method + " \"" + url + "\""];
  if (tok) lines.push("  -H \"Authorization: Bearer " + tok + "\"");
  if (e.body) {
    lines.push("  -H \"Content-Type: application/json\"");
    lines.push("  -d '" + JSON.stringify(e.body, null, 2) + "'");
  }
  const nl = "\n";
  return lines.join(" \\" + nl);
}
function apiExPython(e) {
  const url = apiExUrl(e);
  const tok = apiExToken(e);
  const name = tok === "$CLUSTER_TOKEN" ? "CLUSTER_TOKEN" : "ADMIN_TOKEN";
  const L = ["import requests", ""];
  if (tok) {
    L.push(name + " = \"...\"  # shared " + (name === "CLUSTER_TOKEN" ? "cluster" : "admin") + " token");
    L.push("");
  }
  L.push("url = " + JSON.stringify(url));
  if (e.body) {
    const py = JSON.stringify(e.body, null, 2)
      .replace(/\bnull\b/g, "None").replace(/\btrue\b/g, "True").replace(/\bfalse\b/g, "False");
    L.push("payload = " + py.replace(/\n/g, "\n    "));
  }
  const kwargs = [];
  if (e.body) kwargs.push("json=payload");
  if (tok) kwargs.push("headers={\"Authorization\": f\"Bearer {" + name + "}\"}");
  L.push("");
  L.push("r = requests." + e.method.toLowerCase() + "(url" + (kwargs.length ? ", " + kwargs.join(", ") : "") + ")");
  L.push("print(r.status_code, r.json())");
  return L.join("\n");
}
function apiExTs(e) {
  const url = apiExUrl(e);
  const tok = apiExToken(e);
  const L = [];
  if (tok) {
    L.push("const TOKEN = \"...\";  // " + (tok === "$CLUSTER_TOKEN" ? "cluster" : "admin") + " token");
    L.push("");
  }
  L.push("const res = await fetch(\"" + url + "\", {");
  L.push("  method: \"" + e.method + "\",");
  if (tok) L.push("  headers: { Authorization: \"Bearer \" + TOKEN, \"Content-Type\": \"application/json\" },");
  else L.push("  headers: { \"Content-Type\": \"application/json\" },");
  if (e.body) L.push("  body: JSON.stringify(" + JSON.stringify(e.body, null, 2).split("\n").join("\n    ") + "),");
  L.push("});");
  L.push("console.log(res.status, await res.json());");
  return L.join("\n");
}
const API_EXAMPLE_FNS = { curl: apiExCurl, python: apiExPython, typescript: apiExTs };

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------
/* i18n key for an endpoint summary: "apidocs.summary.<METHOD>.<path>" with
 * path segments joined by "." (leading / stripped, / → ., - → _, { } removed).
 * Keep in sync with the apidocs.summary.* entries in i18n.js when
 * adding/removing endpoints. */
function apiSummaryKey(e) {
  return "apidocs.summary." + e.method + "." + e.path
    .replace(/^\//, "").replaceAll("/", ".").replaceAll("-", "_").replaceAll("{", "").replaceAll("}", "");
}

function renderApiDocs() {
  const content = $("#content");
  clear(content);
  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "panel-head" }, el("h2", {}, el("span", { text: t("apidocs.chrome.pageTitle") })), el("div", { class: "spacer" })),
    el("div", { class: "docs-intro" },
      el("p", { html: "All management endpoints live under <code>/api/v1/*</code> on the <b>admin port</b> (<code>:8081</code>). " +
        "Every <code>/api/v1/*</code> request needs <code>Authorization: Bearer &lt;admin-token&gt;</code> — the same token you use to log into this UI — " +
        "except <code>/metrics</code>, <code>/healthz/leader</code> (token-free) and <code>/api/v1/internal/*</code> (cluster token)." }),
      el("p", { html: "Config writes are hot-reloaded automatically. In cluster mode, a mutation sent to a standby node is " +
        "forwarded to the active leader transparently, so you can point the API at any leader-candidate node." }),
    ),
  ));
  for (const tag of API_DOCS) {
    const panel = el("div", { class: "panel docs-group" },
      el("div", { class: "panel-head" }, el("h2", {}, el("span", { text: t("apidocs.tag." + tag.tag) })), el("div", { class: "spacer" })),
    );
    for (const e of tag.endpoints) panel.appendChild(renderApiEndpoint(e));
    content.appendChild(panel);
  }
}
function renderApiEndpoint(e) {
  const detail = el("div", { class: "api-detail hidden" });
  const head = el("button", { class: "api-head", onClick: () => {
    const open = detail.classList.toggle("hidden");
    head.classList.toggle("open", !open);
  }},
    el("span", { class: "method method-" + e.method.toLowerCase() }, e.method),
    el("code", { class: "api-path", text: e.path }),
    el("span", { class: "api-summary", text: t(apiSummaryKey(e)) }),
    el("span", { class: "api-chev", text: "▾" }),
  );
  detail.appendChild(apiDetail(e));
  return el("div", { class: "api-endpoint" }, head, detail);
}
function apiDetail(e) {
  const root = el("div", { class: "api-detail-inner" });
  if (e.desc) root.appendChild(el("p", { class: "api-desc", text: e.desc }));
  root.appendChild(el("div", { class: "api-meta" },
    el("span", { class: "auth-badge " + (e.auth === false ? "noauth" : e.auth === "cluster" ? "clusterauth" : "adminauth") },
      e.auth === false ? t("apidocs.chrome.noAuth") : e.auth === "cluster" ? t("apidocs.chrome.clusterToken") : t("apidocs.chrome.adminToken")),
  ));
  const params = [
    ...(e.pathParams || []).map((p) => ({ ...p, in: "path" })),
    ...(e.query || []).map((q) => ({ ...q, in: "query" })),
  ];
  if (params.length) {
    root.appendChild(el("h4", { text: t("apidocs.chrome.parameters") }));
    root.appendChild(el("table", { class: "api-table" },
      el("thead", {}, el("tr", {}, el("th", { text: t("apidocs.chrome.name") }), el("th", { text: t("apidocs.chrome.in") }), el("th", { text: t("apidocs.chrome.type") }), el("th", { text: t("apidocs.chrome.description") }))),
      el("tbody", {}, ...params.map((p) =>
        el("tr", {}, el("td", { class: "mono", text: p.name }), el("td", { text: p.in }), el("td", { text: p.type || "string" }), el("td", { text: p.desc || "" })))),
    ));
  }
  if (e.body) {
    root.appendChild(el("h4", { text: t("apidocs.chrome.requestBody") }));
    root.appendChild(el("pre", { class: "api-code" }, JSON.stringify(e.body, null, 2)));
  }
  root.appendChild(el("h4", { text: t("apidocs.chrome.responses") }));
  root.appendChild(el("ul", { class: "api-list" }, ...e.resp.map((r) => el("li", { html: esc(r) }))));
  if (e.errors && e.errors.length) {
    root.appendChild(el("h4", { text: t("apidocs.chrome.errors") }));
    root.appendChild(el("table", { class: "api-table" },
      el("thead", {}, el("tr", {}, el("th", { text: t("apidocs.chrome.status") }), el("th", { text: t("apidocs.chrome.code") }), el("th", { text: t("apidocs.chrome.description") }))),
      el("tbody", {}, ...e.errors.map((x) =>
        el("tr", {}, el("td", { text: x.status }), el("td", { class: "mono", text: x.code }), el("td", { text: x.desc })))),
    ));
  }
  root.appendChild(el("h4", { text: t("apidocs.chrome.examples") }));
  const tabs = ["curl", "python", "typescript"];
  const code = el("pre", { class: "api-code lang-example" });
  const copyBtn = el("button", { class: "btn sm api-copy", text: t("common.action.copy") });
  const bar = el("div", { class: "lang-tabs" },
    ...tabs.map((l) => {
      const btn = el("button", { class: "lang-tab", text: l, dataset: { lang: l } });
      btn.addEventListener("click", () => {
        tabs.forEach((t) => bar.querySelector("[data-lang=\"" + t + "\"]").classList.toggle("active", t === l));
        code.textContent = API_EXAMPLE_FNS[l](e);
      });
      return btn;
    }),
  );
  bar.querySelector("[data-lang=\"curl\"]").classList.add("active");
  code.textContent = apiExCurl(e);
  copyBtn.addEventListener("click", () => copyText(code.textContent, copyBtn));
  root.appendChild(bar);
  root.appendChild(el("div", { class: "api-code-wrap" }, code, copyBtn));
  return root;
}
function copyText(text, btn) {
  const flash = () => {
    const old = t("common.action.copy");
    btn.textContent = t("common.action.copied");
    setTimeout(() => { btn.textContent = old; }, 1200);
  };
  const fallback = () => {
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    try { document.execCommand("copy"); flash(); } catch { /* ignore */ }
    ta.remove();
  };
  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).then(flash).catch(fallback);
  } else fallback();
}