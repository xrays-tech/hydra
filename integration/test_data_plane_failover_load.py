#!/usr/bin/env python3
"""The DATA PLANE under load across a node kill — shared DB, no raft, no leader.

Raft-era history: this drill used to kill the raft leader and assert that a follower's proxied
requests stayed 200 through the election. There is no election now. Three identical nodes share
one relational database and one Redis data plane; every node serves from its own in-process cache
and writes the DB directly. Killing one node must therefore be invisible to a request stream aimed
at a SURVIVOR, and the survivors must keep serving.

This is the shared-DB replacement for acceptance row 2 (20 rps across a failover). The load is
driven at a node that is NOT the victim; the victim is `kill -9`'d mid-run. All bounded.

Assertions:
  A  the loaded node serves a real data-plane request before the run (the model catalog, which is
     resolved from the shared config locally — not merely an open port)
  B  ~20 rps for the whole window (issued >= 90% of planned)
  C  ZERO non-200 responses for the whole run — the core acceptance assertion
  D  the requests within ±3 s of the kill are all 200 (a mean would hide a short outage)
  E  the victim really died, and both surviving nodes still serve the data plane afterwards

Run:
  export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
  export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse,db-postgres --bin hydra
  python3 integration/test_data_plane_failover_load.py
Env: HYDRA_LOAD_SECONDS (default 20), HYDRA_LOAD_RPS (default 20), HYDRA_LOAD_KILL_AT (default 8)
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

from _redis_harness import RedisSetupError, db_url, endpoint as redis_endpoint, flush_db, require_url as require_redis_url
from _shared_db import SetupError, isolated
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
DIR = os.path.join(ROOT, ".acceptance", "data-plane-failover")

ADMIN_TOKEN = "hydra-load-admin-2026"
ENCRYPTION_KEY = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
TENANT_DOMAIN = "load.local"
MODEL = "echo"
REDIS_DB = int(os.environ.get("HYDRA_LOAD_REDIS_DB", "59"))

LOAD_SECONDS = float(os.environ.get("HYDRA_LOAD_SECONDS", "20"))
LOAD_RPS = float(os.environ.get("HYDRA_LOAD_RPS", "20"))
KILL_AT_S = float(os.environ.get("HYDRA_LOAD_KILL_AT", "8"))
NEAR_KILL_S = 3.0

UPSTREAM_PORT = None
failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail=""):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def call(method, url, token=None, body=None, timeout=20, host=None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
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
    except Exception as e:  # refused / reset / timeout
        return 0, {}, str(e)


class Upstream(BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"object":"list","data":[]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        body = json.dumps(
            {
                "id": "chatcmpl-load",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 5, "completion_tokens": 8, "total_tokens": 13},
            }
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


class Node:
    def __init__(self, name, db_url_value, redis_url_value, log_path):
        self.name = name
        self.admin_port = free_port()
        self.data_port = free_port()
        self.db_url = db_url_value
        self.redis_url = redis_url_value
        self.log_path = log_path
        self.proc = None

    def env(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith("HYDRA_")}
        env.update(usage_env())
        env.update(
            {
                "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN,
                "HYDRA_ENCRYPTION_KEY": ENCRYPTION_KEY,
                "HYDRA_NODE_ID": self.name,
                "HYDRA_ADMIN_ADDR": f"127.0.0.1:{self.admin_port}",
                "HYDRA_LISTEN": f"127.0.0.1:{self.data_port}",
                "HYDRA_DB_URL": self.db_url,
                "HYDRA_REDIS_URL": self.redis_url,
                "HYDRA_REDIS_MODE": "single",
                "RUST_LOG": "warn",
            }
        )
        return env

    def start(self):
        with open(self.log_path, "w") as log:
            self.proc = subprocess.Popen(
                [BIN], env=self.env(), stdout=log, stderr=subprocess.STDOUT
            )
        return self

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def kill(self):
        proc = self.proc
        if proc is not None and proc.poll() is None:
            os.kill(proc.pid, signal.SIGKILL)
            proc.wait(timeout=10)

    def admin(self, method, path, body=None, timeout=20):
        return call(
            method,
            f"http://127.0.0.1:{self.admin_port}/api/v1{path}",
            token=ADMIN_TOKEN,
            body=body,
            timeout=timeout,
        )

    def healthy(self):
        return self.admin("GET", "/health", timeout=3)[0] == 200

    def catalog(self, timeout=10):
        st, _, out = call(
            "GET",
            f"http://127.0.0.1:{self.data_port}/v1/models",
            timeout=timeout,
            host=TENANT_DOMAIN,
        )
        try:
            return st, sorted(m["id"] for m in json.loads(out).get("data", []))
        except Exception:
            return st, None

    def log_tail(self, n=900):
        try:
            with open(self.log_path, errors="replace") as fh:
                return fh.read()[-n:]
        except OSError:
            return "<no log>"


def wait_for(cond, budget, what):
    deadline = time.time() + budget
    while time.time() < deadline:
        try:
            if cond():
                return True
        except Exception:
            pass
        time.sleep(0.05)
    print(f"   !!  timed out waiting for {what}", file=sys.stderr)
    return False


def seed(node):
    for path, payload in (
        (
            "/providers",
            {
                "id": "p-load",
                "key": "p-load",
                "name": "Load",
                "endpoint": f"http://127.0.0.1:{UPSTREAM_PORT}",
                "weight": 1,
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/provider-models",
            {
                "id": "pm-load",
                "key": MODEL,
                "name": "Echo",
                "provider_id": "p-load",
                "status": 1,
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/provider-keys",
            {
                "id": "pk-load",
                "provider_id": "p-load",
                "api_key": "sk-upstream",
                "created_at": "",
            },
        ),
        (
            "/tenants",
            {
                "id": "t-load",
                "name": "Load",
                "domain": TENANT_DOMAIN,
                "cert_key": None,
                "cert_file": None,
                "enabled": True,
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/tenant-providers",
            {
                "id": "tp-load",
                "tenant_id": "t-load",
                "provider_id": "p-load",
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/tenant-models",
            {
                "id": "tm-load",
                "tenant_id": "t-load",
                "model_key": MODEL,
                "created_at": "",
                "updated_at": "",
            },
        ),
    ):
        st, _, out = node.admin("POST", path, payload, timeout=25)
        if st not in (200, 201):
            raise RuntimeError(f"seeding {path} -> HTTP {st} {out[:200]}")


def run_load(node, seconds, rps):
    """Drive a fixed request rate at `node`'s DATA port in the background.

    A fixed-rate scheduler (not a pool): the row is "20 rps", so the RATE is held constant and a
    slow node shows up as a lower issued count rather than a hidden failure.
    """
    results = []
    stop = threading.Event()

    def loop():
        interval = 1.0 / rps
        started = time.monotonic()
        deadline = started + seconds
        next_at = started
        while not stop.is_set() and time.monotonic() < deadline:
            now = time.monotonic()
            if now < next_at:
                time.sleep(min(next_at - now, 0.05))
                continue
            t0 = time.monotonic()
            status, _ids = node.catalog()
            results.append((t0, status, time.monotonic() - t0))
            next_at += interval
            if next_at < time.monotonic():  # catch up rather than burst
                next_at = time.monotonic()

    thread = threading.Thread(target=loop, daemon=True)
    started = time.monotonic()
    thread.start()
    return thread, stop, results, started


def main():
    if not os.path.exists(BIN):
        print(f"[load] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    try:
        redis_url = require_redis_url()
        host, port = redis_endpoint(redis_url)
        note = flush_db(REDIS_DB, redis_url)
    except RedisSetupError as e:
        print(f"[load] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2
    try:
        test_db = isolated("data_plane_failover")
    except SetupError as e:
        print(f"[load] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    global UPSTREAM_PORT
    UPSTREAM_PORT = free_port()
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM_PORT), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    node_redis = db_url(REDIS_DB, redis_url)

    nodes = [
        Node(f"load-{label}", test_db.url, node_redis, os.path.join(DIR, f"{label}.log"))
        for label in ("a", "b", "c")
    ]
    A, B, C = nodes
    print(f"== database: {test_db}")
    print(f"== redis: {note}")
    print(f"== upstream: http://127.0.0.1:{UPSTREAM_PORT}; binary={BIN}")
    print(f"== load: {LOAD_RPS:.0f} rps for {LOAD_SECONDS:.0f}s; kill B at t≈{KILL_AT_S:.0f}s")

    try:
        A.start()
        if not wait_for(A.healthy, 60, "node A health"):
            print(f"--- A log ---\n{A.log_tail()}", file=sys.stderr)
            return 2
        B.start()
        C.start()
        if not wait_for(lambda: all(n.healthy() for n in nodes), 60, "all three admin APIs"):
            for n in nodes:
                print(f"--- {n.name} ---\n{n.log_tail()}", file=sys.stderr)
            print("[load] CANNOT VERIFY: the cluster did not come up", file=sys.stderr)
            return 2
        seed(A)
        if not wait_for(
            lambda: all(n.catalog() == (200, [MODEL]) for n in nodes),
            15,
            "every node serving the seeded model",
        ):
            for n in nodes:
                print(f"--- {n.name}: {n.catalog()} ---", file=sys.stderr)
            print("[load] CANNOT VERIFY: the seeded config is not served everywhere", file=sys.stderr)
            return 2
        announce("fixture seeded and served on all three")

        loaded, victim = A, B
        status, ids = loaded.catalog()
        check(
            "A: the loaded node serves a REAL data-plane request before the run",
            status == 200 and ids == [MODEL],
            f"HTTP {status} ids={ids}",
        )

        thread, stop, results, started = run_load(loaded, LOAD_SECONDS, LOAD_RPS)
        planned = int(LOAD_SECONDS * LOAD_RPS)
        killed_at = None
        while thread.is_alive():
            if killed_at is None and time.monotonic() - started >= KILL_AT_S:
                announce("kill -9", f"{victim.name} at t≈{time.monotonic() - started:.1f}s")
                killed_at = time.monotonic()
                victim.kill()
            time.sleep(0.05)
        thread.join(timeout=10)
        stop.set()

        if killed_at is None:
            print("[load] CANNOT VERIFY: the run finished before the kill was issued", file=sys.stderr)
            return 2

        issued = len(results)
        bad = [r for r in results if r[1] != 200]
        durations = sorted(d for _t, _s, d in results)
        p99 = durations[min(len(durations) - 1, int(len(durations) * 0.99))] if durations else 0.0
        lat = (
            f"p50={durations[len(durations) // 2] * 1000:.0f}ms "
            f"p99={p99 * 1000:.0f}ms max={durations[-1] * 1000:.0f}ms"
            if durations
            else "no samples"
        )
        check(
            f"B: {LOAD_RPS:.0f} rps for {LOAD_SECONDS:.0f}s was really driven "
            f"(issued {issued} of ~{planned})",
            issued >= planned * 0.9,
            f"issued={issued} planned={planned} {lat}",
        )
        check(
            "C: ZERO non-200 responses for the whole run — the data plane did not notice the kill",
            not bad,
            (
                f"{len(bad)} failure(s), first: t+{bad[0][0] - killed_at:.1f}s HTTP {bad[0][1]}"
                if bad
                else f"{issued}/{issued} answered 200"
            ),
        )
        near = [r for r in results if abs(r[0] - killed_at) <= NEAR_KILL_S]
        check(
            f"D: the {len(near)} request(s) within ±{NEAR_KILL_S:.0f}s of the kill are all 200",
            bool(near) and all(r[1] == 200 for r in near),
            f"statuses={sorted({r[1] for r in near})} window={len(near)} request(s)",
        )

        survivors = [n for n in nodes if n is not victim]
        check(
            "E: the victim really died, and BOTH surviving nodes still serve the data plane",
            not victim.alive() and all(n.catalog() == (200, [MODEL]) for n in survivors),
            f"victim-alive={victim.alive()} survivors={[(n.name, n.catalog()) for n in survivors]}",
        )
    finally:
        for n in nodes:
            n.kill()
        try:
            upstream.shutdown()
        except Exception:
            pass
        test_db.cleanup()
        shutil.rmtree(DIR, ignore_errors=True)

    print()
    if failures:
        print(f"DATA PLANE UNDER NODE KILL: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print(
        "DATA PLANE UNDER NODE KILL: PASSED (20 rps across a SIGKILL of one node on a shared "
        "DB + Redis cluster; zero failed requests, both survivors still serving)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
