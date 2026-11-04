/* Hydra admin UI — vanilla JS, no build step (design §14.2).
 *
 * Embedded via include_dir! at compile time. Talks same-origin to /api/v1/*
 * with `Authorization: Bearer <admin-token>`. The token is kept in
 * `sessionStorage` for the current browser TAB (see `TOKEN_KEY`): a reload or
 * an in-tab navigation stays signed in, closing the tab does not. Never
 * `localStorage` — the admin token is the fleet's root credential and the
 * admin surface is still plaintext HTTP.
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
  roles:      '<path d="M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2"/><circle cx="9" cy="7" r="4"/><path d="M18 8v6M15 11h6"/>',
  token:      '<rect x="3" y="5" width="18" height="14" rx="2"/><path d="M7 10h10M7 14h6"/><path d="M17 14h1"/>',
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
/* Admin token storage: sessionStorage, NOT localStorage.
 * The admin token is the fleet's root credential and the admin surface is
 * still plaintext HTTP, so a ticket must not outlive the browser tab.
 * Survives reload / same-tab navigation; dies with the tab. */
const TOKEN_KEY = "hydra-admin-token";
function loadStoredToken() {
  try { return sessionStorage.getItem(TOKEN_KEY); } catch { return null; }
}
function storeToken(value) {
  try {
    if (value) sessionStorage.setItem(TOKEN_KEY, value);
    else sessionStorage.removeItem(TOKEN_KEY);
  } catch { /* storage disabled (private mode): stay in-memory only */ }
}
/** Re-entrant suppression for the global 401 handler. A COUNTER, not a
 *  boolean: the leader banner poll fires background requests, and a single
 *  flag would let one of those 401s be swallowed by a concurrent sign-in. */
const suppress401 = {
  n: 0,
  enter() { this.n += 1; },
  leave() { this.n = Math.max(0, this.n - 1); },
  get active() { return this.n > 0; },
};
let TOKEN = loadStoredToken();
let CURRENT = "providers";
const FK = {};          // { providers: {list, map}, tenants: {...} } — lazy
const COUNTS = {};      // section-key -> row count (for nav badges)
const FK_PATHS = { providers: "/providers", tenants: "/tenants" };

/* --- new hierarchy pages -------------------------------------------------
 * ROLE_ROWS is the last global-role list; it doubles as a suggestion source
 * because the admin API does not expose the user hierarchy yet (see the
 * hierarchy note on the page). HIERARCHY is filled opportunistically from the
 * endpoints that DO exist on a newer node; a 404 simply leaves it empty. */
let ROLE_ROWS = [];
let ACCESS_TENANT = null;          // selected tenant on the Access Tokens page
let PENDING_TOKEN = null;          // plaintext shown once, for the current visit
const HIERARCHY = { promise: null, groups: [], users: [], keys: [], available: false };
const MODEL_HINTS = { promise: null, keys: [] };


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
/** The status part of an error message. The `code` is appended ONLY when it says
 *  something the status does not: printing both unconditionally is how an empty
 *  body became "429 429: ". */
function errorHead(status, code) {
  return String(code) === String(status) ? String(status) : `${status} ${code}`;
}

/** A non-JSON error body, made fit for a human. Ingresses in front of this API
 *  answer with HTML pages: the title says what happened, the markup says nothing
 *  and can be kilobytes of it. */
function cleanErrorBody(raw, max = 160) {
  let s = String(raw);
  if (/^\s*</.test(s)) {
    const title = (s.match(/<title[^>]*>([\s\S]*?)<\/title>/i) || [])[1];
    s = title !== undefined ? title : s.replace(/<[^>]*>/g, " ");
  }
  s = s.replace(/\s+/g, " ").trim();
  return s.length > max ? s.slice(0, max) + "\u2026" : s;
}

/** `Retry-After` (RFC 9110: delta-seconds or an HTTP-date) as seconds, else null. */
function parseRetryAfter(v) {
  if (v === null || v === undefined || v === "") return null;
  const s = String(v).trim();
  if (/^\d+$/.test(s)) return Number(s);
  const at = Date.parse(s);
  if (Number.isNaN(at)) return null;
  return Math.max(0, Math.round((at - Date.now()) / 1000));
}

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
    const env = json && typeof json === "object" ? json.error : null;
    const code = (env && env.code) || resp.status;
    // A non-JSON (or empty) body used to be interpolated verbatim. Two measured
    // consequences: an ingress 429 with no body and no reason phrase (HTTP/2
    // carries none, so `statusText` is "") became the literal string `429 429: `,
    // and an HTML error page was dumped WHOLE into a toast — 6478 characters in the
    // case measured for `scripts/admin_ui_render.test.cjs`.
    let message = (env && env.message) || "";
    if (!message && typeof json === "string") message = cleanErrorBody(json);
    if (!message) message = String(resp.statusText || "").trim() || t("common.err.noDetails");
    const retryAfterSec = parseRetryAfter(resp.headers && resp.headers.get("Retry-After"));
    let text = `${errorHead(resp.status, code)}: ${message}`;
    // The server computes `Retry-After` for its rate-limited answers
    // (`admin/mod.rs:181-198` -> `handlers.rs:94` `err_json_throttled`); the UI threw
    // the header away and told the operator to retry without saying when.
    if (retryAfterSec !== null && retryAfterSec > 0) {
      text += ` (${t("common.err.retryAfter", { sec: retryAfterSec })})`;
    }
    const err = new Error(text);
    err.status = resp.status; err.code = code; err.body = json; err.retryAfterSec = retryAfterSec;
    // A rotated/expired ticket used to leave every page half-logged-in: the
    // requests kept failing with no way back except a manual reload.
    if (resp.status === 401 && !suppress401.active) onSessionInvalid();
    throw err;
  }
  return json;
}
/** write = POST/PUT/DELETE; after every successful write, reload the store.
 *
 *  Returns `null` when the running process ADOPTED the new config, or the Error
 *  when the write landed but the reload did not — see `toastReloadFailed`. */
async function writeAndReload(op) {
  await op();
  try {
    await api("POST", "/reload", { body: {} });
    return null;
  } catch (e) {
    return e;
  }
}

/** Report a write that landed while the runtime kept the previous snapshot.
 *
 *  A failed reload is NOT cosmetic: the server answers `400 reload_failed` and
 *  keeps the OLD snapshot, so the row IS committed while the running proxy still
 *  serves the previous config — and every later view in this UI reads the
 *  DATABASE rows, so with only the caller's optimistic toast the operator sees
 *  the new value everywhere and concludes it is live. (`snapshot_stale` on reads
 *  is the read-side counterpart of this condition.) This used to be an empty
 *  `catch { /* reload best-effort *\/ }` in `writeAndReload`.
 *
 *  Called by the CALLER, after its own success toast: `toast()` appends, so the
 *  newest message is the one the operator's eye lands on, and "Saved, but the
 *  runtime reload FAILED" must not be buried above "Provider updated". */
