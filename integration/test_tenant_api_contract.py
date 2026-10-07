#!/usr/bin/env python3
"""The `dev-docs/tenant-api-integration.md` contract, executed against a live node.

That document is what an EXTERNAL integrator codes against, and §4 + the appendix are the
parts they copy: the nine routes, the method convention, the error envelope, the trace
header, the identity rules, idempotency and the parameter validation. Round 88 pinned the
§6 code→HTTP table statically; this drill pins the rest behaviourally:

  §4.1 identity comes from the TOKEN only (`Host` is not consulted; URL tenant must match
       the token's tenant, else 403 `tenant_id_mismatch`; missing/wrong/unconfigured all
       give `401 unauthorized` with the SAME message)
  §4.2 every response carries `X-Hydra-Trace-Id` and it equals `error.trace_id`; `Retry-After`
       appears only on 429
  §4.3 all non-2xx bodies use the `{"error":{code,message,trace_id}}` envelope
  §4.4 read endpoints accept ONLY their one method (else 405), while the WRITE paths answer
       404 for a wrong method (they are method-sensitive) — and anything else under the
       reserved prefix is 404, answered locally
  §4.5 repeated `invalidate` is safe (`invalidated` drops to 0, not an error); repeated PUT
       converges on the SAME immutable id; DELETE of a missing id is a no-op (204)
  §5.3/§5.6 parameter validation: missing `since` / unknown `group_by` / a too-wide window
       are the documented 400s, and the write endpoints answer the documented shapes

Run: python3 integration/test_tenant_api_contract.py     # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from datetime import datetime, timedelta, timezone

from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "tenant-contract-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA = 18780, 18781
TOKEN = "hydra-tenant-contract-admin-2026"
T1_TOKEN = "tenant-contract-token-t1-2026"
T2_TOKEN = "tenant-contract-token-t2-2026"
# t3 is deliberately left WITHOUT an access token (§4.1: 401 with the same message).

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def call(method, url, token=None, body=None, host=None, timeout=15, raw_body=None):
    """Returns (status, headers, body-text)."""
    if raw_body is not None:
        data, ctype = raw_body.encode(), "application/json"
    else:
        data = None if body is None else json.dumps(body).encode()
        ctype = "application/json"
    headers = {"Content-Type": ctype}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if host:
        headers["Host"] = host
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, dict(r.headers), r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode(errors="replace")
    except Exception as e:
        return 0, {}, str(e)


def admin(method, path, body=None):
    return call(method, f"http://127.0.0.1:{ADMIN}/api/v1{path}", token=TOKEN, body=body)


def tenant(method, path, token=T1_TOKEN, body=None, host="contract.local", raw_body=None):
    return call(method, f"http://127.0.0.1:{DATA}/tenant/t1/api/v1{path}", token=token, body=body,
                host=host, raw_body=raw_body)


def start_node():
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'contract.db')}?mode=rwc",
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
                                 "endpoint": "http://127.0.0.1:18999", "weight": 1,
                                 "created_at": "", "updated_at": ""})
    admin("POST", "/provider-models", {"id": "pm1", "key": "echo", "name": "E",
                                       "provider_id": "p1", "status": 1,
                                       "created_at": "", "updated_at": ""})
    admin("POST", "/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                                     "created_at": ""})
    for tid, token, domain in (("t1", T1_TOKEN, "contract.local"),
                               ("t2", T2_TOKEN, "other-contract.local"),
                               ("t3", None, "no-token.local")):
        body = {"id": tid, "name": tid.upper(), "domain": domain,
                "auth_url": "http://127.0.0.1:18999/auth", "enabled": True,
                "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}
        if token:
            body["access_token"] = token
        st, _, out = admin("POST", "/tenants", body)
        if st not in (200, 201):
            raise SystemExit(f"[tenant-contract] CANNOT VERIFY: seeding {tid} -> {st} {out[:160]}")
        if tid != "t3":
            admin("POST", "/tenant-providers", {"id": f"tp-{tid}", "tenant_id": tid,
                                                "provider_id": "p1", "created_at": "",
                                                "updated_at": ""})
            admin("POST", "/tenant-models", {"id": f"tm-{tid}", "tenant_id": tid,
                                             "model_key": "echo", "created_at": "",
                                             "updated_at": ""})
    admin("POST", "/reload", {})
    time.sleep(0.3)


def envelope_ok(status, headers, text, expect_code=None, where=""):
    """§4.3 + §4.2: the documented shape, and trace_id == the header."""
    try:
        body = json.loads(text)
        err = body["error"]
        code, trace = err["code"], err["trace_id"]
    except Exception:
        check(f"{where}: non-2xx body uses the documented error envelope", False, text[:120])
        return
    check(f"{where}: non-2xx body uses the documented error envelope", True, f"code={code}")
    header_trace = headers.get("X-Hydra-Trace-Id") or headers.get("x-hydra-trace-id")
    check(f"{where}: error.trace_id equals the X-Hydra-Trace-Id header",
          bool(header_trace) and trace == header_trace, f"header={header_trace!r} body={trace!r}")
    if expect_code is not None:
        check(f"{where}: code is `{expect_code}` as documented", code == expect_code, f"code={code}")


# The window a read drill asks for is computed from NOW, not hard-coded: the first version used a
# fixed `since=2026-09-01T00:00:00Z`, which silently aged out of the 31-day ceiling
# (`HYDRA_TENANT_API_USAGE_MAX_WINDOW_DAYS`) and turned these legs red on a DATE rather than on a
# change (measured 2026-10-07: 36 days -> 400 window_too_large). A fixture that is a date is a
# fixture that expires.
SINCE = (datetime.now(timezone.utc) - timedelta(days=1)).strftime("%Y-%m-%dT%H:%M:%SZ")
SINCE_OLD = (datetime.now(timezone.utc) - timedelta(days=60)).strftime("%Y-%m-%dT%H:%M:%SZ")
UNTIL = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

def main():
    if not os.path.exists(BIN):
        print(f"[tenant-contract] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    node = start_node()
    try:
        if not wait_healthy():
            print("[tenant-contract] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "node.log")).read()[-600:], file=sys.stderr)
            return 2
        seed()

        # ---- §4.2 the trace header on EVERY response (2xx included) --------------
        st, hdrs, text = tenant("GET", "/whoami")
        trace = hdrs.get("X-Hydra-Trace-Id") or hdrs.get("x-hydra-trace-id")
        ctype = hdrs.get("Content-Type", "")
        check("§4.2: a 2xx tenant response carries X-Hydra-Trace-Id", st == 200 and bool(trace),
              f"HTTP {st} trace={trace!r}")
        check("§4.2: ...and Content-Type is application/json", "application/json" in ctype, ctype)
        check("§4.2: Retry-After must NOT appear on a 200",
              (hdrs.get("Retry-After") or hdrs.get("retry-after")) is None, f"{hdrs.get('Retry-After')}")

        # ---- §4.1 identity comes from the token, not the Host -------------------
        st_a, _, _ = tenant("GET", "/whoami", host="contract.local")
        st_b, _, _ = tenant("GET", "/whoami", host="totally-unrelated.example")
        check("§4.1: Host does not participate in identity (same answer for any Host)",
              st_a == 200 and st_b == 200, f"right-host={st_a} bogus-host={st_b}")
        st, hdrs, text = call("GET", f"http://127.0.0.1:{DATA}/tenant/t1/api/v1/whoami",
                              token=T2_TOKEN, host="contract.local")
        envelope_ok(st, hdrs, text, "tenant_id_mismatch", "§4.1 URL/token mismatch (t2 token on t1)")
        check("§4.1: ...and the status is 403", st == 403, f"HTTP {st}")

        # ---- §4.1 401 with an IDENTICAL message for the three causes ------------
        texts = {}
        st_none, _, texts["missing"] = tenant("GET", "/whoami", token=None)
        st_bad, _, texts["wrong"] = tenant("GET", "/whoami", token="not-a-token-at-all-1234")
        st_unset, _, texts["unconfigured"] = call(
            "GET", f"http://127.0.0.1:{DATA}/tenant/t3/api/v1/whoami",
            token="any-token-value-1234567890", host="no-token.local")
        check("§4.1: missing / wrong / unconfigured tokens are all 401",
              st_none == 401 and st_bad == 401 and st_unset == 401,
              f"{st_none}/{st_bad}/{st_unset}")
        msgs = {}
        for k, t in texts.items():
            try:
                msgs[k] = json.loads(t)["error"]["message"]
            except Exception:
                msgs[k] = f"<unparseable: {t[:40]}>"
        check("§4.1: ...with the SAME message (no 'which tokens exist' oracle)",
              len(set(msgs.values())) == 1, f"{msgs}")

        # ---- §4.4 the nine routes, and the method convention --------------------
        read_routes = [("GET", "/whoami"), ("GET", f"/usage?since={SINCE}"),
                       ("GET", "/sub-tenants"), ("GET", "/sub-tenant-routes")]
        for method, path in read_routes:
            st, _, text = tenant(method, path)
            check(f"§5: the documented read route {method} {path.split('?')[0]} answers",
                  st == 200, f"HTTP {st} {text[:60]}")
        for method, path in read_routes:
            wrong = "POST" if method == "GET" else "GET"
            st, _, text = tenant(wrong, path)
            check(f"§4.4: {wrong} on the read route {path.split('?')[0]} is 405",
                  st == 405, f"HTTP {st} {text[:60]}")

        # The 405-vs-404 rule, measured cell by cell (the corrected §4.4 table):
        # a path that IS one of the five read routes answers 405 for a wrong method, while a
        # path SHAPE that only exists for writes answers 404 for a method it does not have.
        # (The doc used to give `POST /sub-tenant-routes` as its 404 example; that path is
        # also the read route `sub-tenant-routes`, so the real answer is 405.)
        for method, path, want, why in (
            ("GET", "/sub-tenants/QQ", 404, "a write-only path SHAPE has no GET combination"),
            ("POST", "/sub-tenants/QQ", 404, "a write-only path SHAPE has no POST combination"),
            ("POST", "/sub-tenant-routes", 405, "that path is ALSO the read route (GET only)"),
            ("DELETE", "/sub-tenant-routes", 404, "the delete shape is /sub-tenant-routes/{id}"),
            ("PUT", "/sub-tenants", 404, "the write shape is /sub-tenants/{name}"),
            ("GET", "/auth/cache/invalidate", 405, "that path is the read route (POST only)"),
        ):
            st, _, text = tenant(method, path, body={} if method == "POST" else None)
            check(f"§4.4: {method} {path} -> {want} ({why})", st == want, f"HTTP {st} {text[:60]}")

        # anything else under the reserved prefix is 404, answered locally
        st, hdrs, text = tenant("GET", "/not-a-documented-route")
        envelope_ok(st, hdrs, text, "not_found", "§4.4 unknown path under the reserved prefix")
        check("§4.4: ...and it is 404", st == 404, f"HTTP {st}")

        # ---- §4.5 idempotency: invalidate ---------------------------------------
        st1, _, t1 = tenant("POST", "/auth/cache/invalidate")
        st2, _, t2 = tenant("POST", "/auth/cache/invalidate")
        try:
            first = json.loads(t1)["invalidated"]
            second = json.loads(t2)["invalidated"]
        except Exception:
            first = second = None
        announce("§4.5 repeated invalidate", f"invalidated={first} then {second}")
        check("§4.5: repeating the invalidation is safe (200 twice)",
              st1 == 200 and st2 == 200, f"{st1}/{st2}")
        check("§4.5: ...and the second one reports 0 removed, which is NOT an error",
              isinstance(second, int) and second == 0, f"second={second}")

        # ---- §4.5/§5.6 the write endpoints -------------------------------------
        st, _, text = tenant("PUT", "/sub-tenants/QQ", body={"key_prefix": "QQCX_", "enabled": True})
        try:
            row1 = json.loads(text)["sub_tenant"]
            ver1 = json.loads(text).get("config_version")
        except Exception:
            row1, ver1 = {}, None
        check("§5.6: PUT sub-tenants/{name} returns 200 with the row and a config_version",
              st == 200 and row1.get("id") and isinstance(ver1, int),
              f"HTTP {st} id={row1.get('id')} version={ver1} {text[:80]}")
        st, _, text = tenant("PUT", "/sub-tenants/QQ", body={"key_prefix": "QQCX_", "enabled": False})
        row2 = {}
        try:
            row2 = json.loads(text)["sub_tenant"]
        except Exception:
            pass
        check("§4.5: a repeated PUT converges on the SAME immutable id (and updates the fields)",
              st == 200 and row2.get("id") == row1.get("id") and row2.get("enabled") is False,
              f"HTTP {st} id={row2.get('id')} enabled={row2.get('enabled')}")

        st, _, text = tenant("PUT", "/sub-tenant-routes",
                             body={"sub_tenant_id": row1.get("id"), "provider_id": "p1",
                                   "model_key": None, "enabled": True})
        route = {}
        try:
            route = json.loads(text)["route"]
        except Exception:
            pass
        check("§5.6: PUT sub-tenant-routes returns 200 with the route row",
              st == 200 and route.get("id") and route.get("model_key") is None,
              f"HTTP {st} {text[:90]}")

        st_d1, _, _ = tenant("DELETE", f"/sub-tenant-routes/{route.get('id')}")
        st_d2, _, _ = tenant("DELETE", f"/sub-tenant-routes/{route.get('id')}")
        check("§4.5: DELETE is 204 and deleting an already-missing id is a no-op",
              st_d1 == 204 and st_d2 == 204, f"first={st_d1} second={st_d2}")
        st_d3, _, _ = tenant("DELETE", f"/sub-tenants/{row1.get('id')}")
        check("§5.6: DELETE sub-tenants/{id} is 204", st_d3 == 204, f"HTTP {st_d3}")

        # ---- §5.3 parameter validation -----------------------------------------
        for path, code, why in (
            ("/usage", "invalid_since", "§5.3 `since` is required"),
            (f"/usage?since={SINCE}&group_by=tenant", "invalid_group_by",
             "§5.3 `group_by` whitelist"),
            (f"/usage?since={SINCE_OLD}&until={UNTIL}", "window_too_large",
             "§5.3 window ceiling (31 days)"),
        ):
            st, hdrs, text = tenant("GET", path)
            envelope_ok(st, hdrs, text, code, why)
            check(f"{why}: status is 400", st == 400, f"HTTP {st}")

        # ---- §5.6 body validation ----------------------------------------------
        st, hdrs, text = tenant("PUT", "/sub-tenants/QQ2", raw_body="{not json")
        envelope_ok(st, hdrs, text, "invalid_request", "§5.6 a body that is not JSON")
        check("§5.6: ...and it is 400", st == 400, f"HTTP {st}")
        st, hdrs, text = tenant("PUT", "/sub-tenants/QQ3", body={"key_prefix": "", "enabled": True})
        check("§5.6: an empty key_prefix is refused as documented (400 invalid_key_prefix/empty_key_prefix)",
              st == 400 and json.loads(text or "{}").get("error", {}).get("code", "")
              in ("empty_key_prefix", "invalid_key_prefix"),
              f"HTTP {st} {text[:90]}")

        # ---- §5 writes keep the read side consistent ---------------------------
        st, _, text = tenant("GET", "/sub-tenants")
        ids = [r.get("name") for r in json.loads(text or "{}").get("sub_tenants", [])]
        check("§5.4: a written sub-tenant is gone from the list after DELETE (and the reads stay consistent)",
              st == 200 and "QQ" not in ids and "QQ2" not in ids, f"names={ids}")
    finally:
        stop(node)

    print()
    if failures:
        print(f"TENANT CONTRACT: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("TENANT CONTRACT: PASSED (routes, method convention, envelope + trace id, identity, "
          "idempotency, parameter validation)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
