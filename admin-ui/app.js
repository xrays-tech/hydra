/* Hydra admin UI — vanilla JS, no build step (design §14.2).
 *
 * Embedded via include_dir! at compile time. Talks same-origin to /api/v1/*
 * with `Authorization: Bearer <admin-token>`. The token is held in memory only
 * (never persisted) and cleared on sign-out / tab close.
 *
 * Config-driven: each of the 7 CRUD entities is described by a config object
 * (columns + form fields). Generic renderers build the table, the modal form
 * (with async foreign-key <select>s), the confirm dialog and toasts.
 */
"use strict";

/* ===========================================================================
 * DOM helpers
 * ======================================================================== */
const $ = (sel, root = document) => root.querySelector(sel);
const $$ = (sel, root = document) => Array.from(root.querySelectorAll(sel));

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v === null || v === undefined || v === false) continue;
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k === "html") node.innerHTML = v;
    else if (k === "dataset") Object.assign(node.dataset, v);
    else if (k.startsWith("on") && typeof v === "function") node.addEventListener(k.slice(2).toLowerCase(), v);
    else node.setAttribute(k, v === true ? "" : v);
  }
  for (const c of children) append(node, c);
  return node;
}
function append(parent, c) {
  if (c === null || c === undefined || c === false) return;
  parent.appendChild(typeof c === "string" || typeof c === "number" ? document.createTextNode(String(c)) : c);
}
function clear(node) { while (node.firstChild) node.removeChild(node.firstChild); return node; }
function esc(s) {
  if (s === null || s === undefined) return "";
  return String(s).replaceAll("&", "&amp;").replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;").replaceAll('"', "&quot;");
}

/* ---- inline SVG icon set (stroke, 24-grid) ---- */
const ICONS = {
  providers:  '<rect x="3" y="4" width="18" height="6" rx="1.5"/><rect x="3" y="14" width="18" height="6" rx="1.5"/><path d="M7 7h.01M7 17h.01"/>',
  models:     '<path d="M12 2 3 7v10l9 5 9-5V7l-9-5Z"/><path d="m3 7 9 5 9-5"/><path d="M12 22V12"/>',
  keys:       '<circle cx="8" cy="15" r="4"/><path d="m11 12 9-9"/><path d="m17 6 3 3"/><path d="m14 9 2 2"/>',
  tenants:    '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M22 21v-2a4 4 0 0 0-3-3.87"/><path d="M16 3.13a4 4 0 0 1 0 7.75"/>',
  access:     '<path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1 1"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1-1"/>',
  gate:       '<path d="M3 4h18l-7 8v6l-4 2v-8L3 4Z"/>',
  limits:     '<path d="M21 12a9 9 0 1 0-9 9"/><path d="M12 7v5l3 2"/>',
  authcache:  '<path d="M12 2 4 5v7c0 5 3.5 8 8 10 4.5-2 8-5 8-10V5l-8-3Z"/><path d="m9 12 2 2 4-4"/>',
  breaker:    '<path d="M13 2 4 14h7l-1 8 9-12h-7l1-8Z"/>',
  health:     '<path d="M3 12h4l2-6 4 12 2-6h6"/>',
  plus:       '<path d="M12 5v14M5 12h14"/>',
  edit:       '<path d="M12 20h9"/><path d="M16.5 3.5a2.1 2.1 0 0 1 3 3L7 19l-4 1 1-4Z"/>',
  trash:      '<path d="M3 6h18"/><path d="M8 6V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6"/><path d="M10 11v6M14 11v6"/>',
  refresh:    '<path d="M21 12a9 9 0 1 1-3-6.7"/><path d="M21 4v4h-4"/>',
  check:      '<path d="M20 6 9 17l-5-5"/>',
  alert:      '<path d="M12 9v4M12 17h.01"/><path d="M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z"/>',
  close:      '<path d="M18 6 6 18M6 6l12 12"/>',
  info:       '<circle cx="12" cy="12" r="10"/><path d="M12 16v-4M12 8h.01"/>',
  inbox:      '<path d="M22 12h-6l-2 3h-4l-2-3H2"/><path d="M5.5 5h13l3.5 7v6a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2v-6Z"/>',
  key2:       '<circle cx="8" cy="15" r="4"/><path d="m11 12 9-9"/>',
  book:       '<path d="M4 19.5A2.5 2.5 0 0 1 6.5 17H20"/><path d="M6.5 2H20v20H6.5A2.5 2.5 0 0 1 4 19.5v-15A2.5 2.5 0 0 1 6.5 2Z"/>',
};
function icon(name, size = 17) {
  const wrap = document.createElement("span");
  wrap.innerHTML = `<svg viewBox="0 0 24 24" width="${size}" height="${size}" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">${ICONS[name] || ""}</svg>`;
  return wrap.firstElementChild;
}

/* ===========================================================================
 * State
 * ======================================================================== */
const API = "/api/v1";
let TOKEN = null;
let CURRENT = "providers";
const FK = {};          // { providers: {list, map}, tenants: {...} } — lazy
const COUNTS = {};      // section-key -> row count (for nav badges)
const FK_PATHS = { providers: "/providers", tenants: "/tenants" };

async function ensureFK(kind) {
  if (FK[kind]) return FK[kind];
  try {
    const rows = await api("GET", FK_PATHS[kind]) || [];
    FK[kind] = { list: rows, map: new Map(rows.map((r) => [String(r.id), r])) };
  } catch (e) {
    FK[kind] = { list: [], map: new Map() };
  }
  return FK[kind];
}
function invalidateFK(kind) { delete FK[kind]; }

/* ===========================================================================
 * HTTP
 * ======================================================================== */
async function api(method, path, { body, query } = {}) {
  if (!TOKEN) throw new Error("not authenticated");
  const headers = { Authorization: `Bearer ${TOKEN}` };
  let url = API + path;
  if (query) url += query;
  const opts = { method, headers };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    opts.body = typeof body === "string" ? body : JSON.stringify(body);
  }
  let resp, text;
  try {
    resp = await fetch(url, opts);
    text = await resp.text();
  } catch (e) {
    throw new Error(`network: ${e.message}`);
  }
  let json = null;
  if (text) { try { json = JSON.parse(text); } catch { json = text; } }
  if (!resp.ok) {
    const code = json?.error?.code || resp.status;
    const message = json?.error?.message || (typeof json === "string" ? json : resp.statusText);
    const err = new Error(`${resp.status} ${code}: ${message}`);
    err.status = resp.status; err.code = code; err.body = json;
    throw err;
  }
  return json;
}
/** write = POST/PUT/DELETE; after every successful write, reload the store. */
async function writeAndReload(op) {
  await op();
  try { await api("POST", "/reload", { body: {} }); } catch { /* reload best-effort */ }
}