function toastReloadFailed(err) {
  toast(`${t("common.reloadFailed")} — ${err && err.message}`, "err");
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
      // Admission-control limits. These MUST be in the form: editing a provider
      // sends the WHOLE record (a PUT), and the server writes every column from the
      // body it receives — a field the form does not submit therefore arrives as
      // "absent", deserialises to `None`, and the column is set to NULL. Leaving
      // these out silently turned the operator's concurrency cap off (`proxy.rs`
      // uses `max_concurrency` as the admission gate) on any edit through the UI.
      // `optint` maps "" to null, which is exactly "unlimited".
      { name: "max_concurrency", label: "field.maxConcurrency", type: "number", map: "optint", tip: "tip.maxConcurrency" },
      { name: "max_queue_depth", label: "field.maxQueueDepth", type: "number", map: "optint", tip: "tip.maxQueueDepth" },
      { name: "queue_wait_timeout_ms", label: "field.queueWaitTimeoutMs", type: "number", map: "optint", tip: "tip.queueWaitTimeoutMs" },
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
        tip: "tip.authUrl" },
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
      // The legacy cert PATH columns. They belong in the form for the same reason
      // the provider concurrency columns do: PUT is a whole-record replace
      // (`db.rs` `UPDATE tenant SET ... cert_key = ?, cert_file = ?`), so omitting
      // them does not "leave them alone" — it sets them to NULL. `app.js`'s own
      // content fields cannot cover this: they are action-based at the handler
      // (`CertWrite::None` = leave the sealed content alone), while these two are
      // ordinary columns.
      //
      // Re-sending both paths is not a no-op either, by design: the handler READS
      // those files on this node and converts them into sealed content
      // (`handlers.rs` `resolve_tenant_cert_write`), which is how a pre-0007 row
      // gets migrated. Unreadable files are a loud 400 (`cert_file_unreadable`)
      // instead of a silent wipe. `opt` maps a blank input to null = "no path".
      { name: "cert_file", label: "field.certFilePath", map: "opt", full: true,
        placeholder: "/etc/hydra/certs/acme.crt", tip: "tip.certFilePath" },
      { name: "cert_key", label: "field.certKeyPath", map: "opt", full: true,
        placeholder: "/etc/hydra/certs/acme.key", tip: "tip.certKeyPath" },
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

/* ---- custom sections (breaker / health) ---- */
const CUSTOM = {
  "global-roles": {
    title: "custom.globalroles.title", nav: "custom.globalroles.nav", icon: "roles",
    desc: "custom.globalroles.pageDesc",
    render: renderGlobalRoles,
  },
  "access-tokens": {
    title: "custom.accesstokens.title", nav: "custom.accesstokens.nav", icon: "token",
    desc: "custom.accesstokens.pageDesc",
    render: renderAccessTokens,
    teardown: teardownAccessTokens,
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
    // A custom page may own timers; `go()` calls this when leaving the page.
    teardown: teardownStats,
  },
};

/* ordered nav with section dividers */
const NAV = [
  { labelKey: "common.nav.configuration", items: ["providers", "provider-models", "provider-keys", "tenants", "global-roles", "access-tokens", "tenant-providers", "tenant-models", "limit-roles"] },
  { labelKey: "common.nav.operations", items: ["breaker", "health", "stats"] },
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
  // Tear down the page we are LEAVING before switching: the stats page owns a
  // persistent auto-refresh interval, and without this it kept firing and
  // re-rendering itself over the page the user had navigated to (review
  // D3/M-9). Pages that own no timer simply omit `teardown`.
  const leaving = CUSTOM[CURRENT];
  if (leaving && typeof leaving.teardown === "function") {
    leaving.teardown({ sameSection: key === CURRENT });
  }
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

  // No "reveal plaintext" toggle here. The admin API ALWAYS masks provider keys
  // (`hydra_core::rewrite::mask_key`: `first10 + *** + last4` for keys >= 20 chars,
  // `first2 + *** + last2` for >= 6, harder still below that) and `?reveal=1` is
  // an accepted no-op (P1-5), so such a toggle could only ever lie - the one that
  // used to sit in this spot assigned a flag nothing read, and re-rendering
  // unchecked it again. The honest statement lives in `crud.provider-keys.desc` /
  // `tip.masked`, i.e. next to where a key is entered.
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
    el("span", { class: "btn-label", text: isEdit ? t("common.action.saveChanges") : t("common.form.new", { singular: t(cfg.singular) }) }));

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
      const reloadErr = await writeAndReload(() =>
        isEdit ? api("PUT", `${cfg.path}/${record.id}`, { body: bodyObj })
               : api("POST", cfg.path, { body: bodyObj }));
      (cfg.clearsFK || []).forEach(invalidateFK);
      closeModal();
      toast(t(isEdit ? "common.toast.updated" : "common.toast.created", { singular: t(cfg.singular) }), "ok");
      if (reloadErr) toastReloadFailed(reloadErr);
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
    // Whitespace-only counts as "not given", not as an empty VALUE. The server
    // reads a present-but-blank string as an explicit CLEAR for three of these
    // fields (`handlers.rs` `resolve_tenant_cert_write`: `(Some(""), _) =>
    // CertWrite::Clear`; `resolved_secret_writes`: `token.trim().is_empty() =>
    // Some(None)`), so a stray space in the PEM textarea or the access-token box
    // used to delete a tenant's certificate / self-service token — while the copy
    // next to those fields promises "leave blank to keep". `v` itself is passed
    // through untouched: PEM content must round-trip byte-for-byte (the server
    // trims only to test emptiness).
    case "opt": return v.trim() === "" ? null : v;
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
    const reloadErr = await writeAndReload(() => api("DELETE", `${cfg.path}/${record.id}`));
    (cfg.clearsFK || []).forEach(invalidateFK);
    toast(t("common.toast.deleted", { singular: t(cfg.singular) }), "ok");
    if (reloadErr) toastReloadFailed(reloadErr);
    await loadEntity(CURRENT);
  } catch (e) {
    toast(e.message, "err", { title: t("common.toast.failedDelete", { singular: t(cfg.singular) }) });
  }
}

/* ===========================================================================
 * Custom: Global Roles (five dimensions -> provider set)
 * ======================================================================== */

/** Best-effort hierarchy lookup for the dimension suggestions.
 *
 * The Global Role write API validates group/user/api-key ids against the live
 * tables, but the CURRENT admin surface has no read endpoint for them. A newer
 * node may expose /user-groups, /users and /user-api-keys; when it does the
 * form becomes a real cascade. When it does not, the inputs still accept ids and
 * the suggestions come from rules that already use them. A 404 is expected, not
 * an error the operator has to dismiss. */
async function ensureHierarchy() {
  if (!HIERARCHY.promise) {
    HIERARCHY.promise = (async () => {
      const [g, u, k] = await Promise.all([
        api("GET", "/user-groups").catch(() => null),
        api("GET", "/users").catch(() => null),
        api("GET", "/user-api-keys").catch(() => null),
      ]);
      if (Array.isArray(g)) HIERARCHY.groups = g;
      if (Array.isArray(u)) HIERARCHY.users = u;
      if (Array.isArray(k)) HIERARCHY.keys = k;
      HIERARCHY.available = !!(g || u || k);
      return HIERARCHY;
    })();
  }
  return HIERARCHY.promise;
}

/** Model keys for the datalist: /models when it exists, else provider-models. */
async function ensureModelHints() {
  if (!MODEL_HINTS.promise) {
    MODEL_HINTS.promise = (async () => {
      const keys = new Set();
      const collect = (payload) => {
        const list = Array.isArray(payload)
          ? payload
          : (payload && Array.isArray(payload.models) ? payload.models : []);
        for (const m of list) {
          const key = typeof m === "string" ? m : (m.key || m.model_key || m.id || m.name);
          if (key) keys.add(String(key));
        }
      };
      try { collect(await api("GET", "/models")); } catch { /* route may not exist */ }
      if (!keys.size) {
        try { collect(await api("GET", "/provider-models")); } catch { /* optional */ }
      }
      MODEL_HINTS.keys = [...keys].sort((a, b) => a.localeCompare(b));
      return MODEL_HINTS.keys;
    })();
  }
  return MODEL_HINTS.promise;
}

function dedupeRowsById(rows) {
  const seen = new Set();
  const out = [];
  for (const row of rows) {
    if (!row || row.id === undefined || row.id === null) continue;
    const id = String(row.id);
    if (seen.has(id)) continue;
    seen.add(id);
    out.push(row);
  }
  return out;
}

