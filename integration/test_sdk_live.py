#!/usr/bin/env python3
"""The tenant Python SDK (`tools/hydra-py`) against a LIVE node, not a mock.

`tools/hydra-py` documents a precise contract in its README
(`dev-docs/tenant-api-integration.md` §5.2 is the underlying contract):

  * the endpoint is `POST {data-plane node}/tenant/{tenant_id}/api/v1/auth/cache/
    invalidate`, on the DATA plane (8080), because it "was moved off the management
    API on 2026-09-17" (`POST /api/v1/tenants/{id}/auth/cache/invalidate` is gone);
  * `200 single_node` = this node IS the whole data plane ⇒ done; `200 applied` =
    every live node confirmed; `202 pending` = published, NOT confirmed (the SDK
    raises `InvalidatePendingError` — "a 2xx is not the same as done"); `503
    unavailable` = the channel exists but could not do its job (SDK:
    `InvalidateUnavailableError`);
  * `X-Hydra-Trace-Id`, `event_id`, `nodes_total`, `waited_ms` survive to the caller;
  * `wait=none` publishes and does NOT wait.

Its own suite (787 lines) runs against MOCKS. Nothing ever pointed it at a real
node — so "the documented usage works" and "the real node emits those states" were
both untested. That is what this drill does.

Default legs need only `target/debug/hydra` (SERVER features, no Redis):
  S1 the README's own usage runs         -> 200 single_node + trace id + done
  S2 the call really CLEARS the cache    -> allow, flip to deny (still allowed,
                                            cached), SDK invalidate -> 401
  S3 another tenant's token              -> HTTPError (not a "done" state)
  S4 the management-API path is gone     -> 404 on the admin port
The `--cluster` legs (C1-C3: `applied` + an `event_id`, `202 pending` -> InvalidatePendingError,
`503 unavailable` -> InvalidateUnavailableError on a node whose bus cannot answer) were RETIRED
on 2026-10-07 with the topology they measured. They started two PLAIN single-node instances
(`HYDRA_REDIS_MODE=single`, no `HYDRA_CLUSTER_PEERS`), which correctly answer
`state: "single_node"` — so every C1-C3 assertion had been false since ADR-0001 replaced the
Redis leader lease with raft, and those legs could only ever report exit 1/2. The retired CI
steps in `.github/workflows/ci.yml` record where each half of that coverage lives now.

Run: python3 integration/test_sdk_live.py
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import socketserver
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "sdk-live-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
SDK_PATH = os.path.join(ROOT, "tools", "hydra-py")
sys.path.insert(0, SDK_PATH)

ADMIN, DATA = 18730, 18731            # single-node legs
UPSTREAM = 18739
TOKEN = "hydra-sdk-live-admin-2026"
TENANT_TOKEN = "tenant-self-service-token-2026"      # >= MIN_TENANT_TOKEN_LEN (16)
OTHER_TOKEN = "another-tenants-token-20260930"
DEAD_REDIS = "redis://127.0.0.1:18999"
REDIS = os.environ.get("HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380")

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def call(method, url, token=None, body=None, timeout=30):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, dict(r.headers), r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode(errors="replace")
    except Exception as e:
        return 0, {}, str(e)


class AuthUpstream(BaseHTTPRequestHandler):
    """The tenant's `auth_url`. `allowed` is flipped by the test, which is what makes
    the cache-clearing assertion (S2) meaningful."""

    allowed = True

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        body = json.dumps({"allowed": AuthUpstream.allowed,
                           "reason": "sdk-live", "expires_in": 300}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass




def crash(cmd):
    raise SystemExit(f"[sdk-live] CANNOT VERIFY: {cmd}")










def node_env(admin, data, label, extra=None):
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin}",
        "HYDRA_LISTEN": f"127.0.0.1:{data}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "warn",
    })
    if extra:
        env.update(extra)
    return env


def start(admin, data, label, extra=None):
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=node_env(admin, data, label, extra),
                            stdout=log, stderr=subprocess.STDOUT)






def wait_healthy(admin, token=TOKEN, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{admin}/api/v1/health", token=token)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed_core(admin):
    """Provider + model + sealed key: the part that is shared, created once."""
    for path, payload in [
        ("providers", {"id": "p1", "key": "p1", "name": "P", "endpoint": f"http://127.0.0.1:{UPSTREAM}",
                       "weight": 1, "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
    ]:
        st, _, out = call("POST", f"http://127.0.0.1:{admin}/api/v1/{path}", token=TOKEN, body=payload)
        if st not in (200, 201):
            raise SystemExit(f"[sdk-live] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")


def seed_tenant(admin, tenant_id, tenant_token, domain):
    """Tenant + its links + the self-service access token the SDK authenticates with."""
    for path, payload in [
        ("tenants", {"id": tenant_id, "name": "T",
                     "domain": domain, "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "access_token": tenant_token,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": f"tp-{tenant_id}", "tenant_id": tenant_id, "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": f"tm-{tenant_id}", "tenant_id": tenant_id, "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]:
        st, _, out = call("POST", f"http://127.0.0.1:{admin}/api/v1/{path}", token=TOKEN, body=payload)
        if st not in (200, 201):
            raise SystemExit(f"[sdk-live] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")
    call("POST", f"http://127.0.0.1:{admin}/api/v1/reload", token=TOKEN, body={})
    time.sleep(0.3)


def proxied(data_port, api_key="sk-tenant-1"):
    """A data-plane request; the `Host` header selects the tenant."""
    body = {"model": "echo", "messages": [{"role": "user", "content": "hi"}]}
    req = urllib.request.Request(
        f"http://127.0.0.1:{data_port}/v1/chat/completions",
        data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", "Host": "load.local",
                 "Authorization": f"Bearer {api_key}"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)


def sdk_client(nodes, token=TENANT_TOKEN, **kw):
    from hydra_sdk import HydraClient
    return HydraClient(token=token, nodes=nodes, **kw)


def leg_single_node():
    """S1-S4 against a node with no cluster bus."""
    import hydra_sdk

    node = start(ADMIN, DATA, "single")
    try:
        if not wait_healthy(ADMIN):
            print("[sdk-live] CANNOT VERIFY: the single node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "single.log")).read()[-600:], file=sys.stderr)
            return 2
        seed_core(ADMIN)
        seed_tenant(ADMIN, "t1", TENANT_TOKEN, "load.local")
        seed_tenant(ADMIN, "t2", OTHER_TOKEN, "other.local")

        # ---- S1: the README's own usage ----------------------------------------
        client = sdk_client([f"http://127.0.0.1:{DATA}"])
        try:
            res = client.invalidate_with_result("t1")
        finally:
            client.close()
        announce("S1 the SDK's documented call returned",
                 f"HTTP {res.http_status} state={res.state!r} nodes={res.nodes_applied}/"
                 f"{res.nodes_total} event={res.event_id} waited={res.waited_ms}ms trace={res.trace_id}")
        check("S1: a single data-plane node answers 200 single_node", res.http_status == 200,
              f"HTTP {res.http_status} state={res.state}")
        check("S1: ...and the state is the documented `single_node`", res.state == hydra_sdk.FleetState.SINGLE_NODE,
              f"state={res.state!r}")
        check("S1: ...which the SDK reports as done (`res.done`)", res.done is True, f"done={res.done}")
        check("S1: the response's X-Hydra-Trace-Id survives to the caller",
              bool(res.trace_id), f"trace_id={res.trace_id!r}")
        check("S1: nodes_total is filled in (the SDK does not invent it)",
              isinstance(res.nodes_total, int) and res.nodes_total >= 1, f"{res.nodes_total}")

        # ---- S2: does the call actually clear the NODE's cache? -----------------
        # The precondition matters: without step (c) this leg would also pass if the
        # allow verdict were simply never cached.
        AuthUpstream.allowed = True
        st_allow, body_allow = proxied(DATA)
        AuthUpstream.allowed = False
        st_cached, _ = proxied(DATA)
        announce("S2 requests", f"allow={st_allow} after-flip={st_cached} (the cached allow is the point)")
        check("S2: with the auth service allowing, the proxied request succeeds", st_allow == 200,
              f"HTTP {st_allow} {body_allow[:60]}")
        check("S2: after the auth service flips to DENY the request is still served — the verdict is CACHED",
              st_cached == 200, f"HTTP {st_cached} (if this is 401 the cache never held, so the "
                                f"invalidation below would prove nothing)")
        client = sdk_client([f"http://127.0.0.1:{DATA}"])
        try:
            client.invalidate_tenant_auth_cache("t1")
        finally:
            client.close()
        st_after, body_after = proxied(DATA)
        announce("S2 the request AFTER the SDK invalidation", f"HTTP {st_after} {body_after[:80]}")
        check("S2: after the SDK invalidation the cached allow is GONE (the node re-asks and denies)",
              st_after == 401, f"HTTP {st_after} {body_after[:80]}")
        AuthUpstream.allowed = True

        # ---- S3: another tenant's token ----------------------------------------
        client = sdk_client([f"http://127.0.0.1:{DATA}"], token=OTHER_TOKEN)
        try:
            try:
                res3 = client.invalidate_with_result("t1")
                check("S3: another tenant's token must NOT invalidate t1", False,
                      f"it returned state={res3.state!r} HTTP {res3.http_status}")
            except hydra_sdk.HTTPError as e:
                check("S3: another tenant's token is refused as an HTTPError (not a 'done' state)",
                      e.status in (401, 403, 404), f"status={e.status}")
        finally:
            client.close()

        # ---- S5: ...and NOT on the management PORT either ------------------------
        client = sdk_client([f"http://127.0.0.1:{ADMIN}"], token=TENANT_TOKEN)
        try:
            try:
                res5 = client.invalidate_with_result("t1")
                check("S5: the tenant API must not live on the admin port", False,
                      f"it returned HTTP {res5.http_status} state={res5.state!r}")
            except Exception as e:
                st5 = getattr(e, "status", None)
                check("S5: pointing the SDK at the admin port fails (the README's 'not the admin port')",
                      st5 in (401, 403, 404), f"{type(e).__name__} status={st5}")
        finally:
            client.close()

        # ---- S4: the README says the management-API path is GONE ----------------
        for path in ("/api/v1/tenants/t1/auth/cache/invalidate",
                     "/api/v1/tenant/t1/api/v1/auth/cache/invalidate"):
            st4, _, body4 = call("POST", f"http://127.0.0.1:{ADMIN}{path}", token=TOKEN, body={})
            check(f"S4: the old management path {path} does not exist", st4 == 404,
                  f"HTTP {st4} {body4[:80]}")
    finally:
        stop(node)
    return 0


def main():
    if not os.path.exists(BIN):
        print(f"[sdk-live] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    if not os.path.isdir(SDK_PATH):
        print(f"[sdk-live] CANNOT VERIFY: {SDK_PATH} is missing", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), AuthUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    try:
        rc = leg_single_node()
        if rc:
            return rc
    finally:
        upstream.shutdown()

    print()
    if failures:
        print(f"SDK LIVE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
        print("SDK LIVE: PASSED (documented usage, single_node, cache really cleared, other "
              "tenant refused, management-API path gone)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