/* ===========================================================================
 * Toast (bottom-right stack, auto-dismiss)
 * ======================================================================== */
const TOAST_ICONS = { ok: "check", err: "alert", info: "info" };
function toast(message, kind = "info", { title } = {}) {
  const root = $("#toast-root");
  const t = el("div", { class: `toast ${kind}` },
    el("span", { class: "ti" }, icon(TOAST_ICONS[kind] || "info", 18)),
    el("div", { class: "tm" },
      title ? el("strong", { text: title }) : null,
      el("div", { class: "ts", text: message }),
    ),
    el("button", { class: "tx", "aria-label": "dismiss", title: "dismiss",
      onClick: () => dismiss(t) }, icon("close", 15)),
  );
  root.appendChild(t);
  const timer = setTimeout(() => dismiss(t), 3500);
  t._timer = timer;
  return t;
  function dismiss(node) {
    if (node._timer) clearTimeout(node._timer);
    if (!node.parentNode) return;
    node.classList.add("leaving");
    setTimeout(() => node.remove(), 200);
  }
}

/* ===========================================================================
 * Modal system
 * ======================================================================== */
let ACTIVE_MODAL = null;

/* One modal at a time. Returns a controller with close() + an onClose hook
 * (used by confirmDialog to resolve its promise). No self-recursion: ctrl.close
 * tears down the DOM then calls the user-provided onClose exactly once. */
function openModal({ icon: iconName, iconKind = "", title, sub, body, size = "", actions = [] }) {
  closeModal();
  const overlay = el("div", { class: "modal-overlay" });
  const ctrl = { overlay, closed: false, onClose: null };
  ctrl.close = function close(result) {
    if (ctrl.closed) return;
    ctrl.closed = true;
    if (ACTIVE_MODAL === ctrl) ACTIVE_MODAL = null;
    overlay.remove();
    if (typeof ctrl.onClose === "function") ctrl.onClose(result);
  };
  const modal = el("div", { class: `modal ${size}` },
    el("div", { class: "modal-head" },
      iconName ? el("div", { class: `modal-icon ${iconKind}` }, icon(iconName, 18)) : null,
      el("div", {},
        el("h3", { text: title }),
        sub ? el("p", { class: "modal-sub", text: sub }) : null,
      ),
      el("button", { class: "icon-btn close", "aria-label": "close", title: "close (esc)",
        onClick: () => ctrl.close(false) }, icon("close", 17)),
    ),
    el("div", { class: "modal-body" }, body),
    actions.length ? el("div", { class: "modal-foot" },
      el("div", { class: "spacer" }),
      ...actions,
    ) : null,
  );
  overlay.appendChild(modal);
  overlay.addEventListener("mousedown", (e) => { if (e.target === overlay) ctrl.close(false); });
  $("#modal-root").appendChild(overlay);
  ACTIVE_MODAL = ctrl;
  return ctrl;
}
function closeModal(result) { if (ACTIVE_MODAL) ACTIVE_MODAL.close(result); }

document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && ACTIVE_MODAL) closeModal(false);
});

/** Promise-based confirm dialog (replaces native confirm()). */
function confirmDialog({ title = "Are you sure?", message, target, confirmText = "Delete", danger = true }) {
  return new Promise((resolve) => {
    let done = false;
    const settle = (v) => { if (!done) { done = true; closeModal(); resolve(v); } };
    const body = el("div", { class: "confirm-body" },
      el("div", { class: "ci" }, icon("alert", 22)),
      el("div", { class: "ct" },
        el("p", { text: message }),
        target ? el("div", { class: "target", text: target }) : null,
      ),
    );
    const m = openModal({
      title, body, size: "",
      actions: [
        el("button", { class: "btn", text: "Cancel", onClick: () => settle(false) }),
        el("button", { class: `btn ${danger ? "danger solid" : "primary"}`, text: confirmText,
          onClick: () => settle(true) }),
      ],
    });
    m.onClose = () => settle(false);
  });
}

/* ===========================================================================
 * Entity configuration (the 8 CRUD entities)
 * ======================================================================== */
const STATUS_OPTS = [
  { value: "1", label: "online" },
  { value: "0", label: "offline · manual" },
  { value: "-1", label: "offline · probe" },
];
const WINDOW_OPTS = [
  { value: "m", label: "minute" },
  { value: "h", label: "hour" },
  { value: "d", label: "day" },
];

function statusPill(s) {
  const map = { 1: ["ok", "online"], 0: ["warn", "offline"], [-1]: ["err", "probe"] };
  const [cls, label] = map[s] ?? ["", String(s)];
  return el("span", { class: `pill ${cls}`, text: label });
}
function boolPill(v) {
  return el("span", { class: `pill ${v ? "ok" : "warn"}`, text: v ? "true" : "false" });
}

