#!/usr/bin/env python3
"""A tenant write that cannot be PUBLISHED must not answer 200 (ADR-0001 D-6/T3.2).

What this pins, and why it needed measuring: since D-6 (乙-full) the entry node applies a
tenant self-service write itself, and the publish step rides on the post-write `reload_all`. When the
control plane cannot commit (no majority), the SQLite transaction has ALREADY committed — so the node
serves the new config while the cluster does not have it. **Measured 2026-10-05: with 2 of 3 members
killed, `PUT /tenant/t1/api/v1/sub-tenants/s2` answered `200 {"config_version":3,…}`** and the head
was uncommittable. A tenant reads `config_version` as its reconciliation baseline (documented in
`tenant-api-integration.md`), so a `200` there is a lie the tenant cannot detect: no other node will
ever see the sub-tenant, and an LB that moves the next request elsewhere silently loses it.

The admin path has answered `503 config_not_published` for this exact failure since T3.2. The tenant
path was left behind when the write moved to the entry node; this drill is the executable form of
"the same failure gets the same answer on both paths", plus the recovery story the docs promise.

Gates:
  A  healthy quorum: a tenant write answers 200 with a `config_version` (the control case — so a
     `503` later is about the quorum, not about the request)
  B  no quorum (2 of 3 killed): the SAME kind of write answers **503 `config_not_published`**
  C  ...and the error body says the change is local-only (it must not read as "your request was
     bad"), which is what tells an integrator to retry rather than to fix the payload
  D  the survivor is still healthy and still serving its materialized config (`/api/v1/health` 200):
     a refused publish must not take the node down
  E  quorum restored: retrying the SAME request answers 200 (the write is idempotent — a sub-tenant
     is an upsert on `(tenant_id, name)`)
  F  ...and the retry is what PUBLISHES it: a different node, asked for the tenant's own sub-tenants,
     now lists it (the cluster actually converged)

Run:  python3 integration/test_tenant_write_publish_failure.py
Env:  HYDRA_BIN (default target/debug/hydra), HYDRA_TEST_REDIS_URL (default redis://127.0.0.1:6380)
Exit: 0 pass · 1 an assertion failed · 2 could not verify
"""

import base64
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

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
DIR = os.path.join(ROOT, ".acceptance", "tenant-write-publish")

ADMIN_TOKEN = "hydra-pubfail-admin-2026"
CLUSTER_TOKEN = "hydra-pubfail-cluster-2026"
TENANT_TOKEN = "hydra-pubfail-tenant-token-abcdef"
ENCRYPTION_KEY = base64.b64encode(b"P" * 32).decode()

failures = []


