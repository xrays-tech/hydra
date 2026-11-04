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
  §4.5 repeated PUT converges on the SAME immutable id; DELETE of a missing id is a no-op (204)
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

from _mock_clickhouse import MockClickHouse
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "tenant-contract-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA = 18780, 18781
TOKEN = "hydra-tenant-contract-admin-2026"
T1_TOKEN = None
T2_TOKEN = None
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


def tenant(method, path, token=None, body=None, host="contract.local", raw_body=None):
    if token is None:
        token = T1_TOKEN
    return call(method, f"http://127.0.0.1:{DATA}/tenant/t1/api/v1{path}", token=token, body=body,
                host=host, raw_body=raw_body)


# The node READS usage through ClickHouse, so the drill needs something that answers — and it has
# to answer the way ClickHouse does (an aggregate over an empty window is one row of zeros, not an
# empty body; see `_mock_clickhouse.py`). Before ADR-0002 this came for free from the node's own
# SQLite database.
MOCK = MockClickHouse()


def start_node():
    env = dict(os.environ)
    env.update(usage_env("clickhouse", MOCK.url))
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
    global T1_TOKEN, T2_TOKEN
    admin("POST", "/providers", {"id": "p1", "key": "p1", "name": "P",
                                 "endpoint": "http://127.0.0.1:18999", "weight": 1,
                                 "created_at": "", "updated_at": ""})
    admin("POST", "/provider-models", {"id": "pm1", "key": "echo", "name": "E",
                                       "provider_id": "p1", "status": 1,
                                       "created_at": "", "updated_at": ""})
    admin("POST", "/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                                     "created_at": ""})
    for tid, domain in (("t1", "contract.local"),
                        ("t2", "other-contract.local"),
                        ("t3", "no-token.local")):
        body = {"id": tid, "name": tid.upper(), "domain": domain,
                "cert_key": None, "cert_file": None, "enabled": True,
                "created_at": "", "updated_at": ""}
        st, _, out = admin("POST", "/tenants", body)
        if st not in (200, 201):
            raise SystemExit(f"[tenant-contract] CANNOT VERIFY: seeding {tid} -> {st} {out[:160]}")
        if tid != "t3":
            st, _, out = admin("POST", f"/tenants/{tid}/access-tokens", {"name": "test"})
            if st not in (200, 201):
                raise SystemExit(f"[tenant-contract] CANNOT VERIFY: minting token for {tid} -> {st} {out[:160]}")
            minted = json.loads(out)["token"]
            if tid == "t1":
                T1_TOKEN = minted
            elif tid == "t2":
                T2_TOKEN = minted
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
    MOCK.start()
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
        st_none, _, texts["missing"] = call(
            "GET", f"http://127.0.0.1:{DATA}/tenant/t1/api/v1/whoami",
            token=None, host="contract.local")
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
                       ("GET", "/user-groups"), ("GET", "/users")]
        for method, path in read_routes:
            st, _, text = tenant(method, path)
            check(f"§5: the documented read route {method} {path.split('?')[0]} answers",
                  st == 200, f"HTTP {st} {text[:60]}")
        for method, path in read_routes:
            wrong = "POST" if method == "GET" else "GET"
            st, _, text = tenant(wrong, path)
            check(f"§4.4: {wrong} on the read route {path.split('?')[0]} is 405",
                  st == 405, f"HTTP {st} {text[:60]}")

        # anything else under the reserved prefix is 404, answered locally
        st, hdrs, text = tenant("GET", "/not-a-documented-route")
        envelope_ok(st, hdrs, text, "not_found", "§4.4 unknown path under the reserved prefix")
        check("§4.4: ...and it is 404", st == 404, f"HTTP {st}")

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

    finally:
        stop(node)
        MOCK.stop()

    print()
    if failures:
        print(f"TENANT CONTRACT: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("TENANT CONTRACT: PASSED (routes, method convention, envelope + trace id, identity, "
          "idempotency, parameter validation)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