const CRUD = {
  providers: {
    title: "Providers", nav: "Providers", icon: "providers", path: "/providers", singular: "provider",
    desc: "Upstream LLM providers — endpoint + routing weight.",
    clearsFK: ["providers"],
    columns: [
      { key: "id", label: "ID", mono: true },
      { key: "key", label: "Key", mono: true },
      { key: "name", label: "Name" },
      { key: "endpoint", label: "Endpoint", mono: true, truncate: true },
      { key: "weight", label: "Weight", align: "right", num: true },
      { key: "updated_at", label: "Updated", mono: true, muted: true },
    ],
    fields: [
      { name: "id", label: "ID", placeholder: "auto if blank", tip: "leave blank to auto-assign" },
      { name: "key", label: "Key", required: true, placeholder: "openai" },
      { name: "name", label: "Display name", required: true, placeholder: "OpenAI" },
      { name: "endpoint", label: "Endpoint", type: "url", required: true, placeholder: "https://api.openai.com", full: true },
      { name: "weight", label: "Weight", type: "number", map: "int", value: 1, tip: "SWRR weight; 0 = soft-disabled" },
    ],
  },
  "provider-models": {
    title: "Provider Models", nav: "Models", icon: "models", path: "/provider-models", singular: "model",
    desc: "Models exposed by each provider (the routing key).",
    columns: [
      { key: "id", label: "ID", mono: true },
      { key: "key", label: "Key", mono: true },
      { key: "name", label: "Name" },
      { key: "provider_id", label: "Provider", fk: "providers" },
      { key: "status", label: "Status", render: (v) => statusPill(v) },
    ],
    fields: [
      { name: "id", label: "ID", placeholder: "auto if blank" },
      { name: "key", label: "Key (model_key)", required: true, placeholder: "gpt-4" },
      { name: "name", label: "Display name", required: true, placeholder: "GPT-4" },
      { name: "provider_id", label: "Provider", type: "select", fk: "providers", required: true },
      { name: "status", label: "Status", type: "select", options: STATUS_OPTS, map: "int", value: 1, required: true },
    ],
  },
  "provider-keys": {
    title: "Provider Keys", nav: "Keys", icon: "keys", path: "/provider-keys", singular: "key",
    desc: "API keys stored per provider. Masked by default; reveal is audit-logged.",
    maskedKeys: true,
    noEdit: true,
    columns: [
      { key: "id", label: "ID", mono: true },
      { key: "provider_id", label: "Provider", fk: "providers" },
      { key: "api_key", label: "API key", mono: true, render: (v) => el("span", { class: "mono", text: v || "—" }) },
      { key: "created_at", label: "Created", mono: true, muted: true },
    ],
    fields: [
      { name: "id", label: "ID", placeholder: "auto if blank" },
      { name: "provider_id", label: "Provider", type: "select", fk: "providers", required: true },
      { name: "api_key", label: "API key", type: "password", required: true, placeholder: "sk-…", full: true,
        tip: "stored masked after create" },
    ],
  },
  tenants: {
    title: "Tenants", nav: "Tenants", icon: "tenants", path: "/tenants", singular: "tenant",
    desc: "Tenants identified by Host domain, with an external auth endpoint.",
    clearsFK: ["tenants"],
    columns: [
      { key: "id", label: "ID", mono: true },
      { key: "name", label: "Name" },
      { key: "domain", label: "Domain", mono: true },
      { key: "auth_url", label: "Auth URL", mono: true, truncate: true },
      { key: "has_access_token", label: "Token", render: (v) => (v ? "set" : "—") },
      { key: "enabled", label: "Enabled", render: (v) => boolPill(v) },
    ],
    fields: [
      { name: "id", label: "ID", placeholder: "auto if blank" },
      { name: "name", label: "Name", required: true, placeholder: "Acme" },
      { name: "domain", label: "Domain", required: true, placeholder: "acme.com", tip: "lowercased; localhost is special" },
      { name: "auth_url", label: "Auth URL", type: "url", required: true, placeholder: "https://auth.acme.com/v1/verify", full: true,
        tip: "mandatory — empty ⇒ all requests 401",
        action: { label: "Test", action: (input) => testAuthUrl(input) } },
      { name: "access_token", label: "Access token", type: "password", map: "opt", full: true,
        placeholder: "blank = keep current token",
        tip: "tenant self-service token: POST /api/v1/tenants/{id}/auth/cache/invalidate with it to force re-auth (欠费停机 / 付费恢复). Blank on edit keeps the current token; a new value rotates it; never shown again after save.",
        action: { label: "Generate", action: (input) => generateToken(input) } },
      { name: "cert_pem", label: "Cert PEM", type: "textarea", full: true, rows: 4, map: "opt",
        placeholder: "-----BEGIN CERTIFICATE----- …",
        tip: "optional — leave blank for no cert (or to keep the current cert on edit); clearing via the API uses cert_pem:\"\"" },
      { name: "cert_key_pem", label: "Cert private key PEM", type: "textarea", full: true, rows: 4, map: "opt",
        placeholder: "-----BEGIN PRIVATE KEY----- …",
        tip: "optional; required when Cert PEM is set — stored encrypted, never returned by the API; left blank ⇒ no cert" },
      { name: "enabled", label: "Enabled", type: "checkbox", map: "bool", value: true },
    ],
  },
  "tenant-providers": {
    title: "Tenant ↔ Provider Access", nav: "Tenant Access", icon: "access", path: "/tenant-providers", singular: "access",
    desc: "Which providers each tenant may route to.",
    noEdit: true,
    columns: [
      { key: "id", label: "ID", mono: true },
      { key: "tenant_id", label: "Tenant", fk: "tenants" },
      { key: "provider_id", label: "Provider", fk: "providers" },
    ],
    fields: [
      { name: "id", label: "ID", placeholder: "auto if blank" },
      { name: "tenant_id", label: "Tenant", type: "select", fk: "tenants", required: true },
      { name: "provider_id", label: "Provider", type: "select", fk: "providers", required: true },
    ],
  },
  "tenant-models": {
    title: "Tenant Models", nav: "Tenant Models", icon: "gate", path: "/tenant-models", singular: "model gate",
    desc: "Model gate — default-open: a tenant with NO rows here can request ALL models; once any row exists, only listed models are allowed.",
    noEdit: true,
    columns: [
      { key: "id", label: "ID", mono: true },
      { key: "tenant_id", label: "Tenant", fk: "tenants" },
      { key: "model_key", label: "Model key", mono: true },
    ],
    fields: [
      { name: "id", label: "ID", placeholder: "auto if blank" },
      { name: "tenant_id", label: "Tenant", type: "select", fk: "tenants", required: true },
      { name: "model_key", label: "Model key", required: true, placeholder: "gpt-4" },
    ],
  },
  "limit-roles": {
    title: "Limit Roles", nav: "Limit Roles", icon: "limits", path: "/limit-roles", singular: "limit role",
    desc: "Rate-limit roles matched by tenant / key / model / provider.",
    columns: [
      { key: "name", label: "Name" },
      { key: "matching_tenant", label: "Tenant", mono: true, render: (v) => monoOrAll(v) },
      { key: "matching_key", label: "Key", mono: true, render: (v) => monoOrAll(v) },
      { key: "matching_model", label: "Model", mono: true, render: (v) => monoOrAll(v) },
      { key: "matching_provider", label: "Provider", mono: true, render: (v) => monoOrAll(v) },
      { key: "limit_count", label: "Count", align: "right", num: true, render: (v) => numOrDash(v) },
      { key: "limit_token", label: "Token", align: "right", num: true, render: (v) => numOrDash(v) },
      { key: "window", label: "Window", mono: true },
      { key: "enabled", label: "Enabled", render: (v) => boolPill(v) },
    ],
    fields: [
      { name: "name", label: "Name", required: true, placeholder: "acme-default" },
      { name: "matching_tenant", label: "Match tenant", placeholder: "blank = all", map: "opt", tip: "tenant id" },
      { name: "matching_key", label: "Match key", placeholder: "blank = all", map: "opt" },
      { name: "matching_model", label: "Match model", placeholder: "blank = all", map: "opt" },
      { name: "matching_provider", label: "Match provider", placeholder: "blank = all", map: "opt" },
      { name: "limit_count", label: "Limit (count)", type: "number", map: "optint", placeholder: "requests" },
      { name: "limit_token", label: "Limit (tokens)", type: "number", map: "optint", placeholder: "tokens" },
      { name: "window", label: "Window", type: "select", options: WINDOW_OPTS, value: "m" },
      { name: "enabled", label: "Enabled", type: "checkbox", map: "bool", value: true },
    ],
  },
  "provider-key-bindings": {
    title: "Key Prefix Bindings", nav: "Key Bindings", icon: "key2", path: "/provider-key-bindings", singular: "binding",
    desc: "Route gate — client api-keys whose raw value starts with a prefix are pinned to one provider (longest prefix wins, fail-closed).",
    columns: [
      { key: "id", label: "ID", mono: true },
      { key: "key_prefix", label: "Prefix", mono: true },
      { key: "provider_id", label: "Provider", fk: "providers" },
      { key: "enabled", label: "Enabled", render: (v) => boolPill(v) },
    ],
    fields: [
      { name: "id", label: "ID", placeholder: "auto if blank" },
      { name: "key_prefix", label: "Key prefix", required: true, placeholder: "sk_aaa_",
        tip: "client api-key prefix; e.g. sk_aaa_ → keys starting with sk_aaa_ use this provider" },
      { name: "provider_id", label: "Provider", type: "select", fk: "providers", required: true },
      { name: "enabled", label: "Enabled", type: "checkbox", map: "bool", value: true },
    ],
  },
};