/** Group suggestions, narrowed by tenant when one is selected. */
function roleGroupRows(tenant) {
  const rows = HIERARCHY.groups.slice();
  for (const r of ROLE_ROWS) {
    if (r.user_group_id && r.user_group_id !== "*") {
      rows.push({
        id: r.user_group_id,
        tenant_id: r.tenant_id && r.tenant_id !== "*" ? r.tenant_id : "",
        name: "",
      });
    }
  }
  return dedupeRowsById(rows).filter((g) => !tenant || !g.tenant_id || g.tenant_id === tenant);
}

function roleUserRows(tenant, group) {
  const rows = HIERARCHY.users.slice();
  for (const r of ROLE_ROWS) {
    if (r.user_id && r.user_id !== "*") {
      rows.push({
        id: r.user_id,
        tenant_id: r.tenant_id && r.tenant_id !== "*" ? r.tenant_id : "",
        user_group_id: r.user_group_id && r.user_group_id !== "*" ? r.user_group_id : "",
        name: "",
      });
    }
  }
  return dedupeRowsById(rows).filter((u) => {
    if (tenant && u.tenant_id && u.tenant_id !== tenant) return false;
    if (group && u.user_group_id && u.user_group_id !== group) return false;
    return true;
  });
}

function roleKeyRows(tenant, group, user) {
  const users = new Map(HIERARCHY.users.map((u) => [String(u.id), u]));
  const rows = HIERARCHY.keys.slice();
  for (const r of ROLE_ROWS) {
    if (r.api_key_id && r.api_key_id !== "*") {
      rows.push({
        id: r.api_key_id,
        user_id: r.user_id && r.user_id !== "*" ? r.user_id : "",
        tenant_id: r.tenant_id && r.tenant_id !== "*" ? r.tenant_id : "",
        user_group_id: r.user_group_id && r.user_group_id !== "*" ? r.user_group_id : "",
        name: "",
      });
    }
  }
  return dedupeRowsById(rows).filter((k) => {
    if (user && k.user_id && k.user_id !== user) return false;
    const owner = users.get(String(k.user_id || ""));
    const tenantId = k.tenant_id || (owner && owner.tenant_id) || "";
    const groupId = k.user_group_id || (owner && owner.user_group_id) || "";
    if (tenant && tenantId && tenantId !== tenant) return false;
    if (group && groupId && groupId !== group) return false;
    return true;
  });
}

function fillRoleDatalist(list, rows) {
  clear(list);
  for (const row of rows) {
    const id = String(row.id);
    const name = row.name && String(row.name) !== id ? String(row.name) : "";
    list.appendChild(el("option", { value: id, label: name ? `${name} · ${id}` : id }));
  }
}

function roleDimCell(value) {
  return value && value !== "*"
    ? el("span", { class: "mono", text: String(value) })
    : el("span", { class: "muted", text: t("custom.globalroles.any") });
}

function roleProvidersCell(ids) {
  const list = Array.isArray(ids) ? ids : [];
  if (!list.length) return el("span", { class: "muted", text: "—" });
  const map = (FK.providers && FK.providers.map) || new Map();
  const shown = list.slice(0, 3);
  const wrap = el("div", { class: "chips" });
  for (const id of shown) {
    const p = map.get(String(id));
    const label = (p && (p.name || p.key)) || id;
    wrap.appendChild(el("span", { class: "chip", title: String(id) }, el("span", { class: "chip-dot" }), String(label)));
  }
  if (list.length > shown.length) {
    wrap.appendChild(el("span", { class: "chip more", text: t("custom.globalroles.more", { n: list.length - shown.length }) }));
  }
  return wrap;
}

function roleEnabledSwitch(role) {
  const on = !!role.enabled;
  return el("button", {
    class: `switch ${on ? "on" : ""}`,
    type: "button",
    role: "switch",
    "aria-checked": on ? "true" : "false",
    "aria-label": t(on ? "custom.globalroles.toggleDisable" : "custom.globalroles.toggleEnable"),
    onClick: () => toggleGlobalRole(role),
  },
    el("span", { class: "switch-knob" }),
    el("span", { class: "switch-label", text: t(on ? "common.status.true" : "common.status.false") }),
  );
}

async function renderGlobalRoles() {
  clear($("#page-actions"));
  $("#page-actions").appendChild(el("button", { class: "btn sm", onClick: renderGlobalRoles },
    icon("refresh", 14), el("span", { class: "btn-label", text: t("common.action.refresh") })));

  showSkeleton($("#content"));
  try {
    const [rows] = await Promise.all([
      api("GET", "/global-roles"),
      ensureFK("providers"),
    ]);
    ROLE_ROWS = Array.isArray(rows) ? rows : [];
    renderGlobalRolesPanel();
  } catch (e) {
    renderErrorPanel(e);
    toast(e.message, "err", { title: t("custom.globalroles.loadFailed") });
  }
}

function renderGlobalRolesPanel() {
  const content = $("#content");
  clear(content);
  const panel = el("div", { class: "panel" });
  panel.appendChild(el("div", { class: "panel-head" },
    el("h2", {}, el("span", { text: t("custom.globalroles.title") }), " ",
      el("span", { class: "count", text: String(ROLE_ROWS.length) })),
    el("div", { class: "spacer" }),
    el("button", { class: "btn primary sm", onClick: () => openGlobalRoleForm(null) },
      icon("plus", 14),
      el("span", { class: "btn-label", text: t("common.action.new", { singular: t("custom.globalroles.singular") }) })),
  ));
  panel.appendChild(el("p", { class: "note" }, t("custom.globalroles.desc")));

  if (!ROLE_ROWS.length) {
    panel.appendChild(emptyState("inbox", t("custom.globalroles.emptyTitle"), t("custom.globalroles.emptyMsg"),
      {
        primary: t("common.empty.new", { singular: t("custom.globalroles.singular") }),
        onClick: () => openGlobalRoleForm(null),
      }));
  } else {
    panel.appendChild(renderGlobalRoleTable());
  }
  content.appendChild(panel);
}

function renderGlobalRoleTable() {
  const cols = [
    "custom.globalroles.colName",
    "custom.globalroles.colTenant",
    "custom.globalroles.colGroup",
    "custom.globalroles.colUser",
    "custom.globalroles.colKey",
    "custom.globalroles.colModel",
    "custom.globalroles.colProviders",
    "custom.globalroles.colEnabled",
  ];
  const thead = el("thead", {}, el("tr", {},
    ...cols.map((k) => el("th", { text: t(k) })),
    el("th", { text: "" }),
  ));
  const tbody = el("tbody", {});
  for (const role of ROLE_ROWS) {
    tbody.appendChild(el("tr", {},
      el("td", {}, el("span", { class: "role-name", text: role.name || role.id })),
      el("td", {}, roleDimCell(role.tenant_id)),
      el("td", {}, roleDimCell(role.user_group_id)),
      el("td", {}, roleDimCell(role.user_id)),
      el("td", {}, roleDimCell(role.api_key_id)),
      el("td", {}, roleDimCell(role.model_key)),
      el("td", {}, roleProvidersCell(role.provider_ids)),
      el("td", {}, roleEnabledSwitch(role)),
      el("td", { class: "actions" },
        el("div", { class: "row-actions" },
          iconBtn("edit", t("common.action.edit"), () => openGlobalRoleForm(role)),
          iconBtn("trash", t("common.action.delete"), () => deleteGlobalRole(role), "danger"),
        ),
      ),
    ));
  }
  return el("div", { class: "table-wrap" }, el("table", {}, thead, tbody));
}

