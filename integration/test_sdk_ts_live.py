#!/usr/bin/env python3
"""The tenant TypeScript SDK (`tools/hydra-ts`) against a LIVE node.

`tools/hydra-ts` documents exactly the same tenant contract as `tools/hydra-py`
(`README.md`: `200 applied` / `200 single_node` = done, `202 pending` is NOT done,
`503 unavailable` = the fleet was never told, `X-Hydra-Trace-Id` survives to the
caller, the endpoint is on the DATA plane and NOT the admin port). Round 80 pointed the
Python SDK at a real gateway; the TypeScript one had never left its mocked suite — and
two implementations of one documented contract can drift. This drill runs the TS SDK
against a real node (single-node legs), against a real bus (cluster legs), and finally
compares what the two SDKs report about the SAME node.

The TS SDK is exercised through a small generated Node driver (`DRIVER`), which imports
the built `dist/client.js` and prints one JSON object per leg on stdout.

Legs (server feature set, no Redis needed):
  T1 the documented usage        -> 200 single_node, isDoneState, trace id, node counts
  T2 the call really CLEARS      -> allow, flip to deny (still served = cached), TS SDK
                                    invalidate -> 401
  T3 another tenant's token      -> HTTPError, not a "done" state
  T4 the management path is gone -> 404 on the admin port
With `--cluster` (cluster feature set + `HYDRA_TEST_REDIS_URL`):
  C1 wait=converged on a live bus-> 200 applied (2/2 with an edge registered)
  C2 wait=none                   -> 202 pending => InvalidatePendingError
  C3 a bus cut under the node    -> 503 unavailable => InvalidateUnavailableError
  P  PARITY: the TS and the Python SDK must report the same state/counts for one node
             (two implementations, one contract — the divergence the drills exist for)

Run: python3 integration/test_sdk_ts_live.py [--cluster]
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import socket
import socketserver
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "sdk-ts-live")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
SDK = os.path.join(ROOT, "tools", "hydra-ts")
CLIENT = os.path.join(SDK, "dist", "client.js")
DRIVER = os.path.join(DIR, "driver.mjs")

ADMIN, DATA = 18760, 18761
ADMIN_C, DATA_C = 18762, 18763
ADMIN_X, DATA_X = 18764, 18765
UPSTREAM = 18769
TOKEN = "hydra-sdk-ts-admin-2026"
TENANT_TOKEN = "tenant-ts-self-service-2026"
OTHER_TOKEN = "another-tenants-token-20260930"
CLUSTER_TOKEN = "hydra-sdk-ts-cluster-2026"
REDIS = os.environ.get("HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380")
REDIS_DB = int(os.environ.get("HYDRA_SDK_TS_REDIS_DB", "49"))

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
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)


class AuthUpstream(BaseHTTPRequestHandler):
    allowed = True

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        body = json.dumps({"allowed": AuthUpstream.allowed, "reason": "sdk-ts",
                           "expires_in": 300}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


class RedisRelay:
    """A TCP relay in front of the real Redis so the bus can be CUT under a running
    node (a dead Redis URL cannot produce `503 unavailable`: the node refuses to
    start — measured in round 80)."""

    def __init__(self, upstream_host, upstream_port):
        self.upstream = (upstream_host, upstream_port)
        self.server = None
        self.port = None
        self.sockets = []

    def start(self):
        relay = self

        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                try:
                    up = socket.create_connection(relay.upstream, timeout=5)
                except OSError:
                    return
                relay.sockets.extend([self.request, up])
                threading.Thread(target=relay._pump, args=(self.request, up), daemon=True).start()
                relay._pump(up, self.request)

        self.server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self.port

    @staticmethod
    def _pump(src, dst):
        try:
            while True:
                chunk = src.recv(65536)
                if not chunk:
                    break
                dst.sendall(chunk)
        except OSError:
            pass
        finally:
            for s in (src, dst):
                try:
                    s.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def cut(self):
        self.server.shutdown()
        self.server.server_close()
        for s in self.sockets:
            try:
                s.close()
            except OSError:
                pass


def redis_endpoint():
    host = REDIS.split("://", 1)[-1].split("/")[0]
    if ":" in host:
        h, p = host.split(":", 1)
        return h, int(p)
    return host, 6379


def redis_cmd(sock, *parts):
    payload = ("*%d\r\n" % len(parts)).encode()
    for part in parts:
        b = str(part).encode()
        payload += b"$%d\r\n" % len(b) + b + b"\r\n"
    sock.sendall(payload)
    return sock.recv(256)


def flush_db():
    host, port = redis_endpoint()
    try:
        sock = socket.create_connection((host, port), timeout=5)
    except OSError as e:
        return None, f"{host}:{port} unreachable ({e})"
    redis_cmd(sock, "SELECT", REDIS_DB)
    before = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    redis_cmd(sock, "FLUSHDB")
    after = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    sock.close()
    return f"redis://{host}:{port}/{REDIS_DB}", f"db {REDIS_DB}: keys {before} -> {after}"


DRIVER_SOURCE = """\
// Generated by integration/test_sdk_ts_live.py — drives the BUILT TS SDK.
import { HydraClient, isDoneState } from %(client)s;