function monoOrAll(v) {
  return v ? el("span", { class: "mono", text: v }) : el("span", { class: "muted", text: "*" });
}
function numOrDash(v) {
  return v === null || v === undefined || v === ""
    ? el("span", { class: "muted", text: "—" })
    : el("span", { class: "mono", text: String(v) });
}

/* ---- custom sections (auth-cache / breaker / health) ---- */
const CUSTOM = {
  "auth-cache": {
    title: "Auth Cache", nav: "Auth Cache", icon: "authcache",
    desc: "Force re-authentication for cached verdicts.",
    render: renderAuthCache,
  },
  breaker: {
    title: "Circuit Breaker", nav: "Breaker", icon: "breaker",
    desc: "Dead-set of providers excluded from routing.",
    render: renderBreaker,
  },
  health: {
    title: "Health", nav: "Health", icon: "health",
    desc: "Live service status.",
    render: renderHealth,
  },
  "api-docs": {
    title: "API Docs", nav: "API Docs", icon: "book",
    desc: "OpenAPI-style reference for every admin REST endpoint (curl / Python / TypeScript examples).",
    render: renderApiDocs,
  },
};

/* ordered nav with section dividers */
const NAV = [
  { label: "Configuration", items: ["providers", "provider-models", "provider-keys", "tenants", "tenant-providers", "tenant-models", "limit-roles", "provider-key-bindings"] },
  { label: "Operations", items: ["auth-cache", "breaker", "health"] },
  { label: "Reference", items: ["api-docs"] },
];
function sectionConfig(key) { return CRUD[key] || CUSTOM[key]; }

/* ===========================================================================
 * Navigation
 * ======================================================================== */
function renderNav() {
  const nav = $("#nav");
  clear(nav);
  for (const group of NAV) {
    nav.appendChild(el("div", { class: "nav-section", text: group.label }));
    for (const key of group.items) {
      const cfg = sectionConfig(key);
      const btn = el("button", {
        class: `nav-item ${key === CURRENT ? "active" : ""}`,
        dataset: { key },
        onClick: () => go(key),
      },
        icon(cfg.icon, 17),
        el("span", { text: cfg.nav }),
        el("span", { class: "nav-badge", dataset: { badge: key }, text: "" }),
      );
      if (!CRUD[key]) btn.querySelector(".nav-badge").classList.add("hidden");
      nav.appendChild(btn);
    }
  }
}
function setNavBadge(key, n) {
  const b = $(`.nav-badge[data-badge="${key}"]`);
  if (b) { b.textContent = n > 0 ? String(n) : ""; b.classList.toggle("hidden", !(n > 0)); }
}

/* ===========================================================================
 * Content rendering
 * ======================================================================== */
function go(key) {
  CURRENT = key;
  $$(".nav-item").forEach((b) => b.classList.toggle("active", b.dataset.key === key));
  closeSidebar();
  const cfg = sectionConfig(key);
  $("#page-title").textContent = cfg.title;
  $("#page-sub").textContent = cfg.desc || "";
  clear($("#page-actions"));
  clear($("#content"));
  if (CRUD[key]) loadEntity(key);
  else if (CUSTOM[key]) CUSTOM[key].render();
}

function showSkeleton(content) {
  clear(content);
  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "table-wrap" },
      el("table", {},
        el("tbody", {},
          ...Array.from({ length: 4 }).map(() =>
            el("tr", { class: "skeleton-row" },
              el("td", {}, el("span", { class: "skeleton" })),
              el("td", {}, el("span", { class: "skeleton", style: "width:40%" })),
              el("td", {}, el("span", { class: "skeleton", style: "width:70%" })),
              el("td", {}, el("span", { class: "skeleton", style: "width:30%" })),
            ),
          ),
        ),
      ),
    ),
  ));
}

async function loadEntity(key) {
  const cfg = CRUD[key];
  const content = $("#content");
  showSkeleton(content);
  let rows = [];
  try {
    rows = await api("GET", cfg.path) || [];
    // preload FKs the table needs
    const fks = [...new Set(cfg.columns.filter((c) => c.fk).map((c) => c.fk))];
    if (fks.length) await Promise.all(fks.map(ensureFK));
  } catch (e) {
    renderErrorPanel(e);
    toast(e.message, "err", { title: `Failed to load ${cfg.nav}` });
    return;
  }
  COUNTS[key] = rows.length;
  setNavBadge(key, rows.length);
  renderEntityPanel(cfg, rows);
}

function renderErrorPanel(e) {
  clear($("#content"));
  $("#content").appendChild(emptyState("alert", "Couldn't load", e.message || "unknown error"));
}

function renderEntityPanel(cfg, rows) {
  const content = $("#content");
  clear(content);

  const panel = el("div", { class: "panel" });
  const head = el("div", { class: "panel-head" },
    el("h2", {}, el("span", { text: cfg.title }), " ",
      el("span", { class: "count", text: String(rows.length) })),
    el("div", { class: "spacer" }),
  );

  // keys reveal toggle
  if (cfg.maskedKeys) {
    head.appendChild(el("label", { class: "toggle-pill" },
      el("input", { type: "checkbox", id: "keys-reveal",
        onChange: (e) => { STATE.revealKeys = e.target.checked; loadEntity("provider-keys"); } }),
      el("span", { text: "reveal plaintext" }),
    ));
  }
  head.appendChild(el("button", { class: "btn primary sm", onClick: () => openCreate(cfg) },
    icon("plus", 14), el("span", { class: "btn-label", text: `New ${cfg.singular}` }),
  ));
  panel.appendChild(head);

  if (!rows.length) {
    panel.appendChild(emptyState("inbox", `No ${cfg.nav.toLowerCase()} yet`,
      `Create your first ${cfg.singular} to get started.`, { primary: `+ New ${cfg.singular}`, onClick: () => openCreate(cfg) }));
  } else {
    panel.appendChild(renderTable(cfg, rows));
  }
  content.appendChild(panel);
}