function roleFieldGroup(labelKey, control, { required = false, tipKey = null, full = false } = {}) {
  const group = el("div", { class: `input-group ${full ? "full" : ""}` },
    el("label", { text: t(labelKey) }, required ? el("span", { class: "req", text: " *" }) : null),
    control,
  );
  if (tipKey) group.appendChild(el("div", { class: "field-tip", text: t(tipKey) }));
  return group;
}

async function openGlobalRoleForm(record) {
  const isEdit = !!record;
  await Promise.all([ensureFK("tenants"), ensureFK("providers"), ensureHierarchy(), ensureModelHints()]);

  const form = el("form", { class: "form-grid", onSubmit: (e) => e.preventDefault() });
  const alertBox = el("div", { class: "alert err form-alert hidden", role: "alert" });
  form.appendChild(alertBox);

  const nameInput = el("input", {
    type: "text",
    placeholder: "acme-default",
    value: record ? record.name || "" : "",
  });
  const nameError = el("div", { class: "field-error hidden" });
  const nameGroup = el("div", { class: "input-group full" },
    el("label", { text: t("custom.globalroles.colName") }, el("span", { class: "req", text: " *" })),
    nameInput,
    nameError,
  );
  form.appendChild(nameGroup);

  const enabledInput = el("input", { type: "checkbox" });
  enabledInput.checked = record ? !!record.enabled : true;
  form.appendChild(el("div", { class: "input-group full" },
    el("label", { class: "check" }, enabledInput,
      el("span", { text: t("custom.globalroles.colEnabled") })),
  ));

  const tenantSel = el("select", {});
  const tenantIds = new Set();
  tenantSel.appendChild(el("option", { value: "", text: t("custom.globalroles.anyTenant") }));
  for (const tenant of ((FK.tenants && FK.tenants.list) || [])) {
    const id = String(tenant.id);
    tenantIds.add(id);
    tenantSel.appendChild(el("option", { value: id, text: `${tenant.name || tenant.id} · ${id}` }));
  }
  if (record && record.tenant_id && record.tenant_id !== "*") {
    const id = String(record.tenant_id);
    if (!tenantIds.has(id)) {
      tenantSel.appendChild(el("option", { value: id, text: id }));
    }
    tenantSel.value = id;
  }

  const groupInput = el("input", {
    type: "text",
    list: "global-role-groups",
    placeholder: t("custom.globalroles.anyGroup"),
    value: record && record.user_group_id !== "*" ? String(record.user_group_id) : "",
  });
  const userInput = el("input", {
    type: "text",
    list: "global-role-users",
    placeholder: t("custom.globalroles.anyUser"),
    value: record && record.user_id !== "*" ? String(record.user_id) : "",
  });
  const keyInput = el("input", {
    type: "text",
    list: "global-role-keys",
    placeholder: t("custom.globalroles.anyKey"),
    value: record && record.api_key_id !== "*" ? String(record.api_key_id) : "",
  });
  const modelInput = el("input", {
    type: "text",
    list: "global-role-models",
    placeholder: t("custom.globalroles.anyModel"),
    value: record && record.model_key !== "*" ? String(record.model_key) : "",
  });

  const groupsList = el("datalist", { id: "global-role-groups" });
  const usersList = el("datalist", { id: "global-role-users" });
  const keysList = el("datalist", { id: "global-role-keys" });
  const modelsList = el("datalist", { id: "global-role-models" });

  const dims = el("div", { class: "full" },
    el("div", { class: "section-label", text: t("custom.globalroles.fieldDims") }),
    el("div", { class: "role-dims" },
      roleFieldGroup("custom.globalroles.colTenant", tenantSel, { tipKey: "custom.globalroles.tipAny" }),
      roleFieldGroup("custom.globalroles.colGroup", groupInput, { tipKey: "custom.globalroles.tipAny" }),
      roleFieldGroup("custom.globalroles.colUser", userInput, { tipKey: "custom.globalroles.tipAny" }),
      roleFieldGroup("custom.globalroles.colKey", keyInput, { tipKey: "custom.globalroles.tipAny" }),
      roleFieldGroup("custom.globalroles.colModel", modelInput, { tipKey: "custom.globalroles.tipModel" }),
    ),
    groupsList, usersList, keysList, modelsList,
  );
  if (!HIERARCHY.available) {
    dims.appendChild(el("div", { class: "field-tip hierarchy-tip", text: t("custom.globalroles.hierarchyUnavailable") }));
  }
  form.appendChild(dims);

  const selectedProviders = new Set(
    (record && (record.provider_ids || record.providers) || []).map(String),
  );
  const providerField = buildProviderPicker(selectedProviders);
  form.appendChild(providerField.root);

  function refreshSuggestions() {
    const tenant = tenantSel.value.trim();
    const group = groupInput.value.trim();
    const user = userInput.value.trim();
    fillRoleDatalist(groupsList, roleGroupRows(tenant));
    fillRoleDatalist(usersList, roleUserRows(tenant, group));
    fillRoleDatalist(keysList, roleKeyRows(tenant, group, user));
    const modelKeys = new Set(MODEL_HINTS.keys);
    for (const r of ROLE_ROWS) if (r.model_key && r.model_key !== "*") modelKeys.add(String(r.model_key));
    fillRoleDatalist(modelsList, [...modelKeys].sort((a, b) => a.localeCompare(b)).map((id) => ({ id })));
  }

  tenantSel.addEventListener("change", refreshSuggestions);
  groupInput.addEventListener("input", refreshSuggestions);
  userInput.addEventListener("input", refreshSuggestions);
  keyInput.addEventListener("input", refreshSuggestions);
  groupInput.addEventListener("change", () => {
    const row = roleGroupRows("").find((g) => String(g.id) === groupInput.value.trim());
    if (row && row.tenant_id && !tenantSel.value) tenantSel.value = row.tenant_id;
    refreshSuggestions();
  });
  userInput.addEventListener("change", () => {
    const row = roleUserRows("", "").find((u) => String(u.id) === userInput.value.trim());
    if (row) {
      if (!tenantSel.value && row.tenant_id) tenantSel.value = row.tenant_id;
      if (!groupInput.value && row.user_group_id) groupInput.value = row.user_group_id;
    }
    refreshSuggestions();
  });
  keyInput.addEventListener("change", () => {
    const row = roleKeyRows("", "", "").find((k) => String(k.id) === keyInput.value.trim());
    if (row && row.user_id) {
      const owner = roleUserRows("", "").find((u) => String(u.id) === row.user_id);
      if (!userInput.value) userInput.value = row.user_id;
      if (owner) {
        if (!groupInput.value && owner.user_group_id) groupInput.value = owner.user_group_id;
        if (!tenantSel.value && owner.tenant_id) tenantSel.value = owner.tenant_id;
      }
    }
    refreshSuggestions();
  });
  refreshSuggestions();

  const submitBtn = el("button", { class: "btn primary", type: "submit" },
    el("span", { class: "btn-label", text: isEdit ? t("common.action.saveChanges") : t("common.form.new", { singular: t("custom.globalroles.singular") }) }));

  const modal = openModal({
    icon: isEdit ? "edit" : "plus",
    title: isEdit ? t("custom.globalroles.editTitle") : t("custom.globalroles.createTitle"),
    sub: t("custom.globalroles.formNote"),
    body: form,
    size: "lg",
    actions: [
      el("button", { class: "btn", text: t("common.action.cancel"), onClick: () => closeModal() }),
      submitBtn,
    ],
  });

  setTimeout(() => nameInput.focus(), 60);
  submitBtn.addEventListener("click", (e) => { e.preventDefault(); submit(); });
  form.addEventListener("submit", (e) => { e.preventDefault(); submit(); });

  function showError(message) {
    alertBox.textContent = message;
    alertBox.classList.remove("hidden");
    if (typeof alertBox.scrollIntoView === "function") alertBox.scrollIntoView({ block: "nearest" });
  }

  async function submit() {
    const name = nameInput.value.trim();
    nameInput.classList.remove("invalid");
    nameError.classList.add("hidden");
    if (!name) {
      nameInput.classList.add("invalid");
      nameError.textContent = t("common.form.required", { label: t("custom.globalroles.colName") });
      nameError.classList.remove("hidden");
      nameInput.focus();
      return;
    }

    const body = {
      name,
      tenant_id: tenantSel.value.trim() || "*",
      user_group_id: groupInput.value.trim() || "*",
      user_id: userInput.value.trim() || "*",
      api_key_id: keyInput.value.trim() || "*",
      model_key: modelInput.value.trim() || "*",
      providers: [...selectedProviders],
      enabled: !!enabledInput.checked,
    };
    if (isEdit) body.id = record.id;

    setLoading(submitBtn, true, isEdit ? t("common.action.saving") : t("common.action.creating"));
    try {
      const saved = await api("PUT", "/global-roles", { body });
      closeModal();
      toast(t("custom.globalroles.saved"), "ok");
      if (saved && Array.isArray(saved.warnings) && saved.warnings.length) {
        toast(saved.warnings.join(" "), "info", { title: t("custom.globalroles.warnTitle") });
      }
      await renderGlobalRoles();
    } catch (e) {
      setLoading(submitBtn, false);
      if (e.status === 503) {
        showError(`${t("custom.globalroles.errorNotPublished")} ${e.message}`);
        toast(t("custom.globalroles.errorNotPublished"), "err");
        // The write committed locally before the publish failed: refresh the
        // list under the modal so a later Get reflects the stored row.
        renderGlobalRoles();
      } else if (e.status === 400 || e.code === "invalid_role") {
        showError(`${t("custom.globalroles.errorInvalid")} ${e.message}`);
      } else {
        showError(e.message);
      }
    }
  }

  return modal;
}

