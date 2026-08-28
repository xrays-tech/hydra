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
  chart:      '<path d="M3 3v18h18"/><rect x="7" y="10" width="3" height="8" rx="1"/><rect x="12" y="6" width="3" height="12" rx="1"/><rect x="17" y="13" width="3" height="5" rx="1"/>',
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
  if (!TOKEN) throw new Error(t("common.auth.notAuthenticated"));
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
  const toastEl = el("div", { class: `toast ${kind}` },
    el("span", { class: "ti" }, icon(TOAST_ICONS[kind] || "info", 18)),
    el("div", { class: "tm" },
      title ? el("strong", { text: title }) : null,
      el("div", { class: "ts", text: message }),
    ),
    el("button", { class: "tx", "aria-label": t("common.action.dismiss"), title: t("common.action.dismiss"),
      onClick: () => dismiss(toastEl) }, icon("close", 15)),
  );
  root.appendChild(toastEl);
  const timer = setTimeout(() => dismiss(toastEl), 3500);
  toastEl._timer = timer;
  return toastEl;
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
      el("button", { class: "icon-btn close", "aria-label": t("common.action.close"), title: t("common.action.closeEsc"),
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
function confirmDialog({ title = t("common.confirm.areYouSure"), message, target, confirmText = t("common.action.delete"), danger = true }) {
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
        el("button", { class: "btn", text: t("common.action.cancel"), onClick: () => settle(false) }),
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
  { value: "1", label: "opts.status.1" },
  { value: "0", label: "opts.status.0" },
  { value: "-1", label: "opts.status.-1" },
];
const WINDOW_OPTS = [
  { value: "m", label: "opts.window.m" },
  { value: "h", label: "opts.window.h" },
  { value: "d", label: "opts.window.d" },
];

function statusPill(s) {
  const map = { 1: ["ok", "common.status.online"], 0: ["warn", "common.status.offline"], [-1]: ["err", "common.status.probe"] };
  const [cls, key] = map[s] ?? ["", String(s)];
  return el("span", { class: `pill ${cls}`, text: key.startsWith("common.") ? t(key) : key });
}
function boolPill(v) {
  return el("span", { class: `pill ${v ? "ok" : "warn"}`, text: t(v ? "common.status.true" : "common.status.false") });
}

const CRUD = {
  providers: {
    title: "crud.providers.title", nav: "crud.providers.nav", icon: "providers", path: "/providers", singular: "crud.providers.singular",
    desc: "crud.providers.desc",
    clearsFK: ["providers"],
    columns: [
      { key: "id", label: "col.id", mono: true },
      { key: "key", label: "col.key", mono: true },
      { key: "name", label: "col.name" },
      { key: "endpoint", label: "col.endpoint", mono: true, truncate: true },
      { key: "weight", label: "col.weight", align: "right", num: true },
      { key: "updated_at", label: "col.updated", mono: true, muted: true },
    ],
    fields: [
      { name: "id", label: "field.id", placeholder: "ph.autoId", tip: "tip.autoId" },
      { name: "key", label: "field.key", required: true, placeholder: "openai" },
      { name: "name", label: "field.displayName", required: true, placeholder: "OpenAI" },
      { name: "endpoint", label: "field.endpoint", type: "url", required: true, placeholder: "https://api.openai.com", full: true },
      { name: "weight", label: "field.weight", type: "number", map: "int", value: 1, tip: "tip.weight" },
    ],
  },
  "provider-models": {
    title: "crud.provider-models.title", nav: "crud.provider-models.nav", icon: "models", path: "/provider-models", singular: "crud.provider-models.singular",
    desc: "crud.provider-models.desc",
    columns: [
      { key: "id", label: "col.id", mono: true },
      { key: "key", label: "col.key", mono: true },
      { key: "name", label: "col.name" },
      { key: "provider_id", label: "col.provider", fk: "providers" },
      { key: "status", label: "col.status", render: (v) => statusPill(v) },
    ],
    fields: [
      { name: "id", label: "field.id", placeholder: "ph.autoId" },
      { name: "key", label: "field.modelKey", required: true, placeholder: "gpt-4" },
      { name: "name", label: "field.displayName", required: true, placeholder: "GPT-4" },
      { name: "provider_id", label: "field.provider", type: "select", fk: "providers", required: true },
      { name: "status", label: "field.status", type: "select", options: STATUS_OPTS, map: "int", value: 1, required: true },
    ],
  },
  "provider-keys": {
    title: "crud.provider-keys.title", nav: "crud.provider-keys.nav", icon: "keys", path: "/provider-keys", singular: "crud.provider-keys.singular",
    desc: "crud.provider-keys.desc",
    maskedKeys: true,
    noEdit: true,
    columns: [
      { key: "id", label: "col.id", mono: true },
      { key: "provider_id", label: "col.provider", fk: "providers" },
      { key: "api_key", label: "col.apiKey", mono: true, render: (v) => el("span", { class: "mono", text: v || "—" }) },
      { key: "created_at", label: "col.created", mono: true, muted: true },
    ],
    fields: [
      { name: "id", label: "field.id", placeholder: "ph.autoId" },
      { name: "provider_id", label: "field.provider", type: "select", fk: "providers", required: true },
      { name: "api_key", label: "field.apiKey", type: "password", required: true, placeholder: "sk-…", full: true,
        tip: "tip.masked" },
    ],
  },
  tenants: {
    title: "crud.tenants.title", nav: "crud.tenants.nav", icon: "tenants", path: "/tenants", singular: "crud.tenants.singular",
    desc: "crud.tenants.desc",
    clearsFK: ["tenants"],
    columns: [
      { key: "id", label: "col.id", mono: true },
      { key: "name", label: "col.name" },
      { key: "domain", label: "col.domain", mono: true },
      { key: "auth_url", label: "col.authUrl", mono: true, truncate: true },
      { key: "has_access_token", label: "col.token", render: (v) => (v ? t("common.status.set") : "—") },
      { key: "enabled", label: "col.enabled", render: (v) => boolPill(v) },
    ],
    fields: [
      { name: "id", label: "field.id", placeholder: "ph.autoId" },
      { name: "name", label: "field.name", required: true, placeholder: "Acme" },
      { name: "domain", label: "field.domain", required: true, placeholder: "acme.com", tip: "tip.domain" },
      { name: "auth_url", label: "field.authUrl", type: "url", required: true, placeholder: "https://auth.acme.com/v1/verify", full: true,
        tip: "tip.authUrl",
        action: { label: "common.action.test", action: (input) => testAuthUrl(input) } },
      { name: "access_token", label: "field.accessToken", type: "password", map: "opt", full: true,
        placeholder: "ph.blankKeepToken",
        tip: "tip.blankKeep",
        action: { label: "common.action.generate", action: (input) => generateToken(input) } },
      { name: "cert_pem", label: "field.certPem", type: "textarea", full: true, rows: 4, map: "opt",
        placeholder: "-----BEGIN CERTIFICATE----- …",
        tip: "tip.certPem" },
      { name: "cert_key_pem", label: "field.certKeyPem", type: "textarea", full: true, rows: 4, map: "opt",
        placeholder: "-----BEGIN PRIVATE KEY----- …",
        tip: "tip.certKey" },
      { name: "enabled", label: "field.enabled", type: "checkbox", map: "bool", value: true },
    ],
  },
  "tenant-providers": {
    title: "crud.tenant-providers.title", nav: "crud.tenant-providers.nav", icon: "access", path: "/tenant-providers", singular: "crud.tenant-providers.singular",
    desc: "crud.tenant-providers.desc",
    noEdit: true,
    columns: [
      { key: "id", label: "col.id", mono: true },
      { key: "tenant_id", label: "col.tenant", fk: "tenants" },
      { key: "provider_id", label: "col.provider", fk: "providers" },
    ],
    fields: [
      { name: "id", label: "field.id", placeholder: "ph.autoId" },
      { name: "tenant_id", label: "field.tenant", type: "select", fk: "tenants", required: true },
      { name: "provider_id", label: "field.provider", type: "select", fk: "providers", required: true },
    ],
  },
  "tenant-models": {
    title: "crud.tenant-models.title", nav: "crud.tenant-models.nav", icon: "gate", path: "/tenant-models", singular: "crud.tenant-models.singular",
    desc: "crud.tenant-models.desc",
    noEdit: true,
    columns: [
      { key: "id", label: "col.id", mono: true },
      { key: "tenant_id", label: "col.tenant", fk: "tenants" },
      { key: "model_key", label: "col.modelKey", mono: true },
    ],
    fields: [
      { name: "id", label: "field.id", placeholder: "ph.autoId" },
      { name: "tenant_id", label: "field.tenant", type: "select", fk: "tenants", required: true },
      { name: "model_key", label: "field.modelKey", required: true, placeholder: "gpt-4" },
    ],
  },
  "limit-roles": {
    title: "crud.limit-roles.title", nav: "crud.limit-roles.nav", icon: "limits", path: "/limit-roles", singular: "crud.limit-roles.singular",
    desc: "crud.limit-roles.desc",
    columns: [
      { key: "name", label: "col.name" },
      { key: "matching_tenant", label: "col.matchingTenant", mono: true, render: (v) => monoOrAll(v) },
      { key: "matching_key", label: "col.matchingKey", mono: true, render: (v) => monoOrAll(v) },
      { key: "matching_model", label: "col.matchingModel", mono: true, render: (v) => monoOrAll(v) },
      { key: "matching_provider", label: "col.matchingProvider", mono: true, render: (v) => monoOrAll(v) },
      { key: "limit_count", label: "col.limitCount", align: "right", num: true, render: (v) => numOrDash(v) },
      { key: "limit_token", label: "col.limitToken", align: "right", num: true, render: (v) => numOrDash(v) },
      { key: "window", label: "col.window", mono: true },
      { key: "enabled", label: "col.enabled", render: (v) => boolPill(v) },
    ],
    fields: [
      { name: "name", label: "field.name", required: true, placeholder: "acme-default" },
      { name: "matching_tenant", label: "field.matchTenant", placeholder: "ph.blankAll", map: "opt", tip: "tip.tenantId" },
      { name: "matching_key", label: "field.matchKey", placeholder: "ph.blankAll", map: "opt" },
      { name: "matching_model", label: "field.matchModel", placeholder: "ph.blankAll", map: "opt" },
      { name: "matching_provider", label: "field.matchProvider", placeholder: "ph.blankAll", map: "opt" },
      { name: "limit_count", label: "field.limitCount", type: "number", map: "optint", placeholder: "ph.requests" },
      { name: "limit_token", label: "field.limitToken", type: "number", map: "optint", placeholder: "ph.tokens" },
      { name: "window", label: "field.window", type: "select", options: WINDOW_OPTS, value: "m" },
      { name: "enabled", label: "field.enabled", type: "checkbox", map: "bool", value: true },
    ],
  },
  "provider-key-bindings": {
    title: "crud.provider-key-bindings.title", nav: "crud.provider-key-bindings.nav", icon: "key2", path: "/provider-key-bindings", singular: "crud.provider-key-bindings.singular",
    desc: "crud.provider-key-bindings.desc",
    columns: [
      { key: "id", label: "col.id", mono: true },
      { key: "key_prefix", label: "col.prefix", mono: true },
      { key: "provider_id", label: "col.provider", fk: "providers" },
      { key: "enabled", label: "col.enabled", render: (v) => boolPill(v) },
    ],
    fields: [
      { name: "id", label: "field.id", placeholder: "ph.autoId" },
      { name: "key_prefix", label: "field.keyPrefix", required: true, placeholder: "sk_aaa_",
        tip: "tip.keyPrefix" },
      { name: "provider_id", label: "field.provider", type: "select", fk: "providers", required: true },
      { name: "enabled", label: "field.enabled", type: "checkbox", map: "bool", value: true },
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
    title: "custom.authcache.title", nav: "custom.authcache.title", icon: "authcache",
    desc: "custom.authcache.pageDesc",
    render: renderAuthCache,
  },
  breaker: {
    title: "custom.breaker.pageTitle", nav: "custom.breaker.nav", icon: "breaker",
    desc: "custom.breaker.pageDesc",
    render: renderBreaker,
  },
  health: {
    title: "custom.health.pageTitle", nav: "custom.health.nav", icon: "health",
    desc: "custom.health.pageDesc",
    render: renderHealth,
  },
  "api-docs": {
    title: "custom.apidocs.title", nav: "custom.apidocs.nav", icon: "book",
    desc: "custom.apidocs.desc",
    render: renderApiDocs,
  },
  stats: {
    title: "custom.stats.title", nav: "custom.stats.nav", icon: "chart",
    desc: "custom.stats.desc",
    render: renderStats,
  },
};

/* ordered nav with section dividers */
const NAV = [
  { labelKey: "common.nav.configuration", items: ["providers", "provider-models", "provider-keys", "tenants", "tenant-providers", "tenant-models", "limit-roles", "provider-key-bindings"] },
  { labelKey: "common.nav.operations", items: ["auth-cache", "breaker", "health", "stats"] },
  { labelKey: "common.nav.reference", items: ["api-docs"] },
];
function sectionConfig(key) { return CRUD[key] || CUSTOM[key]; }

/* ===========================================================================
 * Navigation
 * ======================================================================== */
function renderNav() {
  const nav = $("#nav");
  clear(nav);
  for (const group of NAV) {
    nav.appendChild(el("div", { class: "nav-section", text: t(group.labelKey) }));
    for (const key of group.items) {
      const cfg = sectionConfig(key);
      const btn = el("button", {
        class: `nav-item ${key === CURRENT ? "active" : ""}`,
        dataset: { key },
        onClick: () => go(key),
      },
        icon(cfg.icon, 17),
        el("span", { text: t(cfg.nav) }),
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
  $("#page-title").textContent = t(cfg.title);
  $("#page-sub").textContent = t(cfg.desc || "");
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
    toast(e.message, "err", { title: t("common.toast.failedLoad", { nav: t(cfg.nav) }) });
    return;
  }
  COUNTS[key] = rows.length;
  setNavBadge(key, rows.length);
  renderEntityPanel(cfg, rows);
}

function renderErrorPanel(e) {
  clear($("#content"));
  $("#content").appendChild(emptyState("alert", t("common.empty.couldNotLoad"), e.message || t("common.empty.unknownError")));
}

function renderEntityPanel(cfg, rows) {
  const content = $("#content");
  clear(content);

  const panel = el("div", { class: "panel" });
  const head = el("div", { class: "panel-head" },
    el("h2", {}, el("span", { text: t(cfg.title) }), " ",
      el("span", { class: "count", text: String(rows.length) })),
    el("div", { class: "spacer" }),
  );

  // keys reveal toggle
  if (cfg.maskedKeys) {
    head.appendChild(el("label", { class: "toggle-pill" },
      el("input", { type: "checkbox", id: "keys-reveal",
        onChange: (e) => { STATE.revealKeys = e.target.checked; loadEntity("provider-keys"); } }),
      el("span", { text: t("common.form.reveal") }),
    ));
  }
  head.appendChild(el("button", { class: "btn primary sm", onClick: () => openCreate(cfg) },
    icon("plus", 14), el("span", { class: "btn-label", text: t("common.action.new", { singular: t(cfg.singular) }) }),
  ));
  panel.appendChild(head);

  if (!rows.length) {
    panel.appendChild(emptyState("inbox", t("common.empty.noRows", { nav: t(cfg.nav).toLowerCase() }),
      t("common.empty.first", { singular: t(cfg.singular) }), { primary: t("common.empty.new", { singular: t(cfg.singular) }), onClick: () => openCreate(cfg) }));
  } else {
    panel.appendChild(renderTable(cfg, rows));
  }
  content.appendChild(panel);
}

const STATE = { revealKeys: false };

function renderTable(cfg, rows) {
  const cols = cfg.columns;
  const thead = el("thead", {}, el("tr", {},
    ...cols.map((c) => el("th", { class: c.align === "right" ? "num" : "", text: t(c.label) })),
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
          cfg.noEdit ? null : iconBtn("edit", t("common.action.edit"), () => openEdit(cfg, r)),
          iconBtn("trash", t("common.action.delete"), () => doDelete(cfg, r), "danger"),
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
    onClick: action.onClick }, icon("plus", 14), el("span", { class: "btn-label", text: action.primary })));
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
    el("span", { class: "btn-label", text: isEdit ? t("common.action.saveChanges") : t("common.action.create", { singular: t(cfg.singular) }) }));

  const body = form;
  const m = openModal({
    icon: isEdit ? "edit" : "plus", title: isEdit ? t("common.form.edit", { singular: t(cfg.singular) }) : t("common.form.new", { singular: t(cfg.singular) }),
    sub: t(cfg.desc), body, size: "lg",
    actions: [
      el("button", { class: "btn", text: t("common.action.cancel"), onClick: () => closeModal() }),
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
    if (firstInvalid) { firstInvalid.focus(); toast(t("common.form.completeHighlighted"), "err"); return; }

    setLoading(submitBtn, true, isEdit ? t("common.action.saving") : t("common.action.creating"));
    try {
      const bodyObj = collectBody(cfg, inputs, record);
      await writeAndReload(() =>
        isEdit ? api("PUT", `${cfg.path}/${record.id}`, { body: bodyObj })
               : api("POST", cfg.path, { body: bodyObj }));
      (cfg.clearsFK || []).forEach(invalidateFK);
      closeModal();
      toast(t(isEdit ? "common.toast.updated" : "common.toast.created", { singular: t(cfg.singular) }), "ok");
      await loadEntity(CURRENT);
    } catch (e) {
      setLoading(submitBtn, false);
      toast(e.message, "err", { title: t(isEdit ? "common.toast.failedUpdate" : "common.toast.failedCreate", { singular: t(cfg.singular) }) });
    }
  }
}

function buildField(f, value, { disabled, isEdit } = {}) {
  const group = el("div", { class: `input-group ${f.full ? "full" : ""}` });
  if (f.type === "checkbox") {
    group.classList.add("full");
    const c = el("label", { class: "check" },
      el("input", { type: "checkbox", dataset: { field: f.name }, disabled }),
      el("span", { text: `${t(f.label)}${f.required ? " *" : ""}` }),
    );
    const input = c.querySelector("input");
    input.checked = !!value;
    group.appendChild(c);
    return group;
  }
  group.appendChild(el("label", { text: t(f.label) },
    f.required ? el("span", { class: "req", text: " *" }) : null));
  let input;
  const placeholder = f.placeholder && f.placeholder.startsWith("ph.") ? t(f.placeholder) : (f.placeholder || "");
  if (f.type === "textarea") {
    input = el("textarea", { dataset: { field: f.name }, disabled, rows: f.rows || 3,
      placeholder });
    input.value = value === null || value === undefined ? "" : String(value);
  } else if (f.type === "select") {
    input = el("select", { dataset: { field: f.name }, disabled });
    if (f.fk) {
      const fk = FK[f.fk] || { list: [] };
      if (!f.required) input.appendChild(el("option", { value: "", text: t("common.form.none") }));
      if (!fk.list.length) input.appendChild(el("option", { value: "", text: t("common.form.noFk", { kind: t("crud." + f.fk + ".nav") }) }));
      for (const r of fk.list) input.appendChild(el("option", { value: String(r.id), text: `${r.name || r.id} · ${r.id}` }));
    } else {
      for (const o of (f.options || [])) input.appendChild(el("option", { value: o.value, text: t(o.label) }));
    }
    const targetVal = String(value ?? "");
    if (targetVal !== "") input.value = targetVal;        // edit / explicit default
    else if (!f.required) input.value = "";                // select the "— none —" option
    // required + empty (create) → leave the first option as the default selection
  } else {
    input = el("input", {
      type: f.type === "password" ? "password" : (f.type === "number" ? "number" : (f.type === "url" ? "url" : "text")),
      dataset: { field: f.name }, disabled,
      placeholder,
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
    wrap.appendChild(el("button", { class: "btn sm ghost", type: "button", text: t(f.action.label),
      onClick: () => f.action.action(input, group) }));
    group.appendChild(wrap);
  } else {
    group.appendChild(input);
  }
  const err = el("div", { class: "field-error hidden", text: "" });
  group.appendChild(err);
  if (f.tip) group.appendChild(el("div", { class: "field-tip", text: t(f.tip) }));
  return group;
}

function validateField(f, input) {
  const err = input.closest(".input-group").querySelector(".field-error");
  if (f.type === "checkbox") { hideErr(); return true; }
  const val = (input.value || "").trim();
  if (f.required && !val) {
    input.classList.add("invalid");
    if (err) { err.textContent = t("common.form.required", { label: t(f.label) }); err.classList.remove("hidden"); }
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
  if (!url) { toast(t("common.auth.testEnter"), "err"); return; }
  const wrap = input.closest(".input-wrap");
  const btn = wrap ? wrap.querySelector("button") : null;
  if (btn) setLoading(btn, true, t("common.auth.testing"));
  try {
    const r = await api("POST", "/tenants/auth/test", { body: { auth_url: url } });
    const ms = typeof r.duration_ms === "number" ? " · " + r.duration_ms + "ms" : "";
    const head = typeof r.status === "number" ? "HTTP " + r.status + ms : ms;
    if (r.ok) toast(r.detail + " (" + head + ")", "ok", { title: t("common.auth.testTitle") });
    else toast(r.detail + " (" + head + ")", "err", { title: t("common.auth.testFailTitle") });
  } catch (e) {
    toast(e.message, "err", { title: t("common.auth.testFailTitle") });
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
  toast(t("common.token.generated"), "info", { title: t("common.token.title") });
}


/* ===========================================================================
 * Delete
 * ======================================================================== */
async function doDelete(cfg, record) {
  const label = record.name || record.key || record.id;
  const ok = await confirmDialog({
    title: t("common.confirm.deleteTitle", { singular: t(cfg.singular) }),
    message: t("common.confirm.deleteMsg", { singular: t(cfg.singular) }),
    target: t("common.confirm.target", { nav: t(cfg.nav), label, id: record.id }),
    confirmText: t("common.action.delete"),
  });
  if (!ok) return;
  try {
    await writeAndReload(() => api("DELETE", `${cfg.path}/${record.id}`));
    (cfg.clearsFK || []).forEach(invalidateFK);
    toast(t("common.toast.deleted", { singular: t(cfg.singular) }), "ok");
    await loadEntity(CURRENT);
  } catch (e) {
    toast(e.message, "err", { title: t("common.toast.failedDelete", { singular: t(cfg.singular) }) });
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
      el("h2", {}, el("span", { text: t("custom.authcache.title") })), el("div", { class: "spacer" })),
    el("p", { class: "note" },
      t("custom.authcache.desc"), " ",
      el("code", { text: "tenant.auth_url" }), t("custom.authcache.desc2")),
    el("div", { class: "field-row" },
      el("div", { class: "input-group" },
        el("label", { text: t("custom.authcache.tenantLabel") }),
        el("input", { type: "text", id: "inv-tenant", placeholder: "e.g. t1" }),
      ),
      el("div", { class: "input-group" },
        el("label", { text: t("custom.authcache.keysLabel") }),
        el("input", { type: "text", id: "inv-keys", placeholder: "sk-aaa, sk-bbb" }),
      ),
    ),
    el("div", { style: "margin-top:14px" },
      el("button", { class: "btn primary", onClick: doInvalidate }, icon("trash", 15), " " + t("custom.authcache.invalidate")),
    ),
    el("div", { id: "inv-result", class: "hidden", style: "margin-top:14px" }),
  );
  content.appendChild(panel);
}

async function doInvalidate() {
  const tenant = $("#inv-tenant").value.trim();
  const keysRaw = $("#inv-keys").value.trim();
  const keys = keysRaw ? keysRaw.split(",").map((s) => s.trim()).filter(Boolean) : null;
  if (!tenant && !keys) { toast(t("common.token.provide"), "err"); return; }
  const body = {};
  if (tenant) body.tenant_id = tenant;
  if (keys) body.api_keys = keys;
  const box = $("#inv-result");
  const invalidatedMsg = (n) => t("common.token.invalidated", { n, y: n === 1 ? "y" : "ies" });
  try {
    const r = await api("DELETE", "/auth/cache", { body });
    box.className = "alert ok";
    box.textContent = invalidatedMsg(r.invalidated) + ".";
    box.classList.remove("hidden");
    toast(invalidatedMsg(r.invalidated), "ok");
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
      el("h2", {}, el("span", { text: t("custom.breaker.title") }), " ", el("span", { class: "count", id: "breaker-count", text: "" })),
      el("div", { class: "spacer" }),
      el("button", { class: "btn sm", onClick: loadBreaker }, icon("refresh", 14), el("span", { class: "btn-label", text: t("common.action.refresh") })),
    ),
    el("p", { class: "note" }, t("custom.breaker.desc")),
    el("div", { id: "breaker-table-wrap" }),
  );
  content.appendChild(panel);
  // manual reset form
  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "panel-head" }, el("h2", {}, el("span", { text: t("custom.breaker.forceReset") }))),
    el("div", { class: "field-row" },
      el("div", { class: "input-group" },
        el("label", { text: t("custom.breaker.providerId") }),
        el("input", { type: "text", id: "breaker-reset-id", placeholder: t("custom.breaker.providerId") }),
      ),
    ),
    el("div", { style: "margin-top:12px" },
      el("button", { class: "btn primary", onClick: resetBreakerById }, icon("refresh", 15), " " + t("custom.breaker.reset")),
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
    wrap.appendChild(emptyState("check", t("custom.breaker.emptyTitle"), t("custom.breaker.emptyMsg")));
    return;
  }
  wrap.appendChild(el("div", { class: "table-wrap" },
    el("table", {},
      el("thead", {}, el("tr", {}, el("th", { text: t("custom.breaker.providerId") }), el("th", { text: t("custom.breaker.state") }), el("th", { text: "" }))),
      el("tbody", {}, ...dead.map((pid) =>
        el("tr", {},
          el("td", {}, el("span", { class: "mono", text: pid })),
          el("td", {}, el("span", { class: "pill dead", text: t("custom.breaker.dead") })),
          el("td", { class: "actions" }, iconBtn("refresh", t("common.action.reset"), () => resetBreaker(pid))),
        ),
      )),
    ),
  ));
}
async function resetBreaker(id) {
  try {
    await api("DELETE", `/breaker/${encodeURIComponent(id)}`);
    toast(t("custom.breaker.resetToast", { id }), "ok");
    await loadBreaker();
  } catch (e) { toast(e.message, "err"); }
}
async function resetBreakerById() {
  const id = $("#breaker-reset-id").value.trim();
  if (!id) { toast(t("custom.breaker.required"), "err"); return; }
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
    icon("refresh", 14), el("span", { class: "btn-label", text: t("common.action.refresh") })));

  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "panel-head" }, el("h2", {}, el("span", { text: t("custom.health.status") })), el("div", { class: "spacer" })),
    el("div", { class: "stat-grid", id: "health-stats" },
      el("span", { class: "skeleton", style: "width:100%;height:60px" }),
    ),
  ));
  // Whole-cluster view (cluster P4): fleet nodes + lease holder.
  content.appendChild(el("div", { class: "panel" },
    el("div", { class: "panel-head" }, el("h2", {}, el("span", { text: t("custom.health.cluster") })), el("div", { class: "spacer" })),
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
      el("div", { class: "sl", text: t("custom.health.mode") }), el("div", { class: "sv", text: t("custom.health.singleNode") })));
    nodes.appendChild(el("p", { class: "muted",
      text: t("custom.health.clusterNotEnabled") }));
    return;
  }
  const alive = c.nodes.filter((n) => n.alive).length;
  const cards = [
    { l: t("custom.health.mode"), v: c.mode, cls: "" },
    { l: t("custom.health.leaseHolder"), v: c.lease_holder ?? "—", cls: c.lease_holder ? "ok" : "warn" },
    { l: t("custom.health.nodesAlive"), v: `${alive}/${c.nodes.length}`, cls: alive === c.nodes.length && c.nodes.length > 0 ? "ok" : "warn" },
    { l: t("custom.health.self"), v: c.node_id || "—", cls: "" },
  ];
  for (const k of cards) stats.appendChild(el("div", { class: `stat ${k.cls}` },
    el("div", { class: "sl", text: k.l }), el("div", { class: "sv", text: String(k.v) })));

  const th = (label) => el("th", { text: label });
  const rows = [el("tr", {}, th(t("custom.health.node")), th(t("custom.health.role")), th(t("custom.health.controlUrl")), th(t("custom.health.state")))];
  for (const n of c.nodes) {
    const name = el("span", { text: n.node_id });
    if (n.is_self) name.appendChild(el("span", { class: "pill info", text: t("custom.health.selfPill") }));
    if (n.is_lease_holder) name.appendChild(el("span", { class: "pill ok", text: t("custom.health.activePill") }));
    rows.push(el("tr", {},
      el("td", {}, name),
      el("td", {}, el("span", { class: `pill ${n.role === "leader" ? "info" : "warn"}`, text: n.role })),
      el("td", { class: "mono" }, n.control_url || "—"),
      el("td", {}, n.alive
        ? el("span", { class: "pill ok", text: t("custom.health.alivePill") })
        : el("span", { class: "pill dead", text: t("custom.health.downPill") })),
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
    { l: t("custom.health.statusVal"), v: h.status ?? "—", cls: up ? "ok" : "err" },
    { l: t("custom.health.db"), v: h.db ?? "—", cls: h.db === "ok" ? "ok" : "err" },
    { l: t("custom.health.breakerDead"), v: fmtNum(h.breaker_dead), cls: "" },
    { l: t("custom.health.tenants"), v: fmtNum(h.tenants), cls: "" },
    { l: t("custom.health.providers"), v: fmtNum(h.providers), cls: "" },
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
  ts.classList.remove("ok"); ts.classList.add("bad");
  const tEl = ts.querySelector(".t");
  tEl.dataset.i18n = "common.auth.notAuthenticated";
  tEl.textContent = t("common.auth.notAuthenticated");
  const inp = $("#login-token"); inp.value = ""; setTimeout(() => inp.focus(), 50);
}
async function tryLogin(token) {
  TOKEN = token;
  const btn = $("#login-btn");
  setLoading(btn, true, t("common.auth.signingIn"));
  try {
    await api("GET", "/health");
    document.body.dataset.state = "ready";
    $("#login-overlay").classList.add("hidden");
    $("#app").setAttribute("aria-hidden", "false");
    const ts = $("#token-status");
    ts.classList.remove("bad"); ts.classList.add("ok");
    const tEl = ts.querySelector(".t");
    tEl.dataset.i18n = "common.auth.authenticated";
    tEl.textContent = t("common.auth.authenticated");
    renderNav();
    go(CURRENT);
    toast(t("common.toast.signedIn"), "ok");
  } catch (e) {
    TOKEN = null;
    const err = $("#login-error");
    err.textContent = t("common.auth.failed", { msg: e.message });
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
      toast(t("common.toast.reloaded", { providers: n(r?.providers), tenants: n(r?.tenants) }), "ok", { title: t("common.toast.configReloaded") });
      await loadEntity(CURRENT);
    } catch (e) { toast(e.message, "err"); }
  });
  $("#menu-toggle").addEventListener("click", () => {
    document.body.classList.toggle("nav-open");
  });
  $("#sidebar-scrim").addEventListener("click", closeSidebar);
  const langSel = $("#lang-select");
  if (langSel) {
    langSel.value = currentLang();
    langSel.addEventListener("change", (e) => setLang(e.target.value));
  }
}

document.addEventListener("DOMContentLoaded", () => {
  // Language switch hook (set by i18n.js's setLang): re-render the static
  // shell, the nav and the current page. A modal is closed first so a form
  // never shows a mix of languages.
  window.__onLangChanged = (code) => {
    closeModal();
    applyStaticI18n();
    const sel = $("#lang-select");
    if (sel) sel.value = code;
    renderNav();
    go(CURRENT);
  };
  wireEvents();
  applyStaticI18n();
  showLogin();
});
