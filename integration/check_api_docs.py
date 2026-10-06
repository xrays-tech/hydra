#!/usr/bin/env python3
"""Check that every endpoint the Admin UI documents actually exists.

`admin-ui/api-docs.js` is the in-app API reference an operator reads inside the
product. It is hand-written, and nothing verified it against the router — so it can
advertise routes or METHODS the server does not serve, and the operator finds out by
calling them.

Two failure shapes count as drift:
  * `404 … "unknown path"` — the path is not routed;
  * `405 method_not_allowed` — the PATH is routed but this METHOD is not (the first
    version of this probe only looked for the first shape and therefore reported
    "no drift" while `PUT /api/v1/tenant-models/{id}` was documented and refused).

Everything else means the route was reached: 200/201/204, 400 (bad body), 401
(cluster-gated), 404 with a RESOURCE message ("provider not found" — the route ran
and looked the id up), 409/422 … all fine.

PHASE 2 — the documented request BODIES must be usable. `api-docs.js` carries an
example body per write endpoint, and that example is what an operator copy-pastes.
This session has twice been bitten by exactly that shape of bug (two e2e fixtures and
the CLI's `update` all omitted fields the handler requires), so every documented body
is POSTed/PUT here and a `400 … missing field …` is reported as drift.

Usage:
  HYDRA_BASE_URL=http://127.0.0.1:18081 HYDRA_ADMIN_TOKEN=… python3 integration/check_api_docs.py
  python3 integration/check_api_docs.py --docs path/to/api-docs.js   # for the guard's own test
Exit: 0 = every documented endpoint is routed, 1 = drift, 2 = setup problem.
"""
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_DOCS = os.path.join(ROOT, "admin-ui", "api-docs.js")
BASE = os.environ.get("HYDRA_BASE_URL", "http://127.0.0.1:18081").rstrip("/")
TOKEN = os.environ.get("HYDRA_ADMIN_TOKEN", "")
PROBE_ID = "probe-does-not-exist"


DOCS_LOADER = """
const fs = require("fs");
const code = fs.readFileSync(process.argv[1], "utf8");
const stubs = { getItem: () => null, setItem() {}, removeItem() {} };
const api = new Function("localStorage", "navigator", "document", "window",
  code + "; return { API_DOCS };")(
  stubs, { language: "en-US", clipboard: null },
  { documentElement: { lang: "" }, querySelectorAll: () => [] }, {});
const out = [];
for (const tag of api.API_DOCS) {
  for (const e of tag.endpoints || []) {
    // `auth` / `resp` / `errors` are carried through as well: the error-contract
    // probe needs them (token-free endpoints, role-dependent success statuses, and
    // the documented per-endpoint error codes). Omitting them made that probe's
    // cross-check vacuous while still printing "undocumented" for everything.
    out.push({
      method: e.method,
      path: e.path,
      body: e.body === undefined ? null : e.body,
      auth: e.auth === undefined ? null : e.auth,
      resp: e.resp || [],
      errors: (e.errors || []).map((x) => ({ status: x.status, code: x.code })),
    });
  }
}
process.stdout.write(JSON.stringify(out));
"""


def documented_endpoints(docs_path):
    """Every documented endpoint, loaded by RUNNING the file's own JS.

    The first version parsed `api-docs.js` with a regex and passed each documented
    body through verbatim — but these are JS object literals with UNQUOTED KEYS, so
    every probe sent invalid JSON and the server answered `key must be a string`.
    All 14 "drifts" were the probe's own bug. Loading the file with node gives the
    real values (and `check_i18n.js` solves the same problem the same way)."""
    proc = subprocess.run(
        ["node", "-e", DOCS_LOADER, docs_path],
        capture_output=True, text=True, timeout=30,
    )
    if proc.returncode != 0:
        raise RuntimeError(f"could not load {docs_path} with node: {proc.stderr.strip()[:200]}")
    return json.loads(proc.stdout)