function buildProviderPicker(selected) {
  const root = el("fieldset", { class: "provider-picker full" },
    el("legend", { text: t("custom.globalroles.fieldProviders") }),
  );

  const search = el("input", {
    type: "search",
    class: "provider-search",
    placeholder: t("custom.globalroles.searchProviders"),
    "aria-label": t("custom.globalroles.searchProviders"),
  });
  const meta = el("div", { class: "picker-meta" });
  const list = el("div", { class: "provider-list" });
  const empty = el("p", { class: "field-tip hidden", text: t("custom.globalroles.noProviders") });
  const providers = ((FK.providers && FK.providers.list) || []).slice()
    .sort((a, b) => String(a.name || a.id).localeCompare(String(b.name || b.id)));

  function updateMeta() {
    meta.textContent = t("custom.globalroles.selected", { n: selected.size });
  }

  if (!providers.length) {
    list.appendChild(el("p", { class: "field-tip", text: t("custom.globalroles.noProviderData") }));
  }

  for (const provider of providers) {
    const id = String(provider.id);
    const checkbox = el("input", { type: "checkbox" });
    checkbox.checked = selected.has(id);
    checkbox.addEventListener("change", () => {
      if (checkbox.checked) selected.add(id);
      else selected.delete(id);
      updateMeta();
    });
    const option = el("label", { class: "provider-option" },
      checkbox,
      el("span", { class: "provider-name", text: provider.name || id }),
      el("span", { class: "provider-id mono", text: id }),
    );
    option.dataset.search = `${provider.name || ""} ${provider.key || ""} ${id}`.toLowerCase();
    list.appendChild(option);
  }

  search.addEventListener("input", () => {
    const needle = search.value.trim().toLowerCase();
    for (const option of list.querySelectorAll(".provider-option")) {
      option.classList.toggle("hidden", !!needle && !option.dataset.search.includes(needle));
    }
    const visible = [...list.querySelectorAll(".provider-option")].some((o) => !o.classList.contains("hidden"));
    empty.classList.toggle("hidden", visible || !providers.length);
  });

  root.appendChild(search);
  root.appendChild(meta);
  root.appendChild(list);
  root.appendChild(empty);
  root.appendChild(el("div", { class: "field-tip", text: t("custom.globalroles.tipProviders") }));
  return { root, selected };
}

function roleToggleBody(role, enabled) {
  return {
    id: role.id,
    name: role.name,
    tenant_id: role.tenant_id,
    user_group_id: role.user_group_id,
    user_id: role.user_id,
    api_key_id: role.api_key_id,
    model_key: role.model_key,
    providers: Array.isArray(role.provider_ids) ? role.provider_ids : [],
    enabled,
  };
}

async function toggleGlobalRole(role) {
  const next = !role.enabled;
  try {
    await api("PUT", "/global-roles", { body: roleToggleBody(role, next) });
    toast(t(next ? "custom.globalroles.enabledToast" : "custom.globalroles.disabledToast"), "ok");
    await renderGlobalRoles();
  } catch (e) {
    if (e.status === 503) {
      toast(`${t("custom.globalroles.errorNotPublished")} ${e.message}`, "err");
      await renderGlobalRoles();
    } else {
      toast(e.message, "err", { title: t("custom.globalroles.toggleFailed") });
    }
  }
}

async function deleteGlobalRole(role) {
  const ok = await confirmDialog({
    title: t("custom.globalroles.deleteTitle"),
    message: t("custom.globalroles.deleteMsg"),
    target: `${role.name || role.id} (${role.id})`,
  });
  if (!ok) return;
  try {
    await api("DELETE", `/global-roles/${encodeURIComponent(role.id)}`);
    toast(t("custom.globalroles.deleted"), "ok");
    await renderGlobalRoles();
  } catch (e) {
    toast(e.message, "err", { title: t("custom.globalroles.deleteFailed") });
  }
}

/* ===========================================================================
 * Custom: Tenant Access Tokens
 * ======================================================================== */

function teardownAccessTokens({ sameSection = false } = {}) {
  // The plaintext must not survive NAVIGATION, but a language switch re-renders
  // the SAME page and must not erase a token the operator has not copied yet.
  if (!sameSection) PENDING_TOKEN = null;
}