const STATE = { revealKeys: false };

function renderTable(cfg, rows) {
  const cols = cfg.columns;
  const thead = el("thead", {}, el("tr", {},
    ...cols.map((c) => el("th", { class: c.align === "right" ? "num" : "", text: c.label })),
    el("th", { text: "" }),
  ));
  const tbody = el("tbody", {});
  for (const r of rows) {
    const tr = el("tr", {},
      ...cols.map((c) => {
        const td = el("td", {});
        if (c.align === "right") td.className = "num";
        const node = renderCell(c, r);
        append(td, node);
        if (c.truncate) td.classList.add("truncate");
        return td;
      }),
      el("td", { class: "actions" },
        el("div", { class: "row-actions" },
          cfg.noEdit ? null : iconBtn("edit", "Edit", () => openEdit(cfg, r)),
          iconBtn("trash", "Delete", () => doDelete(cfg, r), "danger"),
        ),
      ),
    );
    tbody.appendChild(tr);
  }
  return el("div", { class: "table-wrap" }, el("table", {}, thead, tbody));
}

function renderCell(col, row) {
  const v = row[col.key];
  if (col.render) return col.render(v, row);
  if (col.fk) {
    const fk = FK[col.fk]; const r = fk && fk.map.get(String(v));
    const label = r ? (r.name || r.id) : v;
    return label === null || label === undefined || label === ""
      ? el("span", { class: "muted", text: "—" })
      : el("span", { class: "mono", text: String(label) });
  }
  if (v === null || v === undefined || v === "") return el("span", { class: "muted", text: "—" });
  if (col.muted && col.mono) return el("span", { class: "muted mono", text: shortTime(v) });
  if (col.mono) return el("span", { class: "mono", text: String(v) });
  return document.createTextNode(String(v));
}
function shortTime(s) {
  if (typeof s !== "string" || !s) return s;
  // compact ISO-ish timestamps to YYYY-MM-DD HH:MM:SS
  return s.length > 19 ? s.slice(0, 19).replace("T", " ") : s;
}

function iconBtn(name, label, onClick, extra = "") {
  return el("button", { class: `icon-btn ${extra}`, title: label, "aria-label": label, onClick },
    icon(name, 16));
}

function emptyState(iconName, title, msg, action) {
  const body = el("div", { class: "empty" },
    icon(iconName, 38),
    el("h3", { text: title }),
    el("p", { text: msg }),
  );
  if (action) body.appendChild(el("button", { class: "btn primary sm", style: "margin-top:14px",
    onClick: action.onClick }, icon("plus", 14), action.primary));
  return body;
}

/* ===========================================================================
 * Modal form (generic)
 * ======================================================================== */
async function openCreate(cfg) { await openForm(cfg, null); }
async function openEdit(cfg, record) { await openForm(cfg, record); }

async function openForm(cfg, record) {
  const isEdit = !!record;
  // preload FK dropdowns
  const fks = [...new Set(cfg.fields.filter((f) => f.fk).map((f) => f.fk))];
  if (fks.length) await Promise.all(fks.map(ensureFK));

  const form = el("form", { class: "form-grid", onSubmit: (e) => e.preventDefault() });
  const inputs = {};
  for (const f of cfg.fields) {
    const val = isEdit ? record[f.name] : (f.value !== undefined ? f.value : (f.type === "checkbox" ? false : ""));
    const isId = f.name === "id";
    const disabled = isId && isEdit;
    const group = buildField(f, val, { disabled, isEdit });
    form.appendChild(group);
    inputs[f.name] = group.querySelector("[data-field]");
  }

  const submitBtn = el("button", { class: "btn primary", type: "submit" },
    el("span", { class: "btn-label", text: isEdit ? "Save changes" : `Create ${cfg.singular}` }));

  const body = form;
  const m = openModal({
    icon: isEdit ? "edit" : "plus", title: isEdit ? `Edit ${cfg.singular}` : `New ${cfg.singular}`,
    sub: cfg.desc, body, size: "lg",
    actions: [
      el("button", { class: "btn", text: "Cancel", onClick: () => closeModal() }),
      submitBtn,
    ],
  });

  submitBtn.addEventListener("click", (e) => { e.preventDefault(); submit(); });
  form.addEventListener("submit", (e) => { e.preventDefault(); submit(); });

  // focus first editable field
  const first = form.querySelector("[data-field]:not([disabled])");
  if (first) setTimeout(() => first.focus(), 60);

  async function submit() {
    // validate
    let firstInvalid = null;
    for (const f of cfg.fields) {
      const input = inputs[f.name];
      const ok = validateField(f, input);
      if (!ok && !firstInvalid) firstInvalid = input;
    }
    if (firstInvalid) { firstInvalid.focus(); toast("Please complete the highlighted fields", "err"); return; }

    setLoading(submitBtn, true, isEdit ? "Saving…" : "Creating…");
    try {
      const bodyObj = collectBody(cfg, inputs, record);
      await writeAndReload(() =>
        isEdit ? api("PUT", `${cfg.path}/${record.id}`, { body: bodyObj })
               : api("POST", cfg.path, { body: bodyObj }));
      (cfg.clearsFK || []).forEach(invalidateFK);
      closeModal();
      toast(`${isEdit ? "Updated" : "Created"} ${cfg.singular}`, "ok");
      await loadEntity(CURRENT);
    } catch (e) {
      setLoading(submitBtn, false);
      toast(e.message, "err", { title: `Failed to ${isEdit ? "update" : "create"} ${cfg.singular}` });
    }
  }
}

