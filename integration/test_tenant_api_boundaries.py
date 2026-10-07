#!/usr/bin/env python3
"""The tenant API's documented BOUNDARIES (§7 of tenant-api-integration.md), executed.

§7 is the part an integrator reads before an incident, and three of its claims are load-
bearing and previously unverified black-box (the in-process suite covers §7.1's snapshot
view, nothing covered the proxy/HTTP surface):

  §7.1 a tenant whose row is `enabled=false` is refused on its CLIENT requests but the whole
       tenant API keeps working — that is the documented SELF-SERVICE recovery path ("if you
       cannot even get in, you cannot recover"). Steps 1 and 3 of the documented recovery
       flow are executed here, and every read route plus the write routes are checked while
       the tenant is suspended.
  §7.3 `api_keys` cannot be a PREFIX: it is treated as one complete key, so it matches
       nothing, `invalidated` is 0 and the client keeps being served — the document calls
       this "silently ineffective", so the silence is what gets asserted.
  §7.5 the contract has NO `tenant_id` query parameter (it must be IGNORED, not honoured and
       not rejected — honouring it would be a cross-tenant read), and a window with no
       records answers `as_of: null` instead of an error.

Run: python3 integration/test_tenant_api_boundaries.py     # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "tenant-boundaries-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18790, 18791, 18799
TOKEN = "hydra-tenant-boundaries-admin-2026"
T1_TOKEN = "tenant-boundaries-token-t1-2026"
T2_TOKEN = "tenant-boundaries-token-t2-2026"
CLIENT_KEY = "sk-tenant-1"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def call(method, url, token=None, body=None, host=None, timeout=20):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if host:
        headers["Host"] = host
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)


def admin(method, path, body=None):
    return call(method, f"http://127.0.0.1:{ADMIN}/api/v1{path}", token=TOKEN, body=body)


def tenant(method, path, token=T1_TOKEN, body=None):
    return call(method, f"http://127.0.0.1:{DATA}/tenant/t1/api/v1{path}", token=token, body=body,
                host="boundaries.local")


def code_of(text):
    try:
        return json.loads(text)["error"]["code"]
    except Exception:
        return f"<no code: {text[:40]}>"


class Upstream(BaseHTTPRequestHandler):
    """One mock for both the auth hop and the chat path; `allowed` is flipped by the test."""

    allowed = True

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        if self.path.startswith("/auth"):
            body = json.dumps({"allowed": Upstream.allowed, "reason": "boundaries",
                               "expires_in": 300}).encode()
        else:
            body = json.dumps({"id": "chatcmpl-b", "object": "chat.completion",
                               "choices": [{"index": 0,
                                            "message": {"role": "assistant", "content": "ok"},
                                            "finish_reason": "stop"}],
                               "usage": {"prompt_tokens": 5, "completion_tokens": 8,
                                         "total_tokens": 13}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def proxied(key=CLIENT_KEY):
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions",
                token=key, host="boundaries.local",
                body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})


def start_node():
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'boundaries.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, "node.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health")[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed():
    admin("POST", "/providers", {"id": "p1", "key": "p1", "name": "P",
                                 "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                                 "created_at": "", "updated_at": ""})
    admin("POST", "/provider-models", {"id": "pm1", "key": "echo", "name": "E",
                                       "provider_id": "p1", "status": 1,
                                       "created_at": "", "updated_at": ""})
    admin("POST", "/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                                     "created_at": ""})
    for tid, token, domain in (("t1", T1_TOKEN, "boundaries.local"),
                               ("t2", T2_TOKEN, "other-boundaries.local")):
        st, out = admin("POST", "/tenants", {
            "id": tid, "name": tid.upper(), "domain": domain,
            "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
            "access_token": token, "cert_key": None, "cert_file": None,
            "created_at": "", "updated_at": ""})
        if st not in (200, 201):
            raise SystemExit(f"[boundaries] CANNOT VERIFY: seeding {tid} -> {st} {out[:160]}")
        admin("POST", "/tenant-providers", {"id": f"tp-{tid}", "tenant_id": tid,
                                           "provider_id": "p1", "created_at": "", "updated_at": ""})
        admin("POST", "/tenant-models", {"id": f"tm-{tid}", "tenant_id": tid,
                                         "model_key": "echo", "created_at": "", "updated_at": ""})
    admin("POST", "/reload", {})
    time.sleep(0.3)


def set_enabled(enabled):
    st, out = admin("PUT", "/tenants/t1", {
        "id": "t1", "name": "T1", "domain": "boundaries.local",
        "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": enabled,
        "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""})
    admin("POST", "/reload", {})
    time.sleep(0.3)
    return st


def main():
    if not os.path.exists(BIN):
        print(f"[boundaries] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    node = start_node()
    try:
        if not wait_healthy():
            print("[boundaries] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "node.log")).read()[-600:], file=sys.stderr)
            return 2
        seed()

        # ---- §7.1 the documented recovery path, while suspended ---------------
        st, text = proxied()
        check("§7 (control): an ENABLED tenant's client request is served", st == 200,
              f"HTTP {st} {text[:70]}")
        put = set_enabled(False)
        st_proxy, text_proxy = proxied()
        announce("§7.1 a client request while suspended", f"HTTP {st_proxy} code={code_of(text_proxy)}")
        check("§7.1: ...is REFUSED (the tenant's own policy wins)", st_proxy != 200,
              f"HTTP {st_proxy}")
        try:
            proxy_err = json.loads(text_proxy)["error"]
        except Exception:
            proxy_err = {}
        check("§7.1: ...as 403 naming `tenant_disabled` (distinguishable from a bad token)",
              st_proxy == 403 and proxy_err.get("message") == "tenant_disabled",
              f"HTTP {st_proxy} error={proxy_err}")
        # The document now states the difference explicitly: the data-plane envelope has
        # `message` + `type` and NO `code`/`trace_id`, while the tenant API's has both.
        check("§7.1: the data-plane envelope is the documented `{message,type}` shape (no `code`)",
              proxy_err.get("type") == "proxy_error" and "code" not in proxy_err
              and "trace_id" not in proxy_err, f"error={proxy_err}")
        st, text = tenant("GET", "/whoami")
        st_err, text_err = call("GET", f"http://127.0.0.1:{DATA}/tenant/t1/api/v1/nope",
                                token=T1_TOKEN, host="boundaries.local")
        try:
            api_err = json.loads(text_err)["error"]
        except Exception:
            api_err = {}
        check("§7.1: ...while the tenant API envelope DOES carry `code` and `trace_id`",
              set(api_err) >= {"code", "message", "trace_id"}, f"error={api_err}")

        st, text = tenant("GET", "/whoami")
        try:
            who = json.loads(text)
        except Exception:
            who = {}
        check("§7.1 step 1: `whoami` still works and SHOWS the suspension",
              st == 200 and who.get("enabled") is False, f"HTTP {st} enabled={who.get('enabled')}")
        st, text = tenant("POST", "/auth/cache/invalidate", body={})
        check("§7.1 step 3: the recovery `invalidate` still works while suspended",
              st == 200, f"HTTP {st} {text[:70]}")
        for path in ("/usage?since=2026-09-01T00:00:00Z", "/sub-tenants", "/sub-tenant-routes"):
            st, text = tenant("GET", path)
            check(f"§7.1: the read route {path.split('?')[0]} still works while suspended",
                  st == 200, f"HTTP {st} {text[:60]}")
        st, text = tenant("PUT", "/sub-tenants/SUSP", body={"key_prefix": "SUSP_", "enabled": True})
        sub_id = ""
        try:
            sub_id = json.loads(text)["sub_tenant"]["id"]
        except Exception:
            pass
        check("§7.1: the WRITE routes still work while suspended (that is the self-service path)",
              st == 200 and bool(sub_id), f"HTTP {st} {text[:80]}")
        if sub_id:
            st, _ = tenant("DELETE", f"/sub-tenants/{sub_id}")
            check("§7.1: ...and so does cleanup", st == 204, f"HTTP {st}")

        set_enabled(True)
        st, text = tenant("GET", "/whoami")
        check("§7.1: re-enabling is visible on the next `whoami`",
              st == 200 and json.loads(text or "{}").get("enabled") is True,
              f"HTTP {st} enabled={json.loads(text or '{}').get('enabled')}")

        # ---- §7.3 a PREFIX in `api_keys` is a silent no-op --------------------
        Upstream.allowed = True
        st_warm, _ = proxied()
        Upstream.allowed = False
        st_cached, _ = proxied()
        st, text = tenant("POST", "/auth/cache/invalidate", body={"api_keys": [CLIENT_KEY[:6]]})
        try:
            inv_prefix = json.loads(text)["invalidated"]
        except Exception:
            inv_prefix = None
        st_after_prefix, _ = proxied()
        announce("§7.3 prefix invalidation",
                 f"warm={st_warm} cached={st_cached} invalidated={inv_prefix} after={st_after_prefix}")
        check("§7.3: the cache really held a verdict before this leg",
              st_warm == 200 and st_cached == 200, f"{st_warm}/{st_cached}")
        check("§7.3: a PREFIX is treated as one complete key -> `invalidated` is 0",
              st == 200 and inv_prefix == 0, f"HTTP {st} invalidated={inv_prefix}")
        check("§7.3: ...and the client keeps being served (the document calls this SILENTLY ineffective)",
              st_after_prefix == 200, f"HTTP {st_after_prefix}")
        st, text = tenant("POST", "/auth/cache/invalidate", body={"api_keys": [CLIENT_KEY]})
        try:
            inv_full = json.loads(text)["invalidated"]
        except Exception:
            inv_full = None
        st_after_full, body_full = proxied()
        announce("§7.3 full-key invalidation",
                 f"invalidated={inv_full} after={st_after_full} {body_full[:40]}")
        check("§7.3: the documented workaround (send the COMPLETE key) does clear it",
              st == 200 and isinstance(inv_full, int) and inv_full >= 1 and st_after_full == 401,
              f"invalidated={inv_full} after={st_after_full}")
        Upstream.allowed = True

        # ---- §7.5 no `tenant_id` query parameter ------------------------------
        base = "/usage?since=2026-09-01T00:00:00Z"
        st_a, text_a = tenant("GET", base)
        st_b, text_b = tenant("GET", f"{base}&tenant_id=t2")
        try:
            owner_a = json.loads(text_a).get("tenant_id")
            owner_b = json.loads(text_b).get("tenant_id")
        except Exception:
            owner_a = owner_b = None
        announce("§7.5 tenant_id query parameter", f"without={st_a}/{owner_a} with={st_b}/{owner_b}")
        check("§7.5: an extra `tenant_id` parameter is IGNORED (not a 400)",
              st_b == 200, f"HTTP {st_b} {text_b[:60]}")
        check("§7.5: ...and it does NOT switch the answer to that tenant (no cross-tenant read)",
              st_a == 200 and owner_a == "t1" and owner_b == "t1", f"{owner_a!r} vs {owner_b!r}")

        # ---- §7.5 an empty window is `as_of: null`, not an error --------------
        st, text = tenant("GET", "/usage?since=2026-09-01T00:00:00Z&until=2026-09-01T00:00:01Z")
        try:
            empty = json.loads(text)
        except Exception:
            empty = {}
        check("§7.5: a window with no records answers 200 with `as_of: null`",
              st == 200 and empty.get("as_of") is None and empty.get("requests") in (0, None),
              f"HTTP {st} as_of={empty.get('as_of')!r} requests={empty.get('requests')!r}")
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"TENANT BOUNDARIES: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("TENANT BOUNDARIES: PASSED (suspended tenant keeps the self-service API, prefix "
          "invalidation is a silent no-op as documented, no tenant_id query parameter, empty "
          "window is as_of: null)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