async function renderAccessTokens() {
  clear($("#page-actions"));
  $("#page-actions").appendChild(el("button", { class: "btn sm", onClick: renderAccessTokens },
    icon("refresh", 14), el("span", { class: "btn-label", text: t("common.action.refresh") })));

  showSkeleton($("#content"));
  let tenants = [];
  try {
    tenants = await api("GET", "/tenants") || [];
  } catch (e) {
    renderErrorPanel(e);
    toast(e.message, "err", { title: t("custom.accesstokens.loadFailed") });
    return;
  }

  if (!ACCESS_TENANT || !tenants.some((x) => String(x.id) === String(ACCESS_TENANT))) {
    ACCESS_TENANT = tenants.length ? String(tenants[0].id) : null;
  }

  const content = $("#content");
  clear(content);
  const panel = el("div", { class: "panel" });

  if (!tenants.length) {
    panel.appendChild(el("div", { class: "panel-head" },
      el("h2", {}, el("span", { text: t("custom.accesstokens.title") })), el("div", { class: "spacer" })));
    panel.appendChild(emptyState("inbox", t("custom.accesstokens.noTenantsTitle"), t("custom.accesstokens.noTenantsMsg")));
    panel.appendChild(el("div", { class: "empty-action" },
      el("button", { class: "btn sm", onClick: () => go("tenants") },
        icon("tenants", 14),
        el("span", { text: t("custom.accesstokens.goTenants") }))));
    content.appendChild(panel);
    return;
  }

  const tenantSelect = el("select", { id: "access-tenant", "aria-label": t("custom.accesstokens.selectTenant") });
  for (const tenant of tenants) {
    tenantSelect.appendChild(el("option", {
      value: String(tenant.id),
      text: `${tenant.name || tenant.id} · ${tenant.id}`,
    }));
  }
  tenantSelect.value = String(ACCESS_TENANT);
  tenantSelect.addEventListener("change", () => {
    ACCESS_TENANT = tenantSelect.value;
    renderAccessTokens();
  });

  panel.appendChild(el("div", { class: "panel-head access-head" },
    el("h2", {}, el("span", { text: t("custom.accesstokens.title") }), " ",
      el("span", { class: "count", id: "access-token-count", text: "" })),
    el("div", { class: "spacer" }),
    el("div", { class: "tenant-picker" },
      el("label", { for: "access-tenant", text: t("custom.accesstokens.selectTenant") }),
      tenantSelect,
    ),
    el("button", { class: "btn primary sm", onClick: () => openAccessTokenForm(String(ACCESS_TENANT)) },
      icon("plus", 14),
      el("span", { class: "btn-label", text: t("custom.accesstokens.new") })),
  ));
  panel.appendChild(el("p", { class: "note" }, t("custom.accesstokens.desc")));

  if (PENDING_TOKEN && String(PENDING_TOKEN.tenant_id) === String(ACCESS_TENANT)) {
    panel.appendChild(pendingTokenCallout());
  }
  const wrap = el("div", { id: "access-token-wrap" });
  panel.appendChild(wrap);
  content.appendChild(panel);
  await loadAccessTokenTable(String(ACCESS_TENANT), wrap);
}

function tokenSkeleton() {
  return el("div", { class: "table-wrap" },
    el("table", {},
      el("tbody", {}, ...Array.from({ length: 3 }).map(() =>
        el("tr", { class: "skeleton-row" },
          el("td", {}, el("span", { class: "skeleton" })),
          el("td", {}, el("span", { class: "skeleton", style: "width:40%" })),
          el("td", {}, el("span", { class: "skeleton", style: "width:30%" })),
          el("td", {}, el("span", { class: "skeleton", style: "width:20%" })),
        ),
      )),
    ),
  );
}

async function loadAccessTokenTable(tenantId, wrap) {
  clear(wrap);
  wrap.appendChild(tokenSkeleton());
  let rows = [];
  try {
    rows = await api("GET", `/tenants/${encodeURIComponent(tenantId)}/access-tokens`) || [];
  } catch (e) {
    clear(wrap);
    wrap.appendChild(emptyState("alert", t("custom.accesstokens.loadFailed"), e.message));
    toast(e.message, "err", { title: t("custom.accesstokens.loadFailed") });
    return;
  }
  clear(wrap);
  const count = $("#access-token-count");
  if (count) count.textContent = String(rows.length);
  if (!rows.length) {
    wrap.appendChild(emptyState("inbox", t("custom.accesstokens.emptyTitle"), t("custom.accesstokens.emptyMsg"),
      { primary: t("common.empty.new", { singular: t("custom.accesstokens.singular") }),
        onClick: () => openAccessTokenForm(tenantId) }));
    return;
  }
  wrap.appendChild(accessTokenTable(rows, tenantId));
}

function accessTokenTable(rows, tenantId) {
  const headers = [
    "custom.accesstokens.colId",
    "custom.accesstokens.colName",
    "custom.accesstokens.colPrefix",
    "custom.accesstokens.colLast4",
    "custom.accesstokens.colEnabled",
    "custom.accesstokens.colExpires",
  ];
  const thead = el("thead", {}, el("tr", {},
    ...headers.map((k) => el("th", { text: t(k) })),
    el("th", { text: "" }),
  ));
  const tbody = el("tbody", {});
  for (const row of rows) {
    tbody.appendChild(el("tr", {},
      el("td", {}, el("span", { class: "mono", text: row.id })),
      el("td", {}, el("span", { class: "role-name", text: row.name || "—" })),
      el("td", {}, el("span", { class: "mono", text: row.token_prefix || "—" })),
      el("td", {}, el("span", { class: "mono", text: row.token_last4 || "—" })),
      el("td", {}, boolPill(!!row.enabled)),
      el("td", {}, expiryCell(row.expires_at)),
      el("td", { class: "actions" },
        el("div", { class: "row-actions" },
          iconBtn("trash", t("custom.accesstokens.revoke"), () => revokeAccessToken(tenantId, row), "danger"),
        ),
      ),
    ));
  }
  return el("div", { class: "table-wrap" }, el("table", {}, thead, tbody));
}

function expiryCell(value) {
  if (!value) return el("span", { class: "muted", text: t("custom.accesstokens.never") });
  const at = Date.parse(value);
  const expired = !Number.isNaN(at) && at <= Date.now();
  return el("span", { class: expired ? "pill err" : "mono", text: shortTime(value), title: String(value) });
}

function pendingTokenCallout() {
  const copyBtn = el("button", { class: "btn primary sm", type: "button", text: t("common.action.copy") });
  copyBtn.addEventListener("click", () => copyText(PENDING_TOKEN.token, copyBtn));
  return el("div", { class: "token-callout", role: "status" },
    el("div", { class: "token-callout-head" },
      el("div", {},
        el("strong", { text: t("custom.accesstokens.createdTitle") }),
        el("div", { class: "field-tip", text: t("custom.accesstokens.createdWarning") }),
      ),
      copyBtn,
    ),
    el("code", { class: "token-value", text: PENDING_TOKEN.token }),
    el("div", { class: "token-meta", text: `${PENDING_TOKEN.name} · ${PENDING_TOKEN.token_prefix}…${PENDING_TOKEN.token_last4}` }),
    el("div", { class: "token-actions" },
      el("span", { class: "field-tip", text: t("custom.accesstokens.prefixHelp") }),
      el("button", { class: "btn sm", type: "button", text: t("custom.accesstokens.done"),
        onClick: () => { PENDING_TOKEN = null; renderAccessTokens(); } }),
    ),
  );
}