function buildField(f, value, { disabled, isEdit } = {}) {
  const group = el("div", { class: `input-group ${f.full ? "full" : ""}` });
  if (f.type === "checkbox") {
    group.classList.add("full");
    const c = el("label", { class: "check" },
      el("input", { type: "checkbox", dataset: { field: f.name }, disabled }),
      el("span", { text: `${f.label}${f.required ? " *" : ""}` }),
    );
    const input = c.querySelector("input");
    input.checked = !!value;
    group.appendChild(c);
    return group;
  }
  group.appendChild(el("label", { text: f.label },
    f.required ? el("span", { class: "req", text: " *" }) : null));
  let input;
  if (f.type === "textarea") {
    input = el("textarea", { dataset: { field: f.name }, disabled, rows: f.rows || 3,
      placeholder: f.placeholder || "" });
    input.value = value === null || value === undefined ? "" : String(value);
  } else if (f.type === "select") {
    input = el("select", { dataset: { field: f.name }, disabled });
    if (f.fk) {
      const fk = FK[f.fk] || { list: [] };
      if (!f.required) input.appendChild(el("option", { value: "", text: "— none —" }));
      if (!fk.list.length) input.appendChild(el("option", { value: "", text: `(no ${f.fk} yet)` }));
      for (const r of fk.list) input.appendChild(el("option", { value: String(r.id), text: `${r.name || r.id} · ${r.id}` }));
    } else {
      for (const o of (f.options || [])) input.appendChild(el("option", { value: o.value, text: o.label }));
    }
    const targetVal = String(value ?? "");
    if (targetVal !== "") input.value = targetVal;        // edit / explicit default
    else if (!f.required) input.value = "";                // select the "— none —" option
    // required + empty (create) → leave the first option as the default selection
  } else {
    input = el("input", {
      type: f.type === "password" ? "password" : (f.type === "number" ? "number" : (f.type === "url" ? "url" : "text")),
      dataset: { field: f.name }, disabled,
      placeholder: f.placeholder || "",
    });
    input.value = value === null || value === undefined ? "" : String(value);
    if (isEdit && f.name === "id") input.classList.add("mono");
  }
  if (f.action) {
    // Input + an inline action button (e.g. the Tenants "Test" auth-URL
    // probe or the "Generate" access-token button). The button is
    // type="button" so it never submits the form.
    const wrap = el("div", { class: "input-wrap" });
    input.style.flex = "1";
    wrap.appendChild(input);
    wrap.appendChild(el("button", { class: "btn sm ghost", type: "button", text: f.action.label || "Action",
      onClick: () => f.action.action(input, group) }));
    group.appendChild(wrap);
  } else {
    group.appendChild(input);
  }
  const err = el("div", { class: "field-error hidden", text: "" });
  group.appendChild(err);
  if (f.tip) group.appendChild(el("div", { class: "field-tip", text: f.tip }));
  return group;
}

function validateField(f, input) {
  const err = input.closest(".input-group").querySelector(".field-error");
  if (f.type === "checkbox") { hideErr(); return true; }
  const val = (input.value || "").trim();
  if (f.required && !val) {
    input.classList.add("invalid");
    if (err) { err.textContent = `${f.label} is required`; err.classList.remove("hidden"); }
    return false;
  }
  input.classList.remove("invalid");
  if (err) err.classList.add("hidden");
  return true;
  function hideErr() { input.classList.remove("invalid"); if (err) err.classList.add("hidden"); }
}

function readValue(f, input) {
  if (input.type === "checkbox") return !!input.checked;
  const v = input.value;
  switch (f.map) {
    case "int": return v === "" ? 0 : parseInt(v, 10);
    case "optint": return v === "" ? null : parseInt(v, 10);
    case "bool": return !!input.checked;
    case "opt": return v === "" ? null : v;
    default: return v;
  }
}
function collectBody(cfg, inputs, record) {
  const body = {};
  for (const f of cfg.fields) body[f.name] = readValue(f, inputs[f.name]);
  // preserve server-managed timestamps on edit
  if (record && record.created_at) body.created_at = record.created_at;
  if (record && record.updated_at) body.updated_at = record.updated_at;
  if (!record) {
    // The admin API deserialises the request body directly into the entity
    // structs, where the timestamp columns are REQUIRED fields (the server
    // defaults them when empty, see admin/handlers.rs). Send them on create so
    // the POST succeeds; serde ignores them for entities without timestamps.
    body.created_at = body.created_at ?? "";
    body.updated_at = body.updated_at ?? "";
  }
  return body;
}

function setLoading(btn, loading, label) {
  if (loading) {
    btn.disabled = true;
    btn._prevHTML = btn.innerHTML;
    btn.innerHTML = "";
    btn.appendChild(el("span", { class: "spinner" }));
    if (label) btn.appendChild(document.createTextNode(" " + label));
  } else {
    btn.disabled = false;
    if (btn._prevHTML !== undefined) { btn.innerHTML = btn._prevHTML; }
  }
}

/* ===========================================================================
 * Tenant auth-url probe ("Test" button on the Tenants form)
 * ======================================================================== */
async function testAuthUrl(input) {
  const url = (input.value || "").trim();
  if (!url) { toast("Enter an auth URL first", "err"); return; }
  const wrap = input.closest(".input-wrap");
  const btn = wrap ? wrap.querySelector("button") : null;
  if (btn) setLoading(btn, true, "Testing…");
  try {
    const r = await api("POST", "/tenants/auth/test", { body: { auth_url: url } });
    const ms = typeof r.duration_ms === "number" ? " · " + r.duration_ms + "ms" : "";
    const head = typeof r.status === "number" ? "HTTP " + r.status + ms : ms;
    if (r.ok) toast(r.detail + " (" + head + ")", "ok", { title: "Auth URL OK" });
    else toast(r.detail + " (" + head + ")", "err", { title: "Auth URL test failed" });
  } catch (e) {
    toast(e.message, "err", { title: "Auth URL test failed" });
  } finally {
    if (btn) setLoading(btn, false);
  }
}

/* ---- tenant access-token generator (Generate button, migration 0009) ---- */
function generateToken(input) {
  const bytes = new Uint8Array(24);
  crypto.getRandomValues(bytes);
  const token = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
  input.value = token;
  // reveal once so the operator can copy it — the API never returns it
  input.type = "text";
  setTimeout(() => { input.type = "password"; }, 20000);
  toast("Random access token generated — copy it now; it is never shown again", "info", { title: "Access token" });
}


/* ===========================================================================
 * Delete
 * ======================================================================== */
async function doDelete(cfg, record) {
  const label = record.name || record.key || record.id;
  const ok = await confirmDialog({
    title: `Delete ${cfg.singular}?`,
    message: `This will permanently remove the ${cfg.singular}.`,
    target: `${cfg.nav}: ${label} (${record.id})`,
    confirmText: "Delete",
  });
  if (!ok) return;
  try {
    await writeAndReload(() => api("DELETE", `${cfg.path}/${record.id}`));
    (cfg.clearsFK || []).forEach(invalidateFK);
    toast(`Deleted ${cfg.singular}`, "ok");
    await loadEntity(CURRENT);
  } catch (e) {
    toast(e.message, "err", { title: `Failed to delete ${cfg.singular}` });
  }
}