def call_with_body(method, path, body_text):
    """Send a documented body (now real JSON, produced by the node loader)."""
    return call_raw(method, BASE + path, body_text.encode())


def call_raw(method, url, data):
    req = urllib.request.Request(
        url, data=data, method=method,
        headers={"Authorization": f"Bearer {TOKEN}", "Content-Type": "application/json"},
    )
    try:
        with urllib.request.urlopen(req, timeout=5) as resp:
            return resp.status, resp.read(300).decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read(300).decode(errors="replace")
    except Exception as e:  # connection refused, timeout, …
        return 0, str(e)


def call(method, path):
    url = BASE + re.sub(r"\{[^}]+\}", PROBE_ID, path)
    data = b"{}" if method in ("POST", "PUT", "PATCH") else None
    return call_raw(method, url, data)


def main():
    docs = DEFAULT_DOCS
    if len(sys.argv) > 2 and sys.argv[1] == "--docs":
        docs = sys.argv[2]
    if not TOKEN:
        print("check_api_docs: HYDRA_ADMIN_TOKEN is required", file=sys.stderr)
        return 2
    try:
        endpoints = documented_endpoints(docs)
    except RuntimeError as e:
        print(f"check_api_docs: {e}", file=sys.stderr)
        return 2
    pairs = [(e["method"], e["path"]) for e in endpoints]
    if len(pairs) < 20:
        print(f"check_api_docs: only {len(pairs)} documented endpoints parsed — "
              "the pattern in api-docs.js probably changed", file=sys.stderr)
        return 2
    # The instance must be up, otherwise every probe "fails" for the wrong reason.
    status, body = call("GET", "/api/v1/health")
    if status != 200:
        print(f"check_api_docs: instance not healthy ({status} {body[:80]})", file=sys.stderr)
        return 2

    # ── phase 1: routes and methods ────────────────────────────────────────────
    drift = []
    for method, path in pairs:
        status, body = call(method, path)
        why = None
        if "unknown path" in body:
            why = "not routed"
        elif status == 405:
            why = "method not allowed"
        if why:
            drift.append((method, path, status, why))
            print(f"DRIFT  {method:6s} {path:46s} {status} {why}")
        else:
            print(f"ok     {method:6s} {path:46s} {status}")

    # ── phase 2: the documented request BODIES ────────────────────────────────
    # Endpoints whose body triggers an OUTBOUND request are skipped: probing them is
    # a different test (and they are slow / non-deterministic).
    SKIP_BODIES = {
        ("POST", "/api/v1/tenants/auth/test"): "makes the server call the auth_url",
    }
    bodies = [(e["method"], e["path"], e["body"]) for e in endpoints if e["body"]]
    checked_bodies = 0
    for method, path, body in bodies:
        if (method, path) in SKIP_BODIES:
            print(f"skip   {method:6s} {path:46s} body not probed: {SKIP_BODIES[(method, path)]}")
            continue
        target = re.sub(r"\{[^}]+\}", PROBE_ID, path) if method == "PUT" else path
        status, resp = call_with_body(method, target, json.dumps(body))
        checked_bodies += 1
        if status == 400 and ("missing field" in resp or "invalid_json" in resp):
            drift.append((method, path, status, "documented body is rejected"))
            print(f"DRIFT  {method:6s} {path:46s} {status} documented body rejected: {resp[:120]}")
        else:
            print(f"ok     {method:6s} {path:46s} {status} (documented body accepted as a shape)")
    if checked_bodies < 8:
        print(f"check_api_docs: only {checked_bodies} documented bodies probed (< 8) — "
              "the body pattern in api-docs.js probably changed", file=sys.stderr)
        return 2

    print(f"\ndocumented endpoints: {len(pairs)} | documented bodies: {checked_bodies} | drift: {len(drift)}")
    return 1 if drift else 0


if __name__ == "__main__":
    sys.exit(main())