function openAccessTokenForm(tenantId) {
  const form = el("form", { class: "form-grid", onSubmit: (e) => e.preventDefault() });
  const alertBox = el("div", { class: "alert err form-alert hidden", role: "alert" });
  form.appendChild(alertBox);

  const nameInput = el("input", { type: "text", placeholder: "ci-deploy" });
  const nameError = el("div", { class: "field-error hidden" });
  form.appendChild(el("div", { class: "input-group full" },
    el("label", { text: t("custom.accesstokens.fieldName") }, el("span", { class: "req", text: " *" })),
    nameInput,
    nameError,
  ));

  const expiresInput = el("input", { type: "datetime-local" });
  const expiresError = el("div", { class: "field-error hidden" });
  form.appendChild(el("div", { class: "input-group full" },
    el("label", { text: t("custom.accesstokens.fieldExpires") }),
    expiresInput,
    expiresError,
    el("div", { class: "field-tip", text: t("custom.accesstokens.tipExpires") }),
  ));

  const submitBtn = el("button", { class: "btn primary", type: "submit" },
    el("span", { class: "btn-label", text: t("common.form.new", { singular: t("custom.accesstokens.singular") }) }));

  openModal({
    icon: "token",
    title: t("custom.accesstokens.createTitle"),
    sub: t("custom.accesstokens.desc"),
    body: form,
    actions: [
      el("button", { class: "btn", text: t("common.action.cancel"), onClick: () => closeModal() }),
      submitBtn,
    ],
  });

  setTimeout(() => nameInput.focus(), 60);
  submitBtn.addEventListener("click", (e) => { e.preventDefault(); submit(); });
  form.addEventListener("submit", (e) => { e.preventDefault(); submit(); });

  function showError(message) {
    alertBox.textContent = message;
    alertBox.classList.remove("hidden");
  }

  async function submit() {
    const name = nameInput.value.trim();
    nameInput.classList.remove("invalid");
    nameError.classList.add("hidden");
    expiresInput.classList.remove("invalid");
    expiresError.classList.add("hidden");
    if (!name) {
      nameInput.classList.add("invalid");
      nameError.textContent = t("custom.accesstokens.nameRequired");
      nameError.classList.remove("hidden");
      nameInput.focus();
      return;
    }
    let expiresAt = null;
    if (expiresInput.value) {
      const parsed = new Date(expiresInput.value);
      if (Number.isNaN(parsed.getTime())) {
        expiresInput.classList.add("invalid");
        expiresError.textContent = t("custom.accesstokens.badExpiry");
        expiresError.classList.remove("hidden");
        expiresInput.focus();
        return;
      }
      expiresAt = parsed.toISOString();
    }
    const body = { name };
    if (expiresAt) body.expires_at = expiresAt;

    setLoading(submitBtn, true, t("common.action.creating"));
    try {
      const created = await api("POST", `/tenants/${encodeURIComponent(tenantId)}/access-tokens`, { body });
      closeModal();
      PENDING_TOKEN = {
        tenant_id: tenantId,
        token: created.token,
        name: created.name || name,
        token_prefix: created.token_prefix || "",
        token_last4: created.token_last4 || "",
      };
      toast(t("custom.accesstokens.created"), "ok");
      await renderAccessTokens();
    } catch (e) {
      setLoading(submitBtn, false);
      if (e.status === 503) {
        // The handler stores the token, then returns 503 without the plaintext
        // body. The secret is unrecoverable; say so instead of pretending the
        // create failed as a whole.
        showError(t("custom.accesstokens.notPublished"));
        toast(t("custom.accesstokens.notPublished"), "err");
        renderAccessTokens();
      } else {
        showError(e.message);
      }
    }
  }
}