/* ===========================================================================
 * Custom: Auth Cache
 * ======================================================================== */
function renderAuthCache() {
  const content = $("#content");
  clear(content);
  const panel = el("div", { class: "panel" },
    el("div", { class: "panel-head" },
      el("h2", {}, el("span", { text: "Auth Cache" })), el("div", { class: "spacer" })),
    el("p", { class: "note" },
      "Invalidation forces the next request for the given key/tenant to re-authenticate against ",
      el("code", { text: "tenant.auth_url" }), ". Leave both blank to clear everything."),
    el("div", { class: "field-row" },
      el("div", { class: "input-group" },
        el("label", { text: "tenant_id (optional)" }),
        el("input", { type: "text", id: "inv-tenant", placeholder: "e.g. t1" }),
      ),
      el("div", { class: "input-group" },
        el("label", { text: "api_keys (comma-separated, optional)" }),
        el("input", { type: "text", id: "inv-keys", placeholder: "sk-aaa, sk-bbb" }),
      ),
    ),
    el("div", { style: "margin-top:14px" },
      el("button", { class: "btn primary", onClick: doInvalidate }, icon("trash", 15), " Invalidate"),
    ),
    el("div", { id: "inv-result", class: "hidden", style: "margin-top:14px" }),
  );
  content.appendChild(panel);
}

async function doInvalidate() {
  const tenant = $("#inv-tenant").value.trim();
  const keysRaw = $("#inv-keys").value.trim();
  const keys = keysRaw ? keysRaw.split(",").map((s) => s.trim()).filter(Boolean) : null;
  if (!tenant && !keys) { toast("Provide tenant_id and/or api_keys", "err"); return; }
  const body = {};
  if (tenant) body.tenant_id = tenant;
  if (keys) body.api_keys = keys;
  const box = $("#inv-result");
  try {
    const r = await api("DELETE", "/auth/cache", { body });
    box.className = "alert ok";
    box.textContent = `Invalidated ${r.invalidated} entr${r.invalidated === 1 ? "y" : "ies"}.`;
    box.classList.remove("hidden");
    toast(`Invalidated ${r.invalidated} entr${r.invalidated === 1 ? "y" : "ies"}`, "ok");
  } catch (e) {
    box.className = "alert err";
    box.textContent = e.message;
    box.classList.remove("hidden");
    toast(e.message, "err");
  }
}

/* ===========================================================================
 * Custom: Breaker
 * ======================================================================== */
function renderBreaker() {
  const content = $("#content");
  clear(content);
  const panel = el("div", { class: "panel" },
    el("div", { class: "panel-head" },
      el("h2", {}, el("span", { text: "Dead-set" }), " ", el("span", { class: "count", id: "breaker-count", text: "" })),
      el("div", { class: "spacer" }),
      el("button", { class: "btn sm", onClick: loadBreaker }, icon("refresh", 14), el("span", { class: "btn-label", text: "Refresh" })),
    ),
    el("p", { class: "note" }, "Dead providers are excluded from candidate selection. A background probe revives them; force a reset here."),
    el("div", { id: "breaker-table-wrap" }),
  );
  content.appendChild(panel);
  // manual reset form
  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "panel-head" }, el("h2", {}, el("span", { text: "Force reset" }))),
    el("div", { class: "field-row" },
      el("div", { class: "input-group" },
        el("label", { text: "provider_id" }),
        el("input", { type: "text", id: "breaker-reset-id", placeholder: "provider_id" }),
      ),
    ),
    el("div", { style: "margin-top:12px" },
      el("button", { class: "btn primary", onClick: resetBreakerById }, icon("refresh", 15), " Reset"),
    ),
  ));
  loadBreaker();
}

async function loadBreaker() {
  const wrap = $("#breaker-table-wrap");
  if (!wrap) return;
  let dead = [];
  try {
    const r = await api("GET", "/breaker");
    dead = Array.isArray(r) ? r : (r?.dead || []);
  } catch (e) { toast(e.message, "err"); return; }
  const cnt = $("#breaker-count"); if (cnt) cnt.textContent = String(dead.length);
  clear(wrap);
  if (!dead.length) {
    wrap.appendChild(emptyState("check", "No dead providers", "All candidates are selectable."));
    return;
  }
  wrap.appendChild(el("div", { class: "table-wrap" },
    el("table", {},
      el("thead", {}, el("tr", {}, el("th", { text: "provider_id" }), el("th", { text: "state" }), el("th", { text: "" }))),
      el("tbody", {}, ...dead.map((pid) =>
        el("tr", {},
          el("td", {}, el("span", { class: "mono", text: pid })),
          el("td", {}, el("span", { class: "pill dead", text: "DEAD" })),
          el("td", { class: "actions" }, iconBtn("refresh", "Reset", () => resetBreaker(pid))),
        ),
      )),
    ),
  ));
}
async function resetBreaker(id) {
  try {
    await api("DELETE", `/breaker/${encodeURIComponent(id)}`);
    toast(`Reset ${id}`, "ok");
    await loadBreaker();
  } catch (e) { toast(e.message, "err"); }
}
async function resetBreakerById() {
  const id = $("#breaker-reset-id").value.trim();
  if (!id) { toast("provider id required", "err"); return; }
  await resetBreaker(id);
  const inp = $("#breaker-reset-id"); if (inp) inp.value = "";
}

/* ===========================================================================
 * Custom: Health
 * ======================================================================== */
