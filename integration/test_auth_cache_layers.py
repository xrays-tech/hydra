#!/usr/bin/env python3
"""The auth cache's TWO layers, measured — including the claim that L2 is SHARED.

`design.md` / `dev-docs/design-tenant-api.md` describe a two-layer auth verdict cache: an
in-process L1 and a Redis-backed L2 that exists so that "a verdict one node learned is not
re-learned by the next node". `ops.md` §9.1 relies on it (`hydra_auth_cache_clear_total{layer,
result}`), the tenant contract's §7.4 residual-window argument relies on the item TTLs, and
decision item **D-9** asks whether L2 should become a trait — a question that needs the
current behaviour written down. Nothing had ever measured it black-box.

The drill uses a real cluster (TWO members of a three-member raft list) with a real Redis and a
COUNTING auth mock, so
"was the tenant's auth_url asked again?" is a number, not a guess:

  A L1 works: a second request on the same node does not re-ask the auth service
  B L2 is really written: the verdict appears in Redis under `hydra:{auth}:…`
  C **L2 is SHARED**: a request on the SECOND node is served WITHOUT re-asking the auth
    service (the verdict came from Redis, not from a second upstream call)
  D invalidation clears it FLEET-WIDE: after an invalidate on the other node, the first node
    asks the auth service again and (with the upstream flipped to deny) refuses the request
  E ...and the Redis entry is gone afterwards, i.e. the clear reached L2 and not just L1

Run: HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_auth_cache_layers.py
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
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
DIR = os.path.join(ROOT, ".acceptance", "auth-cache-layers-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
# ALL THREE members are started, and that is not decoration. The fleet view the invalidation barrier
# waits on is the STATIC MEMBER LIST (ADR-0001 D-2: there is no registry and no per-peer liveness), so
# a cluster with one member down can never report `applied` — `DELETE /auth/cache` stays `202 pending`
# with the absent member in `lagging`, forever. Measured 2026-10-05: with 2 of 3 up, the D leg below
# saw `202 {"invalidated":1,…,"fleet":{"state":"pending"}}` and every "the clear converged" assertion
# failed. That is the honest behaviour of a static fleet (and `ops.md` §5.2 already documents
# `pending` as "in flight, not a failure"), so the DRILL runs a complete cluster rather than
# weakening the assertions.
A_ADMIN, A_DATA, A_RAFT = 18820, 18821, 18826
B_ADMIN, B_DATA, B_RAFT = 18822, 18823, 18827
C_ADMIN, C_DATA, C_RAFT = 18824, 18825, 18828
UPSTREAM = 18829
TOKEN = "hydra-auth-layers-admin-2026"
MEMBERS = f"layers-a=127.0.0.1:{A_RAFT},layers-b=127.0.0.1:{B_RAFT},layers-c=127.0.0.1:{C_RAFT}"
CLIENT_KEY = "sk-tenant-1"
REDIS = os.environ.get("HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380")
REDIS_DB = int(os.environ.get("HYDRA_LAYERS_REDIS_DB", "51"))

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


# --------------------------------------------------------------------------
# A counting, flippable auth upstream.
# --------------------------------------------------------------------------
class AuthState:
    allowed = True
    hits = 0
    lock = threading.Lock()


class AuthUpstream(BaseHTTPRequestHandler):
    """Serves BOTH the tenant's `auth_url` (/auth) and the provider endpoint (/v1/...).

    The counter counts ONLY the auth hop. The first version counted every POST, so it was
    really measuring upstream chat calls too — and legs A and C "failed" because of that,
    not because the cache was broken.
    """

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        if self.path.startswith("/auth"):
            with AuthState.lock:
                AuthState.hits += 1
                allowed = AuthState.allowed
            body = json.dumps({"allowed": allowed, "reason": "layers",
                               "expires_in": 300}).encode()
        else:
            body = json.dumps({"id": "chatcmpl-layers", "object": "chat.completion",
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


def hits():
    with AuthState.lock:
        return AuthState.hits


def reset_hits():
    with AuthState.lock:
        AuthState.hits = 0


# --------------------------------------------------------------------------
# Raw RESP (no redis-cli in this environment).
# --------------------------------------------------------------------------
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
    time.sleep(0.05)
    return sock.recv(65536)


def redis_session():
    host, port = redis_endpoint()
    sock = socket.create_connection((host, port), timeout=5)
    redis_cmd(sock, "SELECT", REDIS_DB)
    return sock


def flush_db():
    try:
        sock = redis_session()
    except OSError as e:
        return None, f"{redis_endpoint()} unreachable ({e})"
    before = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    redis_cmd(sock, "FLUSHDB")
    after = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    sock.close()
    return f"redis://{redis_endpoint()[0]}:{redis_endpoint()[1]}/{REDIS_DB}", f"db {REDIS_DB}: keys {before} -> {after}"


def cache_value(key):
    try:
        sock = redis_session()
    except OSError:
        return None
    out = redis_cmd(sock, "GET", key).decode(errors="replace")
    sock.close()
    lines = [l for l in out.replace("\r\n", "\n").split("\n") if l]
    return lines[-1] if lines else None


def index_members(tenant="t1"):
    """The members of the tenant index set.

    Round 167: this stripped the `*<n>` array header but NOT the `$<len>` bulk-string header, so a
    one-member set was reported as TWO members (`['$64', '<hash>']` — measured) and every "the index
    has ≥1 member" assertion was satisfied by framing alone.
    """
    try:
        sock = redis_session()
    except OSError:
        return []
    out = redis_cmd(sock, "SMEMBERS", f"hydra:{{auth:idx}}:{tenant}").decode(errors="replace")
    sock.close()
    return [l for l in out.replace("\r\n", "\n").split("\n")
            if l and not l.startswith("*") and not l.startswith("$") and not l.isdigit()]


def cache_keys(tenant="t1"):
    """The L2 VERDICT keys of the auth cache, for one tenant.

    Round 167: `KEYS hydra:{auth}:*` also matches the tenant INDEX key
    (`hydra:{auth:idx}:<tenant>`, a SET), so the old version returned index keys among the verdicts:
    `len(keys) >= 1` then could not distinguish "a verdict was written to L2" from "only the index
    exists", and `keys[0]` happening to be the index made `GET` answer `WRONGTYPE` — a red leg for the
    wrong reason. The index prefix is filtered out here, and callers ask for the verdict key only.
    """
    try:
        sock = redis_session()
    except OSError:
        return []
    out = redis_cmd(sock, "KEYS", f"hydra:{{auth}}:{tenant}:*").decode(errors="replace")
    sock.close()
    prefix = f"hydra:{{auth}}:{tenant}:"
    return [k for k in out.replace("\r\n", "\n").split("\n")
            if k.startswith(prefix) and not k.startswith("hydra:{auth:idx}:")]


# --------------------------------------------------------------------------
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


def admin(port, method, path, body=None):
    return call(method, f"http://127.0.0.1:{port}/api/v1{path}", token=TOKEN, body=body)


def proxied(data_port, key=CLIENT_KEY):
    return call("POST", f"http://127.0.0.1:{data_port}/v1/chat/completions", token=key,
                host="layers.local",
                body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})


def tenant_invalidate(data_port):
    return call("POST", f"http://127.0.0.1:{data_port}/tenant/t1/api/v1/auth/cache/invalidate",
                token="layers-tenant-token-1234", host="layers.local", body={})


def node_env(admin_port, data_port, label, extra):
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN,
        "HYDRA_CLUSTER_PEERS": MEMBERS, "HYDRA_CLUSTER_ID": "auth-layers-drill",
        "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin_port}", "HYDRA_LISTEN": f"127.0.0.1:{data_port}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_REDIS_MODE": "single",
        "HYDRA_USAGE_SINK": "clickhouse", "HYDRA_CLICKHOUSE_URL": "http://127.0.0.1:18999",
        "RUST_LOG": "warn",
    })
    env.update(extra)
    return env


def start(admin_port, data_port, label, extra):
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=node_env(admin_port, data_port, label, extra),
                            stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(port, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin(port, "GET", "/health")[0] == 200:
            return True
        time.sleep(0.25)
    return False


def wait_for(cond, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        try:
            if cond():
                return True
        except Exception:
            pass
        time.sleep(0.25)
    return False


def wait_leader(port, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{port}/healthz/leader")[0] == 200:
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
    for path, payload in (
        ("/providers", {"id": "p1", "key": "p1", "name": "P",
                        "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                            "created_at": ""}),
        ("/tenants", {"id": "t1", "name": "T", "domain": "layers.local",
                      "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                      "access_token": "layers-tenant-token-1234",
                      "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
    ):
        st, out = admin(A_ADMIN, "POST", path, payload)
        if st not in (200, 201):
            raise SystemExit(f"[layers] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")
    admin(A_ADMIN, "POST", "/reload", {})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[layers] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    redis_url, note = flush_db()
    if redis_url is None:
        print(f"[layers] CANNOT VERIFY: {note}", file=sys.stderr)
        return 2
    announce("redis for this drill", f"{redis_url} ({note})")
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), AuthUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    common = {"HYDRA_REDIS_URL": redis_url}
    # BOTH members at once: one node of a three-member table can never claim its data directory (the
    # claim is a raft write and needs a majority), so a sequential bring-up would time out with "no
    # member adopted this node's Arachne data directory" — measured 2026-10-05.
    a = start(A_ADMIN, A_DATA, "layers-a", dict(common, **{
        "HYDRA_NODE_ID": "layers-a", "HYDRA_ARACHNE_LISTEN": f"127.0.0.1:{A_RAFT}",
        "HYDRA_ARACHNE_DATA_DIR": os.path.join(DIR, "raft-a")}))
    b = start(B_ADMIN, B_DATA, "layers-b", dict(common, **{
        "HYDRA_NODE_ID": "layers-b", "HYDRA_ARACHNE_LISTEN": f"127.0.0.1:{B_RAFT}",
        "HYDRA_ARACHNE_DATA_DIR": os.path.join(DIR, "raft-b")}))
    c = start(C_ADMIN, C_DATA, "layers-c", dict(common, **{
        "HYDRA_NODE_ID": "layers-c", "HYDRA_ARACHNE_LISTEN": f"127.0.0.1:{C_RAFT}",
        "HYDRA_ARACHNE_DATA_DIR": os.path.join(DIR, "raft-c")}))
    try:
        if not all(wait_healthy(p) for p in (A_ADMIN, B_ADMIN, C_ADMIN)):
            print("[layers] CANNOT VERIFY: a member never became healthy", file=sys.stderr)
            for label in ("layers-a", "layers-b", "layers-c"):
                print(f"--- {label} ---\n"
                      f"{open(os.path.join(DIR, label + '.log')).read()[-700:]}", file=sys.stderr)
            return 2
        # Exactly one of the three must be the raft writer — and it can be ANY of them (the writer is
        # whichever member raft elected), so all three are asked. Probing with a bare GET rather than
        # `wait_leader` keeps the check inside one pass: nesting a 25-second waiter inside `wait_for`
        # made the writer's own poll consume the outer budget.
        if not wait_for(lambda: sum(
            1 for p in (A_ADMIN, B_ADMIN, C_ADMIN)
            if call("GET", f"http://127.0.0.1:{p}/healthz/leader")[0] == 200
        ) == 1, budget=30):
            print("[layers] CANNOT VERIFY: no single raft writer", file=sys.stderr)
            return 2
        # The other member must have materialized the config before it can serve the tenant at all.
        deadline = time.time() + 20
        while time.time() < deadline:
            if proxied(B_DATA)[0] in (200, 401, 403):
                break
            time.sleep(0.5)
        seed()
        deadline = time.time() + 20
        while time.time() < deadline:
            if proxied(B_DATA)[0] == 200:
                break
            time.sleep(0.5)
        # Make the cache COLD on purpose: the warm-up above populated L1+L2, so "the first
        # request asks the auth service" can only be measured after a clear (the first
        # version asserted it against a warm cache and was wrong).
        #
        # Round 167: the response was DISCARDED, so "the clear fan out to every node" was assumed
        # rather than checked — and the C leg below ("the other node serves it WITHOUT asking the auth
        # service, i.e. from the shared L2") is only evidence if THIS clear really emptied the other
        # node's L1 too (otherwise it answers from its own warm L1 and L2 is never read). The
        # invalidation response carries the fleet report, so require it to say every node applied.
        st_inv0, out_inv0 = tenant_invalidate(A_DATA)
        try:
            fleet0 = json.loads(out_inv0).get("fleet", {})
        except ValueError:
            fleet0 = {}
        check("PREMISE: the cold clear reached EVERY node before L2 is measured "
              "(the fleet report says applied)",
              st_inv0 == 200 and fleet0.get("nodes_total", 0) >= 2
              and fleet0.get("nodes_applied") == fleet0.get("nodes_total"),
              f"HTTP {st_inv0} fleet={fleet0}")
        time.sleep(0.3)
        reset_hits()
        announce("auth-service call counter reset after a cold clear", f"hits={hits()}")

        # ---- A: L1 -------------------------------------------------------------
        st1, _ = proxied(A_DATA)
        after_first = hits()
        st2, _ = proxied(A_DATA)
        after_second = hits()
        announce("A L1 on the leader", f"first={st1} hits={after_first} · second={st2} hits={after_second}")
        check("A: with a cold cache the first request is served and asks the auth service once",
              st1 == 200 and after_first == 1, f"HTTP {st1} hits={after_first}")
        check("A: the second request on the SAME node does NOT re-ask (L1 hit)",
              st2 == 200 and after_second == after_first, f"hits={after_second}")

        # ---- B: L2 is really written -------------------------------------------
        keys = cache_keys()
        value = cache_value(keys[0]) if keys else None
        announce("B Redis L2 entry", f"keys={keys} value={value!r} index={index_members()}")
        check("B: the verdict is in Redis under `hydra:{auth}:…` (L2 is really used)",
              len(keys) >= 1, f"keys={keys}")
        # The index must name THIS verdict (not merely exist): the tenant clear walks it, so an
        # index that does not point at the cached key would leave the entry behind on invalidation.
        key_suffix = keys[0].rsplit(":", 1)[-1] if keys else None
        check("B: ...encoded as the documented allow verdict `1`, and indexed by its own key hash "
              "for tenant clears",
              value == "1" and index_members() == [key_suffix],
              f"value={value!r} index={index_members()} key hash={key_suffix!r}")

        # ---- C: L2 is SHARED across nodes --------------------------------------
        before_b = hits()
        st_b, body_b = proxied(B_DATA)
        after_b = hits()
        announce("C the SAME client key on the EDGE",
                 f"HTTP {st_b} hits {before_b} -> {after_b}")
        check("C: the second node serves the request", st_b == 200, f"HTTP {st_b} {body_b[:60]}")
        check("C: ...WITHOUT asking the auth service again — the verdict came from the shared L2",
              after_b == before_b, f"hits {before_b} -> {after_b}")

        # ---- D: invalidation clears it fleet-wide ------------------------------
        AuthState.allowed = False
        st_inv, out_inv = tenant_invalidate(B_DATA)
        check("D: the invalidation on the OTHER member is accepted", st_inv == 200, f"HTTP {st_inv} {out_inv[:60]}")
        # The L2 clear must be visible BEFORE any new request re-caches a verdict.
        keys_cleared = cache_keys()
        announce("D the Redis L2 right after the invalidation",
                 f"keys={keys_cleared} index={index_members()}")
        check("D: the clear reached L2 — the Redis verdict is gone (the invalidation is not "
              "L1-only)", keys_cleared == [], f"keys={keys_cleared}")
        before_a = hits()
        st_a_after, body_a_after = proxied(A_DATA)
        after_a = hits()
        announce("D the original node after the fleet invalidation",
                 f"HTTP {st_a_after} hits {before_a} -> {after_a}")
        check("D: the first node now RE-ASKS the auth service (its cached allow is gone)",
              after_a > before_a, f"hits {before_a} -> {after_a}")
        check("D: ...and refuses the request, which is what the invalidation was for",
              st_a_after == 401, f"HTTP {st_a_after} {body_a_after[:60]}")

        # ---- E: the DENY verdict is cached again (with the shorter deny TTL) -----
        keys_after = cache_keys()
        value_after = cache_value(keys_after[0]) if keys_after else None
        announce("E Redis L2 after the re-asked request", f"keys={keys_after} value={value_after!r}")
        check("E: the refused request cached its DENY verdict in L2 as `0`",
              value_after == "0", f"value={value_after!r} keys={keys_after}")
    finally:
        stop(a)
        stop(b)
        stop(c)
        upstream.shutdown()

    print()
    if failures:
        print(f"AUTH CACHE LAYERS: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("AUTH CACHE LAYERS: PASSED (L1 per node, L2 written to Redis, L2 shared across "
          "nodes, fleet invalidation reaches both layers)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