async function revokeAccessToken(tenantId, token) {
  const ok = await confirmDialog({
    title: t("custom.accesstokens.revokeTitle"),
    message: t("custom.accesstokens.revokeMsg"),
    target: `${token.name || token.id} (${token.id})`,
    confirmText: t("custom.accesstokens.revoke"),
  });
  if (!ok) return;
  try {
    await api("DELETE", `/tenants/${encodeURIComponent(tenantId)}/access-tokens/${encodeURIComponent(token.id)}`);
    toast(t("custom.accesstokens.revoked"), "ok");
    await renderAccessTokens();
  } catch (e) {
    toast(e.message, "err", { title: t("custom.accesstokens.revokeFailed") });
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
  // Each panel renders from ITS OWN result. `Promise.all` used to let a failed
  // cluster probe discard the `/health` payload that had ALREADY arrived: the
  // Status panel kept its skeleton forever while the process was healthy, and the
  // raw-JSON view showed only the cluster error. Note the asymmetry this fixes —
  // `refreshLeaderBanner` polls this same endpoint and already tolerates its
  // failure (`catch { return; }`), while the health page let it take the page down.
  //
  // `cluster_unavailable` (502) means "cannot read the cluster registry (Redis
  // unreachable?)" — NOT "single node": single-node mode answers
  // `200 {cluster:false}` (`cluster_api.rs:92-95` -> `single_node_status`). So a
  // 502 here is a real degradation and is rendered as one.
  const errText = (r) => (r && r.message) || t("common.empty.unknownError");
  const [healthRes, clusterRes] = await Promise.allSettled([
    api("GET", "/health"),
    api("GET", "/cluster/status"),
  ]);
  if (healthRes.status === "fulfilled") {
    renderHealthStats(healthRes.value);
  } else {
    renderHealthProbeError(healthRes.reason);
    toast(errText(healthRes.reason), "err");
  }
  if (clusterRes.status === "fulfilled") {
    renderClusterStatus(clusterRes.value);
  } else {
    renderClusterUnavailable(clusterRes.reason);
  }
  // Both outcomes are in the raw view, labelled — an absent key would read as
  // "healthy and empty".
  $("#health-json").innerHTML = highlightJson({
    health: healthRes.status === "fulfilled" ? healthRes.value : { error: errText(healthRes.reason) },
    cluster: clusterRes.status === "fulfilled" ? clusterRes.value : { error: errText(clusterRes.reason) },
  });
}

/** The `/health` probe itself failed: say so where the numbers would have been. */
function renderHealthProbeError(e) {
  const grid = $("#health-stats");
  clear(grid);
  grid.appendChild(el("div", { class: "stat warn" },
    el("div", { class: "sl", text: t("custom.health.status") }),
    el("div", { class: "sv", text: t("custom.health.unavailable") })));
  grid.appendChild(el("p", { class: "muted", text: (e && e.message) || t("common.empty.unknownError") }));
}

/** The `/cluster/status` probe failed. Deliberately NOT `clusterNotEnabled`:
 *  that message ASSERTS "no member list / single node", which is a different
 *  fact from "this node could not answer the status route". */
function renderClusterUnavailable(e) {
  const stats = $("#cluster-stats");
  const nodes = $("#cluster-nodes");
  clear(stats); clear(nodes);
  stats.appendChild(el("div", { class: "stat warn" },
    el("div", { class: "sl", text: t("custom.health.mode") }),
    el("div", { class: "sv", text: t("custom.health.unavailable") })));
  nodes.appendChild(el("p", { class: "muted",
    text: t("custom.health.clusterUnavailable", { msg: (e && e.message) || t("common.empty.unknownError") }) }));
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
  // `alive` is a THREE-state field since ADR-0001 T4.1: `true` for this node, `null` for a peer
  // whose liveness this node cannot observe (there is no registry and no per-node RPC to ask).
  // Counting null as "not alive" would render a healthy peer as DOWN, which is a claim nobody
  // measured — so the card counts what is known and says so when something is not.
  const alive = c.nodes.filter((n) => n.alive === true).length;
  const unknown = c.nodes.filter((n) => n.alive === null || n.alive === undefined).length;
  const cards = [
    { l: t("custom.health.mode"), v: c.mode, cls: "" },
    { l: t("custom.health.leaseHolder"), v: c.lease_holder ?? "—", cls: c.lease_holder ? "ok" : "warn" },
    {
      l: t("custom.health.nodesAlive"),
      v: unknown > 0 ? `${alive}/${c.nodes.length} (+${unknown} ${t("custom.health.unknownPill")})` : `${alive}/${c.nodes.length}`,
      cls: unknown > 0 ? "" : alive === c.nodes.length && c.nodes.length > 0 ? "ok" : "warn",
    },
    { l: t("custom.health.self"), v: c.node_id || "—", cls: "" },
  ];
  for (const k of cards) stats.appendChild(el("div", { class: `stat ${k.cls}` },
    el("div", { class: "sl", text: k.l }), el("div", { class: "sv", text: String(k.v) })));

  const th = (label) => el("th", { text: label });
  // Columns since ADR-0001 (the role is gone, so the old ROLE column is too):
  //   NODE          | the member, with `self` marked
  //   RAFT ADDRESS  | the address in `control_url` — which is the raft TRANSPORT address now, not
  //                 a control URL (the field name is kept because it is this table's key; the
  //                 header must not keep lying about it)
  //   LEADER        | `leader` for the member raft's hint names, `member` otherwise. This used to
  //                 be `n.role`, a field the DTO no longer HAS — so the cell rendered `undefined`
  //                 (an empty pill under a header that said "RULE", itself a mistranslation of
  //                 ROLE). One pill per fact: the leader is named HERE, not twice.
  //   STATE         | the three-state liveness (`alive` null = this node cannot observe the peer)
  const rows = [el("tr", {}, th(t("custom.health.node")), th(t("custom.health.raftAddr")),
    th(t("custom.health.leaderCol")), th(t("custom.health.state")))];
  for (const n of c.nodes) {
    const name = el("span", { text: n.node_id });
    if (n.is_self) name.appendChild(el("span", { class: "pill info", text: t("custom.health.selfPill") }));
    rows.push(el("tr", {},
      el("td", {}, name),
      el("td", { class: "mono" }, n.control_url || "—"),
      el("td", {}, n.is_lease_holder
        ? el("span", { class: "pill ok", text: t("custom.health.leaderPill") })
        : el("span", { class: "pill", text: t("custom.health.memberPill") })),
      el("td", {}, n.alive === true
        ? el("span", { class: "pill ok", text: t("custom.health.alivePill") })
        : n.alive === false
          ? el("span", { class: "pill dead", text: t("custom.health.downPill") })
          : el("span", { class: "pill", text: t("custom.health.unknownPill") })),
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
 * Leader banner (T9.5 / §7-5)
 * ======================================================================== */
/** Show a banner when this admin UI is NOT on the node raft says is leading.
 *
 *  Every node serves its own admin API since ADR-0001 (there is no edge), and writes are accepted
 *  on ANY node — the library forwards them to the leader — so unlike the old leader/edge topology
 *  this banner no longer warns about a write that is about to bounce. It says which node leads,
 *  which is what an operator needs when reading the fleet view from a follower.
 *
 *  The "jump to the leader's admin URL" link is GONE, and it is not a missing feature: the link
 *  was built from `HYDRA_PUBLIC_URL` per node, a variable this plan retired, and the field the DTO
 *  still calls `control_url` now carries the node's **raft transport** address — linking a browser
 *  at that would dial a raft port. Naming the leader is the honest thing that is left.
 *
 *  Re-entrant: it is called on login, every 30s, and after a language change, so
 *  it reuses the existing element instead of stacking banners. */
async function refreshLeaderBanner() {
  let cluster = null;
  try {
    cluster = await api("GET", "/cluster/status");
  } catch {
    return; // not cluster-enabled, or the probe failed: no banner either way
  }
  const existing = document.getElementById("leader-banner");
  if (
    !cluster ||
    !cluster.cluster ||
    // `lease_holder` is `null` while this node does not yet know who leads (cold start, lost
    // quorum). That is "not known", not "I am the leader" — so it shows NO banner rather than a
    // claim in either direction.
    !cluster.lease_holder ||
    cluster.node_id === cluster.lease_holder
  ) {
    if (existing) existing.remove();
    return;
  }
  const banner = existing || el("div", { id: "leader-banner" });
  clear(banner);
  banner.appendChild(
    el("span", {
      text: t("common.leaderBanner.notLeader", { node: cluster.lease_holder }),
    }),
  );
  // Insert into `.main` (the content column), NOT `#app`: `#app` is a flex ROW
  // of sidebar + main, so prepending there renders a squeezed third column.
  if (!existing) $(".main").prepend(banner);
}

/* ===========================================================================
 * Auth / login
 * ======================================================================== */
/** The login VIEW only. Deliberately does NOT touch TOKEN or storage.
 *
 *  Doing so (the first draft did) makes a session restore impossible: `api()`
 *  refuses to even fetch without a token, so a restore path that cleared the
 *  token here would never issue the validation request and would always look
 *  like a failure. */
function showLoginView({ expired = false } = {}) {
  document.body.dataset.state = "locked";
  $("#login-overlay").classList.remove("hidden");
  $("#app").setAttribute("aria-hidden", "true");
  const ts = $("#token-status");
  ts.classList.remove("ok"); ts.classList.add("bad");
  const tEl = ts.querySelector(".t");
  tEl.dataset.i18n = "common.auth.notAuthenticated";
  tEl.textContent = t("common.auth.notAuthenticated");
  const err = $("#login-error");
  if (expired) {
    err.textContent = t("common.auth.sessionExpired");
    err.classList.remove("hidden");
  } else {
    err.textContent = "";
    err.classList.add("hidden");
  }
  const inp = $("#login-token"); inp.value = ""; setTimeout(() => inp.focus(), 50);
}

/** Drop the ticket AND return to the login view. Logout / 401 only. */
function showLogin({ expired = false } = {}) {
  TOKEN = null;
  storeToken(null);
  showLoginView({ expired });
}

/** One place that switches from the login view to the app. */
function enterApp() {
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
  // T9.5: the banner is installed now; the guard stays so a scrollback to a
  // build without it cannot throw `ReferenceError` on login — the failure class
  // this repo has already shipped once ("the write landed and the UI still
  // reported a failure").
  if (typeof refreshLeaderBanner === "function") refreshLeaderBanner();
}

/** The single 401 handler for the whole app: drop the ticket, go back to the
 *  login view and say so ONCE — instead of leaving every page half-logged-in. */
function onSessionInvalid() {
  if (document.body.dataset.state === "locked") return; // already there
  showLogin({ expired: true });
  toast(t("common.auth.sessionExpired"), "err");
}

/** Validate a ticket restored from sessionStorage.
 *
 *  ORDER IS LOAD-BEARING: the login view is drawn first, then the CANDIDATE is
 *  put back in `TOKEN` BEFORE the validation call — `api()` throws without a
 *  token, so validating first would never issue a request. The ticket is
 *  cleared only on failure. */
async function restoreSession() {
  const candidate = TOKEN;
  if (!candidate) { showLoginView(); return; }
  TOKEN = candidate;
  showLoginView();                       // view only: TOKEN stays set
  const btn = $("#login-btn");
  setLoading(btn, true, t("common.auth.verifying"));
  suppress401.enter();
  try {
    await api("GET", "/health");
    enterApp();
  } catch (e) {
    // Fail closed: a ticket the server rejects must not leave a half-logged-in
    // UI, and must not stay in storage.
    TOKEN = null;
    storeToken(null);
    showLoginView({ expired: e.status === 401 });
    const err = $("#login-error");
    err.textContent = e.status === 401
      ? t("common.auth.sessionExpired")
      : t("common.auth.failed", { msg: e.message });
    err.classList.remove("hidden");
  } finally {
    suppress401.leave();
    setLoading(btn, false);
  }
}

async function tryLogin(token) {
  const btn = $("#login-btn");
  setLoading(btn, true, t("common.auth.signingIn"));
  suppress401.enter();           // a wrong manual token is not an expired session
  TOKEN = token;
  try {
    await api("GET", "/health");
    storeToken(token);           // persist only after the server accepted it
    enterApp();
    toast(t("common.toast.signedIn"), "ok");
  } catch (e) {
    TOKEN = null;                // in-memory only: storage untouched, so a
                                 // mistyped token cannot nuke a stored session
    const err = $("#login-error");
    err.textContent = t("common.auth.failed", { msg: e.message });
    err.classList.remove("hidden");
  } finally {
    suppress401.leave();
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
  $("#logout-btn").addEventListener("click", () => { showLogin(); });
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
  // The lease can move under us (failover) while this page stays open: keep the
  // banner honest without a reload. The request is suppressed-aware for 401s,
  // like every other background call.
  setInterval(() => {
    if (TOKEN) refreshLeaderBanner().catch(() => {});
  }, 30000);
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
    // The banner's text interpolates the node id, so it cannot use `data-i18n`
    // (there is no variable interpolation in `applyStaticI18n`) — it has to be
    // rebuilt for the new language.
    refreshLeaderBanner();
  };
  wireEvents();
  applyStaticI18n();
  // A ticket left in this tab's sessionStorage is validated against the server
  // before the app is shown; only its rejection returns to the login view.
  restoreSession();
});