async function renderHealth() {
  const content = $("#content");
  clear(content);
  // actions
  clear($("#page-actions"));
  $("#page-actions").appendChild(el("button", { class: "btn sm", onClick: renderHealth },
    icon("refresh", 14), el("span", { class: "btn-label", text: "Refresh" })));

  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "panel-head" }, el("h2", {}, el("span", { text: "Status" })), el("div", { class: "spacer" })),
    el("div", { class: "stat-grid", id: "health-stats" },
      el("span", { class: "skeleton", style: "width:100%;height:60px" }),
    ),
  ));
  // Whole-cluster view (cluster P4): fleet nodes + lease holder.
  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "panel-head" }, el("h2", {}, el("span", { text: "Cluster" })), el("div", { class: "spacer" })),
    el("div", { class: "stat-grid", id: "cluster-stats" },
      el("span", { class: "skeleton", style: "width:100%;height:60px" }),
    ),
    el("div", { id: "cluster-nodes" }),
  ));
  content.appendChild(el("pre", { class: "json", id: "health-json" }, "{}"));
  try {
    const [h, c] = await Promise.all([
      api("GET", "/health"),
      api("GET", "/cluster/status"),
    ]);
    renderHealthStats(h);
    renderClusterStatus(c);
    $("#health-json").innerHTML = highlightJson({ health: h, cluster: c });
  } catch (e) {
    $("#health-json").textContent = `error: ${e.message}`;
    toast(e.message, "err");
  }
}
function renderClusterStatus(c) {
  const stats = $("#cluster-stats");
  const nodes = $("#cluster-nodes");
  clear(stats); clear(nodes);
  if (!c || !c.cluster) {
    stats.appendChild(el("div", { class: "stat" },
      el("div", { class: "sl", text: "mode" }), el("div", { class: "sv", text: "single-node" })));
    nodes.appendChild(el("p", { class: "muted",
      text: "集群模式未启用（HYDRA_ROLE 未设置）—— 本页仅显示本节点状态。" }));
    return;
  }
  const alive = c.nodes.filter((n) => n.alive).length;
  const cards = [
    { l: "mode", v: c.mode, cls: "" },
    { l: "lease holder", v: c.lease_holder ?? "—", cls: c.lease_holder ? "ok" : "warn" },
    { l: "nodes alive", v: `${alive}/${c.nodes.length}`, cls: alive === c.nodes.length && c.nodes.length > 0 ? "ok" : "warn" },
    { l: "self", v: c.node_id || "—", cls: "" },
  ];
  for (const k of cards) stats.appendChild(el("div", { class: `stat ${k.cls}` },
    el("div", { class: "sl", text: k.l }), el("div", { class: "sv", text: String(k.v) })));

  const th = (t) => el("th", { text: t });
  const rows = [el("tr", {}, th("NODE"), th("ROLE"), th("CONTROL URL"), th("STATE"))];
  for (const n of c.nodes) {
    const name = el("span", { text: n.node_id });
    if (n.is_self) name.appendChild(el("span", { class: "pill info", text: "self" }));
    if (n.is_lease_holder) name.appendChild(el("span", { class: "pill ok", text: "active" }));
    rows.push(el("tr", {},
      el("td", {}, name),
      el("td", {}, el("span", { class: `pill ${n.role === "leader" ? "info" : "warn"}`, text: n.role })),
      el("td", { class: "mono" }, n.control_url || "—"),
      el("td", {}, n.alive
        ? el("span", { class: "pill ok", text: "alive" })
        : el("span", { class: "pill dead", text: "down" })),
    ));
  }
  nodes.appendChild(el("div", { class: "table-wrap" },
    el("table", {}, el("thead", {}, rows[0]), el("tbody", {}, ...rows.slice(1)))));
}
function renderHealthStats(h) {
  const grid = $("#health-stats");
  clear(grid);
  const up = (h.status === "ok" || h.status === "healthy" || h.status === "up");
  const cards = [
    { l: "status", v: h.status ?? "—", cls: up ? "ok" : "err" },
    { l: "db", v: h.db ?? "—", cls: h.db === "ok" ? "ok" : "err" },
    { l: "breaker dead", v: fmtNum(h.breaker_dead), cls: "" },
    { l: "tenants", v: fmtNum(h.tenants), cls: "" },
    { l: "providers", v: fmtNum(h.providers), cls: "" },
  ];
  for (const c of cards) grid.appendChild(el("div", { class: `stat ${c.cls}` },
    el("div", { class: "sl", text: c.l }), el("div", { class: "sv", text: String(c.v) })));
}
function fmtNum(v) { return Array.isArray(v) ? v.length : (v ?? "—"); }

function highlightJson(obj) {
  const json = JSON.stringify(obj, null, 2);
  let html = esc(json);
  html = html.replace(/(&quot;.*?&quot;)(\s*:)?/g, (m, str, colon) =>
    colon ? `<span class="k">${str}</span>${colon}` : `<span class="s">${str}</span>`);
  html = html.replace(/\b(true|false)\b/g, '<span class="b">$&</span>');
  html = html.replace(/\bnull\b/g, '<span class="b">null</span>');
  html = html.replace(/\b(-?\d+\.?\d*)\b/g, '<span class="n">$&</span>');
  return html;
}

/* ===========================================================================
 * Auth / login
 * ======================================================================== */
function showLogin() {
  TOKEN = null;
  document.body.dataset.state = "locked";
  $("#login-overlay").classList.remove("hidden");
  $("#app").setAttribute("aria-hidden", "true");
  const ts = $("#token-status");
  ts.classList.remove("ok"); ts.classList.add("bad"); ts.querySelector(".t").textContent = "not authenticated";
  const inp = $("#login-token"); inp.value = ""; setTimeout(() => inp.focus(), 50);
}
async function tryLogin(token) {
  TOKEN = token;
  const btn = $("#login-btn");
  setLoading(btn, true, "Signing in…");
  try {
    await api("GET", "/health");
    document.body.dataset.state = "ready";
    $("#login-overlay").classList.add("hidden");
    $("#app").setAttribute("aria-hidden", "false");
    const ts = $("#token-status");
    ts.classList.remove("bad"); ts.classList.add("ok"); ts.querySelector(".t").textContent = "authenticated";
    renderNav();
    go(CURRENT);
    toast("Signed in", "ok");
  } catch (e) {
    TOKEN = null;
    const err = $("#login-error");
    err.textContent = `Authentication failed: ${e.message}`;
    err.classList.remove("hidden");
  } finally {
    setLoading(btn, false);
  }
}

/* ===========================================================================
 * Sidebar (mobile)
 * ======================================================================== */
function openSidebar() { document.body.classList.add("nav-open"); }
function closeSidebar() { document.body.classList.remove("nav-open"); }

/* ===========================================================================
 * Wiring
 * ======================================================================== */
function wireEvents() {
  $("#login-form").addEventListener("submit", (e) => { e.preventDefault(); tryLogin($("#login-token").value); });
  $("#logout-btn").addEventListener("click", () => { sessionStorage.removeItem("hydra-admin-ok"); showLogin(); });
  $("#reload-btn").addEventListener("click", async () => {
    try {
      const r = await api("POST", "/reload", { body: {} });
      const n = (p) => (p === undefined ? "" : ` ${p}`);
      toast(`Reloaded${n(r?.providers)} providers,${n(r?.tenants)} tenants`, "ok", { title: "Config reloaded" });
      await loadEntity(CURRENT);
    } catch (e) { toast(e.message, "err"); }
  });
  $("#menu-toggle").addEventListener("click", () => {
    document.body.classList.toggle("nav-open");
  });
  $("#sidebar-scrim").addEventListener("click", closeSidebar);
}

document.addEventListener("DOMContentLoaded", () => {
  wireEvents();
  showLogin();
});