const [mode, base, token, tenant, ...rest] = process.argv.slice(2);
const out = (o) => process.stdout.write(JSON.stringify(o));

function describe(res) {
  return {
    httpStatus: res.httpStatus, state: res.state, done: isDoneState(res.state),
    nodesApplied: res.nodesApplied, nodesTotal: res.nodesTotal, lagging: res.lagging,
    eventId: res.eventId ?? null, waitedMs: res.waitedMs ?? null,
    traceId: res.traceId ?? null, node: res.node, checked: res.checked,
  };
}

const waitMode = mode.startsWith('none') ? 'none' : undefined;
const client = new HydraClient({ token, nodes: [base], waitMode, disableBackgroundRecheck: true });
try {
  if (mode === 'result' || mode === 'none-result') {
    const res = await client.invalidateWithResult(tenant);
    out({ ok: true, result: describe(res) });
  } else if (mode === 'raise' || mode === 'none-raise') {
    try {
      await client.invalidateTenantAuthCache(tenant);
      out({ ok: true, raised: null });
    } catch (e) {
      out({
        ok: true, raised: e?.constructor?.name ?? 'Error', message: String(e?.message ?? '').slice(0, 120),
        result: e?.result ? describe(e.result) : null,
      });
    }
  } else {
    out({ ok: false, error: 'unknown mode ' + mode });
  }
} catch (e) {
  out({ ok: false, error: e?.constructor?.name + ': ' + String(e?.message ?? '').slice(0, 160) });
} finally {
  client.close();
}
"""


def write_driver():
    # The SDK is ESM; the driver is a module in the sidecar dir importing the built file
    # by absolute path (file:// URL) so node resolves it regardless of cwd.
    src = DRIVER_SOURCE % {"client": json.dumps("file://" + CLIENT)}
    with open(DRIVER, "w", encoding="utf-8") as f:
        f.write(src)


def ts(mode, base, token, tenant="t1", timeout=60):
    p = subprocess.run(["node", DRIVER, mode, base, token, tenant],
                       capture_output=True, text=True, timeout=timeout)
    try:
        return json.loads(p.stdout), p.returncode, p.stderr
    except Exception:
        return None, p.returncode, (p.stdout + p.stderr)[:300]


def node_env(admin, data, label, extra=None):
    env = dict(os.environ)
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


def wait_healthz(admin, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{admin}/healthz")[0] == 200:
            return True
        time.sleep(0.25)
    return False


def wait_leader(admin, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{admin}/healthz/leader")[0] == 200:
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
    for path, payload in [
        ("providers", {"id": "p1", "key": "p1", "name": "P", "endpoint": f"http://127.0.0.1:{UPSTREAM}",
                       "weight": 1, "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
    ]:
        st, out = call("POST", f"http://127.0.0.1:{admin}/api/v1/{path}", token=TOKEN, body=payload)
        if st not in (200, 201):
            raise SystemExit(f"[sdk-ts] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")


def seed_tenant(admin, tenant_id, tenant_token, domain):
    for path, payload in [
        ("tenants", {"id": tenant_id, "name": "T", "domain": domain,
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "access_token": tenant_token,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": f"tp-{tenant_id}", "tenant_id": tenant_id, "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": f"tm-{tenant_id}", "tenant_id": tenant_id, "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]:
        st, out = call("POST", f"http://127.0.0.1:{admin}/api/v1/{path}", token=TOKEN, body=payload)
        if st not in (200, 201):
            raise SystemExit(f"[sdk-ts] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")
    call("POST", f"http://127.0.0.1:{admin}/api/v1/reload", token=TOKEN, body={})
    time.sleep(0.3)


def proxied(data_port, api_key="sk-tenant-1"):
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


def python_sdk_result(base, token, tenant):
    """The OTHER implementation of the same contract (tools/hydra-py), for the parity leg."""
    sys.path.insert(0, os.path.join(ROOT, "tools", "hydra-py"))
    from hydra_sdk import HydraClient  # noqa: E402
    client = HydraClient(token=token, nodes=[base], disable_background_recheck=True)
    try:
        res = client.invalidate_with_result(tenant)
        return res
    finally:
        client.close()


def leg_single_node():
    node = start(ADMIN, DATA, "single")
    try:
        if not wait_healthy(ADMIN):
            print("[sdk-ts] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "single.log")).read()[-600:], file=sys.stderr)
            return 2
        seed_core(ADMIN)
        seed_tenant(ADMIN, "t1", TENANT_TOKEN, "load.local")
        seed_tenant(ADMIN, "t2", OTHER_TOKEN, "other.local")
        base = f"http://127.0.0.1:{DATA}"

        # ---- T1: the documented usage -----------------------------------------
        got, rc, err = ts("result", base, TENANT_TOKEN)
        check("T1: the TS SDK's documented call returns a result", bool(got and got.get("ok")),
              f"rc={rc} out={got} err={err[:120]}")
        if got and got.get("ok"):
            res = got["result"]
            announce("T1 the TS SDK's documented call", f"{res}")
            check("T1: a single data-plane node answers 200 single_node",
                  res["httpStatus"] == 200 and res["state"] == "single_node", f"{res['httpStatus']}")
            check("T1: ...which the SDK reports as done (`isDoneState`)", res["done"] is True)
            check("T1: the X-Hydra-Trace-Id survives to the caller", bool(res["traceId"]),
                  f"traceId={res['traceId']!r}")
            check("T1: node counts are filled in (not invented)",
                  isinstance(res["nodesTotal"], int) and res["nodesTotal"] >= 1, f"{res['nodesTotal']}")

        # ---- T2: does it really clear the cache? ------------------------------
        AuthUpstream.allowed = True
        st_allow, _ = proxied(DATA)
        AuthUpstream.allowed = False
        st_cached, _ = proxied(DATA)
        got, rc, err = ts("raise", base, TENANT_TOKEN)
        st_after, body_after = proxied(DATA)
        announce("T2 requests", f"allow={st_allow} after-flip={st_cached} after-invalidate={st_after}")
        check("T2: the verdict is cached (the flip alone does not change the answer)",
              st_allow == 200 and st_cached == 200, f"{st_allow}/{st_cached}")
        check("T2: after the TS SDK's invalidate the cached allow is GONE",
              st_after == 401, f"HTTP {st_after} {body_after[:60]}")
        AuthUpstream.allowed = True

        # ---- T3: another tenant's token ---------------------------------------
        got, rc, err = ts("raise", base, OTHER_TOKEN)
        raised = (got or {}).get("raised")
        check("T3: another tenant's token is refused (HTTPError, not a 'done' state)",
              raised == "HTTPError", f"raised={raised} out={got}")

        # ---- T4: the management path is gone ----------------------------------
        for path in ("/api/v1/tenants/t1/auth/cache/invalidate",
                     "/api/v1/tenant/t1/api/v1/auth/cache/invalidate"):
            st, body = call("POST", f"http://127.0.0.1:{ADMIN}{path}", token=TOKEN, body={})
            check(f"T4: the old management path {path} does not exist", st == 404,
                  f"HTTP {st} {body[:60]}")
    finally:
        stop(node)
    return 0


def leg_cluster():
    redis_db, note = flush_db()
    if redis_db is None:
        print(f"[sdk-ts] CANNOT VERIFY: {note}", file=sys.stderr)
        return 2
    announce("redis for the cluster leg", f"{redis_db} ({note})")
    common = {
        "HYDRA_REDIS_MODE": "single",
        "HYDRA_USAGE_SINK": "clickhouse", "HYDRA_CLICKHOUSE_URL": "http://127.0.0.1:18999",
    }
    live = start(ADMIN_C, DATA_C, "cluster-live", dict(common, **{
        "HYDRA_NODE_ID": "sdk-ts-a",
        "HYDRA_REDIS_URL": redis_db,
    }))
    host, port = redis_endpoint()
    relay = RedisRelay(host, port)
    relay_port = relay.start()
    relay_url = f"redis://127.0.0.1:{relay_port}/{REDIS_DB}"
    edge = start(ADMIN_X, DATA_X, "cluster-edge", dict(common, **{
        "HYDRA_NODE_ID": "sdk-ts-edge",
        "HYDRA_REDIS_URL": relay_url,
    }))
    try:
        if not wait_healthy(ADMIN_C):
            print("[sdk-ts] CANNOT VERIFY: the cluster node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "cluster-live.log")).read()[-700:], file=sys.stderr)
            return 2
        if not wait_leader(ADMIN_C):
            print("[sdk-ts] CANNOT VERIFY: the cluster node never won the lease", file=sys.stderr)
            return 2
        edge_ready = wait_healthz(ADMIN_X)
        check("C0: the edge node behind the relay is up (role-correct probe /healthz)",
              edge_ready, f"healthz ready={edge_ready}")
        seed_core(ADMIN_C)
        seed_tenant(ADMIN_C, "t1", TENANT_TOKEN, "load.local")
        base = f"http://127.0.0.1:{DATA_C}"

        # ---- C1: converged -----------------------------------------------------
        got, rc, err = ts("result", base, TENANT_TOKEN)
        res = (got or {}).get("result") or {}
        announce("C1 wait=converged on a live bus", f"{res}")
        check("C1: a node with a live bus reports 200 applied",
              res.get("httpStatus") == 200 and res.get("state") == "applied", f"{res}")
        check("C1: ...with the LIVE node counts (the edge is registered too)",
              (res.get("nodesTotal") or 0) >= 1 and res.get("nodesApplied") == res.get("nodesTotal"),
              f"{res.get('nodesApplied')}/{res.get('nodesTotal')}")
        check("C1: ...and an event id to reconcile against later", bool(res.get("eventId")),
              f"eventId={res.get('eventId')!r}")

        # ---- C2: wait=none -> 202 pending --------------------------------------
        got, rc, err = ts("none-result", base, TENANT_TOKEN)
        res = (got or {}).get("result") or {}
        announce("C2 wait=none (returning API)", f"{res}")
        check("C2: wait=none publishes without waiting -> 202 pending",
              res.get("httpStatus") == 202 and res.get("state") == "pending", f"{res}")
        check("C2: ...`isDoneState` is false for it", res.get("done") is False, f"done={res.get('done')}")
        got, rc, err = ts("none-raise", base, TENANT_TOKEN)
        check("C2: the RAISING api throws InvalidatePendingError for that 202",
              (got or {}).get("raised") == "InvalidatePendingError",
              f"raised={(got or {}).get('raised')} out={got}")

        # ---- P: parity with the Python SDK on the SAME node --------------------
        ts_res = (ts("result", base, TENANT_TOKEN)[0] or {}).get("result") or {}
        py_res = python_sdk_result(base, TENANT_TOKEN, "t1")
        announce("P the two SDKs on the same node",
                 f"ts state={ts_res.get('state')} {ts_res.get('nodesApplied')}/{ts_res.get('nodesTotal')} | "
                 f"py state={py_res.state} {py_res.nodes_applied}/{py_res.nodes_total}")
        check("P: both SDKs report the SAME fleet state for the same node",
              ts_res.get("state") == py_res.state, f"ts={ts_res.get('state')} py={py_res.state}")
        check("P: ...and the same node counts",
              (ts_res.get("nodesApplied"), ts_res.get("nodesTotal"))
              == (py_res.nodes_applied, py_res.nodes_total),
              f"ts={ts_res.get('nodesApplied')}/{ts_res.get('nodesTotal')} "
              f"py={py_res.nodes_applied}/{py_res.nodes_total}")

        # ---- C3: a bus cut under the node -> 503 unavailable -------------------
        relay.cut()
        time.sleep(0.5)
        got, rc, err = ts("none-result", f"http://127.0.0.1:{DATA_X}", TENANT_TOKEN)
        res = (got or {}).get("result") or {}
        announce("C3 cut bus on the edge", f"{res}")
        check("C3: a bus that cannot answer is 503 `unavailable`, not a success",
              res.get("httpStatus") == 503 and res.get("state") == "unavailable", f"{res}")
        check("C3: ...and NOT reported as `pending` (a different failure)",
              res.get("state") != "pending", f"state={res.get('state')}")
        got, rc, err = ts("none-raise", f"http://127.0.0.1:{DATA_X}", TENANT_TOKEN)
        check("C3: the RAISING api throws InvalidateUnavailableError for that 503",
              (got or {}).get("raised") == "InvalidateUnavailableError",
              f"raised={(got or {}).get('raised')}")
    finally:
        stop(live)
        stop(edge)
        try:
            relay.cut()
        except Exception:
            pass
    return 0


def main():
    if not os.path.exists(BIN):
        print(f"[sdk-ts] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    if not os.path.exists(CLIENT):
        print(f"[sdk-ts] CANNOT VERIFY: {CLIENT} is not built "
              f"(cd tools/hydra-ts && npm install && npm run build)", file=sys.stderr)
        return 2
    cluster_mode = "--cluster" in sys.argv
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    write_driver()

    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), AuthUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    try:
        rc = leg_cluster() if cluster_mode else leg_single_node()
        if rc:
            return rc
    finally:
        upstream.shutdown()

    print()
    if failures:
        print(f"SDK-TS LIVE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    if cluster_mode:
        print("SDK-TS LIVE (cluster): PASSED (200 applied, 202 pending → InvalidatePendingError, "
              "503 unavailable → InvalidateUnavailableError, parity with the Python SDK)")
    else:
        print("SDK-TS LIVE: PASSED (documented usage, single_node, cache really cleared, other "
              "tenant refused, management-API path gone)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