class MockUpstream(BaseHTTPRequestHandler):
    """The tenant's `auth_url` (the tenant API gate calls it) and the upstream chat endpoint."""

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        if self.path.startswith("/auth"):
            body = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            body = json.dumps({"id": "x", "object": "chat.completion", "choices": []}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


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


def http(port, method, path, token=None, body=None, timeout=15, host="pubfail.local"):
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        req.add_header("Content-Type", "application/json")
    req.add_header("Host", host)
    try:
        with urllib.request.urlopen(req, data=data, timeout=timeout) as r:
            raw = r.read().decode(errors="replace")
            try:
                return r.status, json.loads(raw) if raw else {}
            except json.JSONDecodeError:
                return r.status, raw
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            return e.code, raw
    except Exception as e:
        return 0, str(e)


class Node:
    def __init__(self, name, data_dir):
        self.name = name
        self.admin_port = free_port()
        self.data_port = free_port()
        self.raft_port = free_port()
        self.db = os.path.join(data_dir, f"{name}.db")
        self.env = dict(os.environ)
        self.env.update(
            {
                "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN,
                "HYDRA_CLUSTER_TOKEN": CLUSTER_TOKEN,
                "HYDRA_ENCRYPTION_KEY": ENCRYPTION_KEY,
                "HYDRA_NODE_ID": name,
                "HYDRA_CLUSTER_ID": "tenant-write-publish",
                "HYDRA_ARACHNE_LISTEN": f"127.0.0.1:{self.raft_port}",
                "HYDRA_ADMIN_ADDR": f"127.0.0.1:{self.admin_port}",
                "HYDRA_LISTEN": f"127.0.0.1:{self.data_port}",
                "HYDRA_REDIS_URL": os.environ.get(
                    "HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380"
                ).rstrip("/")
                + "/62",
                "HYDRA_REDIS_MODE": "single",
                "HYDRA_USAGE_SINK": "clickhouse",
                "HYDRA_CLICKHOUSE_URL": "http://127.0.0.1:18896",
                "HYDRA_DB_URL": f"sqlite://{self.db}?mode=rwc",
                "HYDRA_ARACHNE_DATA_DIR": os.path.join(data_dir, f"raft-{name}"),
                "RUST_LOG": "warn",
            }
        )
        # The member list is static and ordered (raft ids are positional), so the ports are allocated
        # before any member starts.
        self.port = None
        self.log_path = os.path.join(data_dir, f"{name}.log")
        self.proc = None

    def start(self):
        log = open(self.log_path, "a")
        self.proc = subprocess.Popen([BIN], env=self.env, stdout=log, stderr=subprocess.STDOUT)
        return self

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def kill(self):
        if self.alive():
            os.kill(self.proc.pid, signal.SIGKILL)
            self.proc.wait(timeout=10)

    def serving(self):
        return http(self.admin_port, "GET", "/api/v1/health", token=ADMIN_TOKEN, timeout=3)[0] == 200

    def leads(self):
        return http(self.admin_port, "GET", "/healthz/leader", timeout=3)[0] == 200

    def log_tail(self, n=700):
        try:
            with open(self.log_path, errors="replace") as f:
                return f.read()[-n:]
        except OSError:
            return "<no log>"


def seed(node, upstream):
    for path, payload in (
        ("/providers", {"id": "p-pf", "key": "p-pf", "name": "PF", "endpoint": upstream,
                        "weight": 1, "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm-pf", "key": "echo", "name": "E", "provider_id": "p-pf",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk-pf", "provider_id": "p-pf", "api_key": "sk-up",
                            "created_at": ""}),
        ("/tenants", {"id": "t1", "name": "T", "domain": "pubfail.local",
                      "auth_url": f"{upstream}/auth", "enabled": True, "cert_key": None,
                      "cert_file": None, "access_token": TENANT_TOKEN,
                      "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp-pf", "tenant_id": "t1", "provider_id": "p-pf",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm-pf", "tenant_id": "t1", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
    ):
        status, body = http(node.admin_port, "POST", f"/api/v1{path}", token=ADMIN_TOKEN, body=payload)
        if status not in (200, 201):
            raise RuntimeError(f"seeding {path} -> HTTP {status} {body}")


def tenant_write(node, name):
    """The tenant self-service write under test (an upsert on `(tenant_id, name)`)."""
    return http(
        node.data_port,
        "PUT",
        f"/tenant/t1/api/v1/sub-tenants/{name}",
        token=TENANT_TOKEN,
        body={"enabled": True},
    )


def tenant_list(node):
    return http(node.data_port, "GET", "/tenant/t1/api/v1/sub-tenants", token=TENANT_TOKEN)


def wait_for(cond, budget, what):
    deadline = time.time() + budget
    while time.time() < deadline:
        try:
            if cond():
                return True
        except Exception:
            pass
        time.sleep(0.05)
    print(f"   !!  timed out waiting for {what}")
    return False


def main():
    if not os.path.exists(BIN):
        print(f"[pubfail] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    upstream_port = free_port()
    threading.Thread(
        target=ThreadingHTTPServer(("127.0.0.1", upstream_port), MockUpstream).serve_forever,
        daemon=True,
    ).start()
    upstream = f"http://127.0.0.1:{upstream_port}"

    nodes = [Node(f"pf-{i + 1}", DIR) for i in range(3)]
    spec = ",".join(f"{n.name}=127.0.0.1:{n.raft_port}" for n in nodes)
    for n in nodes:
        n.env["HYDRA_CLUSTER_PEERS"] = spec
    print(f"== 3 nodes: {spec}")
    print(f"== binary: {BIN}")
    for n in nodes:
        n.start()

    try:
        if not wait_for(lambda: all(n.serving() for n in nodes), 30, "all three admin APIs"):
            for n in nodes:
                print(f"--- {n.name} ---", file=sys.stderr)
                print(n.log_tail(), file=sys.stderr)
            print("[pubfail] CANNOT VERIFY: the cluster did not come up", file=sys.stderr)
            return 2
        if not wait_for(lambda: sum(1 for n in nodes if n.leads()) == 1, 20, "exactly one leader"):
            print("[pubfail] CANNOT VERIFY: no single leader", file=sys.stderr)
            return 2

        seed(nodes[0], upstream)
        # Every node must serve the seeded tenant before the writes start.
        if not wait_for(lambda: all(tenant_list(n)[0] == 200 for n in nodes), 30,
                        "the tenant API on every node"):
            print("[pubfail] CANNOT VERIFY: the seeded config is not served everywhere",
                  file=sys.stderr)
            return 2
        announce("cluster up and the fixture is materialized on all three")

        writer = next(n for n in nodes if n.leads())
        entry = next(n for n in nodes if n is not writer)
        announce("write target", f"{entry.name} (a follower; the writer is {writer.name})")

        # ---------------------------------------------------------------- gate A
        status, body = tenant_write(entry, "s-ok")
        check("A: with a healthy quorum the tenant write answers 200 with a config_version",
              status == 200 and isinstance(body, dict) and "config_version" in body,
              f"HTTP {status} {str(body)[:100]}")

        # ---------------------------------------------------------------- B/C/D
        killed = [n for n in nodes if n is not entry]
        for n in killed:
            n.kill()
        announce("killed two members", f"{[n.name for n in killed]} — no majority left")
        # The node must notice it cannot commit before we write: /healthz/leader is 503 on the
        # survivor, which is the same signal the acceptance drill's gate 5 uses.
        wait_for(lambda: not entry.leads(), 10, "the survivor to lose leadership")

        status, body = tenant_write(entry, "s-nopublish")
        code = body.get("error", {}).get("code") if isinstance(body, dict) else None
        check("B: WITHOUT a quorum the same write answers 503 config_not_published (not 200)",
              status == 503 and code == "config_not_published",
              f"HTTP {status} code={code!r} {str(body)[:160]}")
        message = body.get("error", {}).get("message", "") if isinstance(body, dict) else ""
        check("C: the refusal says the change is LOCAL-ONLY (an integrator must retry, not fix the "
              "payload)",
              "could NOT be published" in message and "retry" in message.lower(),
              f"message={message[:200]!r}")
        check("D: the survivor is still healthy and still serving its materialized config",
              entry.serving() and tenant_list(entry)[0] == 200,
              f"health={entry.serving()} tenant_api={tenant_list(entry)[0]}")

        # ---------------------------------------------------------------- E/F
        for n in killed:
            n.start()
        if not wait_for(lambda: all(n.serving() for n in nodes), 30, "quorum restored"):
            print("[pubfail] CANNOT VERIFY: the cluster did not come back", file=sys.stderr)
            return 2
        if not wait_for(lambda: any(n.leads() for n in nodes), 20, "a writer again"):
            print("[pubfail] CANNOT VERIFY: no writer after restoring quorum", file=sys.stderr)
            return 2
        announce("quorum restored", "retrying the SAME request")

        status, body = tenant_write(entry, "s-nopublish")
        check("E: retrying the same request once quorum is back answers 200 (it is an upsert)",
              status == 200 and isinstance(body, dict) and "config_version" in body,
              f"HTTP {status} {str(body)[:120]}")

        # A DIFFERENT node must now list it: the retry is what publishes it.
        other = next(n for n in nodes if n is not entry)
        ok = wait_for(
            lambda: any(st.get("name") == "s-nopublish"
                        for st in (tenant_list(other)[1].get("sub_tenants") or [])),
            20,
            "the OTHER node to list the retried sub-tenant",
        )
        names = [st.get("name") for st in (tenant_list(other)[1].get("sub_tenants") or [])]
        check("F: ...and the retry is what PUBLISHES it — another node now serves it (the cluster "
              "converged)", ok, f"{other.name} sees {names}")
    finally:
        for n in nodes:
            n.kill()

    print()
    if failures:
        print(f"TENANT WRITE PUBLISH FAILURE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("TENANT WRITE PUBLISH FAILURE: PASSED (503 config_not_published without a quorum, "
          "idempotent retry republishes it)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
