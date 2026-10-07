#!/usr/bin/env python3
"""Arachne control plane — the REAL multi-process acceptance drill (ADR-0001 gates 1/3/4/5).

Why this file exists at all: the plan's verification gates named it
(`integration/test_arachne_control_plane.py`) as the harness for acceptance 1/3/4/5, and it was
never written — so "acceptance 1–5 have passed", the documented PRECONDITION for the irreversible
T4.0/T4.1 deletions, had no executable evidence behind it. This is that evidence.

What it runs: three REAL `hydra` processes, each a raft member (`HYDRA_CLUSTER_PEERS`), each with
its own database and Arachne data directory, plus a real Redis for the data plane. Nothing is
in-process and nothing is mocked except the upstream LLM.

Gates:

  1  SIGKILL the current leader → a new leader within 3 s; the killing node's
     `/healthz/leader` is gone with it, the survivor's answers 200; a management write aimed at
     ANY node succeeds (the entry node applies it and publishes; the library forwards the commit)
  3  at no instant do two nodes answer 200 on `/healthz/leader` (checked continuously across the
     failover, not just before and after)
  4  after a write every node serves the SAME config — the content-level half of "all nodes
     materialize the same content hash". The hash-level half is asserted in-process against three
     real raft nodes by `crates/hydra-server/tests/arachne_three_nodes.rs`
     (`materialized() == head` on all three), which is where a per-node head hash is observable.
  5  with a MINORITY alive (2 of 3 killed) there is no quorum: a management write is refused with
     503 rather than hanging or being silently accepted, while the surviving node keeps serving
     from its own materialized state

Run:  python3 integration/test_arachne_control_plane.py
Env:  HYDRA_BIN (default target/debug/hydra), HYDRA_TEST_REDIS_URL (default redis://127.0.0.1:6380)
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
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
DIR = os.path.join(ROOT, ".acceptance", "arachne-drill")

ADMIN_TOKEN = "hydra-acceptance-admin-2026"
ENCRYPTION_KEY = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
FAILOVER_BUDGET_S = 3.0

UPSTREAM_PORT = None  # chosen in main(); the mock serves both /auth and the chat completion

failures = []


class MockUpstream(BaseHTTPRequestHandler):
    """One process answering BOTH the tenant's `auth_url` and the upstream chat completion.

    Gate 5 asserts the data plane still serves while the control plane has lost its quorum, and
    that cannot be shown by an open port: it needs a request that actually goes through the gate,
    the router and an upstream. This is that upstream (and the auth endpoint the gate calls).
    """

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        if self.path.startswith("/auth"):
            body = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            body = json.dumps(
                {
                    "id": "chatcmpl-ok",
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


def http(port, method, path, token=None, body=None, timeout=10, host="accept.local"):
    """One request to an admin or data-plane port. Returns (status, parsed_body_or_text)."""
    url = f"http://127.0.0.1:{port}{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    if body is not None:
        req.add_header("Content-Type", "application/json")
    req.add_header("Host", host)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
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
    except Exception as e:  # timeout / connection refused / reset
        return 0, str(e)


class Node:
    def __init__(self, name, peers_spec, data_dir):
        self.name = name
        self.admin_port = free_port()
        self.data_port = free_port()
        self.raft_port = free_port()
        self.db = os.path.join(data_dir, f"{name}.db")
        self.env = dict(os.environ)
        self.env.update(usage_env())
        self.env.update(
            {
                "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN,
                "HYDRA_ENCRYPTION_KEY": ENCRYPTION_KEY,
                "HYDRA_NODE_ID": name,
                "HYDRA_CLUSTER_PEERS": peers_spec,
                "HYDRA_ARACHNE_LISTEN": f"127.0.0.1:{self.raft_port}",
                "HYDRA_ADMIN_ADDR": f"127.0.0.1:{self.admin_port}",
                "HYDRA_LISTEN": f"127.0.0.1:{self.data_port}",
                "HYDRA_REDIS_URL": os.environ.get(
                    "HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380"
                ).rstrip("/")
                + "/60",
                "HYDRA_REDIS_MODE": "single",
                "HYDRA_USAGE_SINK": "clickhouse",
                "HYDRA_CLICKHOUSE_URL": "http://127.0.0.1:18898",
                "HYDRA_DB_URL": f"sqlite://{self.db}?mode=rwc",
                "HYDRA_ARACHNE_DATA_DIR": os.path.join(data_dir, f"raft-{name}"),
                "RUST_LOG": "warn",
            }
        )
        # The retired world's variables must NOT be needed: a node that still required them would
        # start in standalone mode and this drill would fail at the first write.
        for gone in (
            "HYDRA_ROLE",
            "HYDRA_CONTROL_URL",
            "HYDRA_PUBLIC_URL",
            "HYDRA_LEADER_LEASE_MS",
            "HYDRA_CONTROL_POLL_MS",
        ):
            self.env.pop(gone, None)
        self.log_path = os.path.join(data_dir, f"{name}.log")
        self.proc = None

    def start(self):
        log = open(self.log_path, "w")
        self.proc = subprocess.Popen(
            [BIN], env=self.env, stdout=log, stderr=subprocess.STDOUT
        )
        return self

    def kill(self):
        """SIGKILL — no graceful shutdown, exactly what acceptance 1 asks for."""
        if self.proc and self.proc.poll() is None:
            os.kill(self.proc.pid, signal.SIGKILL)
            self.proc.wait(timeout=10)

    def leader(self):
        status, _ = http(self.admin_port, "GET", "/healthz/leader", timeout=3)
        return status

    def log_tail(self, n=800):
        try:
            with open(self.log_path, errors="replace") as f:
                return f.read()[-n:]
        except OSError:
            return "<no log>"


def node_env_probe(node, path="/api/v1/health"):
    return http(node.admin_port, "GET", path, token=ADMIN_TOKEN)


def seed_proxy_fixture(node):
    """Provider + model + tenant + grants, so ONE real request can be proxied through a node.

    Written through the ADMIN api of whichever node is being tested, which also exercises the
    write path (the node applies it and publishes).
    """
    upstream = f"http://127.0.0.1:{UPSTREAM_PORT}"
    for path, payload in (
        ("/providers", {"id": "p-up", "key": "p-up", "name": "Up",
                        "endpoint": upstream, "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm-echo", "key": "echo", "name": "Echo",
                              "provider_id": "p-up", "status": 1,
                              "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk-up", "provider_id": "p-up", "api_key": "sk-up",
                            "created_at": ""}),
        ("/tenants", {"id": "t-drill", "name": "Drill", "domain": "drill.local",
                      "auth_url": f"{upstream}/auth", "enabled": True,
                      "cert_key": None, "cert_file": None,
                      "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp-drill", "tenant_id": "t-drill", "provider_id": "p-up",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm-drill", "tenant_id": "t-drill", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
    ):
        status, body = http(node.admin_port, "POST", f"/api/v1{path}", token=ADMIN_TOKEN, body=payload)
        if status not in (200, 201):
            raise RuntimeError(f"seeding {path} -> HTTP {status} {body}")


def proxied(node):
    """One real request through the data plane: gate -> router -> upstream."""
    return http(
        node.data_port,
        "POST",
        "/v1/chat/completions",
        token="sk-client",
        body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]},
        timeout=20,
        host="drill.local",
    )


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
        print(f"[arachne-acceptance] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    # The peer table is static and ordered: raft ids are the 1-based POSITION in it, so the ports
    # must be known before the nodes start (which is why they are allocated up front).
    nodes = [Node(f"n{i + 1}", "", DIR) for i in range(3)]
    spec = ",".join(f"{n.name}=127.0.0.1:{n.raft_port}" for n in nodes)
    for n in nodes:
        n.env["HYDRA_CLUSTER_PEERS"] = spec

    global UPSTREAM_PORT
    UPSTREAM_PORT = free_port()
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM_PORT), MockUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    print(f"== mock upstream on :{UPSTREAM_PORT} (auth + chat completion)")

    print(f"== 3 nodes: {spec}")
    print(f"== binary: {BIN}")
    for n in nodes:
        n.start()

    try:
        # ---------------------------------------------------------------- startup
        # EVERY node must be serving before the first write. Waiting only for "one leader" is not
        # enough: that can be true while the other two are still binding their listeners, and the
        # writes aimed at them then fail with "connection refused" — measured exactly that way.
        if not wait_for(
            lambda: all(
                http(n.admin_port, "GET", "/api/v1/health", token=ADMIN_TOKEN, timeout=3)[0] == 200
                for n in nodes
            ),
            90,
            "all three admin APIs healthy",
        ):
            for n in nodes:
                print(f"--- {n.name} log ---\n{n.log_tail(1200)}")
            return 2
        logs = "\n".join(n.log_tail(4000) for n in nodes)
        # A binary built without the features this drill needs cannot start a raft member, and the
        # resulting failure is a bare "no leader" unless it is named. Reported as CANNOT VERIFY
        # (exit 2) rather than as a failure of the control plane — the distinction matters, and
        # this trap is easy to fall into locally: `cargo test` rebuilds the binary WITHOUT the
        # feature set, so the drill's target is whatever was built last.
        # ONLY on the explicit message. An earlier version also fired when the log tail happened
        # not to mention HYDRA_CLUSTER_PEERS, which is true of a perfectly healthy start (the node
        # logs warnings, not its configuration) — a guard that fires on success is worse than none.
        if "requires the 'cluster-redis' cargo feature" in logs:
            print(
                "[arachne-acceptance] CANNOT VERIFY: the binary was not built with the features "
                "this drill needs.\n"
                "  cargo build -p hydra-server --features "
                "server,cluster-redis,arachne,usage-clickhouse --bin hydra",
                file=sys.stderr,
            )
            return 2
        if not wait_for(
            lambda: sum(1 for n in nodes if n.leader() == 200) == 1, 60, "exactly one leader"
        ):
            for n in nodes:
                print(f"--- {n.name} log ---\n{n.log_tail(1200)}")
            return 2
        leader = next(n for n in nodes if n.leader() == 200)
        announce("elected", f"{leader.name} (raft :{leader.raft_port}, admin :{leader.admin_port})")

        # ---------------------------------------------------------------- gate 3 (pre-write)
        check(
            "gate 3: exactly one node answers 200 on /healthz/leader at rest",
            sum(1 for n in nodes if n.leader() == 200) == 1,
        )

        # ---------------------------------------------------------------- gate 1 (write anywhere)
        for i, n in enumerate(nodes):
            status, body = http(
                n.admin_port,
                "POST",
                "/api/v1/providers",
                token=ADMIN_TOKEN,
                body={
                    "id": f"p{i + 1}",
                    "key": f"key{i + 1}",
                    "name": f"provider-{i + 1}",
                    "endpoint": "https://api.example.com",
                    "weight": 1,
                    "created_at": "",
                    "updated_at": "",
                },
                timeout=15,
            )
            check(
                f"gate 1: a management write aimed at {n.name} succeeds (entry node applies + publishes)",
                status in (200, 201),
                f"HTTP {status} {body if status not in (200, 201) else ''}",
            )

        # ---------------------------------------------------------------- gate 4 (convergence)
        def served_provider_ids():
            out = []
            for n in nodes:
                status, body = http(n.admin_port, "GET", "/api/v1/providers", token=ADMIN_TOKEN)
                if status != 200:
                    return None
                out.append(sorted(p["id"] for p in body))
            return out

        # Every node must end up serving the same set. The head is last-writer-wins across the
        # three publishes above, so the CONTENT is what is asserted — and it must be identical
        # everywhere, whichever publish won.
        merged_ok = wait_for(
            lambda: (lambda s: s is not None and all(x == s[0] for x in s) and s[0])(
                served_provider_ids()
            ),
            20,
            "all three nodes serving the same provider set",
        )
        sets = served_provider_ids()
        check(
            "gate 4: after the writes every node serves the SAME config",
            merged_ok,
            f"per-node id sets: {sets}",
        )

        # A proxy fixture, written THROUGH the admin API of one node (so it also exercises the
        # write path): gate 5 needs a request that can actually be served.
        seed_proxy_fixture(nodes[0])
        announce("proxy fixture seeded through", nodes[0].name)

        # ---------------------------------------------------------------- gate 1 + 3 (failover)
        announce("kill -9 the leader", leader.name)
        leader.kill()
        t0 = time.time()

        def a_new_leader():
            live = [n for n in nodes if n.proc.poll() is None]
            return sum(1 for n in live if n.leader() == 200) == 1

        # While the election runs, no instant may show TWO leaders. Poll as fast as we can from a
        # thread-free loop: the check is cheap (3 HTTP GETs with a 3 s timeout) and the window is
        # the thing under test.
        double_leader = False
        deadline = time.time() + 10
        elected_at = None
        while time.time() < deadline:
            live = [n for n in nodes if n.proc.poll() is None]
            leaders = sum(1 for n in live if n.leader() == 200)
            if leaders > 1:
                double_leader = True
            if leaders == 1 and elected_at is None:
                elected_at = time.time()
                break
            time.sleep(0.02)

        elapsed = (elected_at - t0) if elected_at else None
        check(
            f"gate 1: a new leader was elected within {FAILOVER_BUDGET_S:.0f}s",
            elapsed is not None and elapsed <= FAILOVER_BUDGET_S,
            f"{elapsed:.2f}s" if elapsed else "never",
        )
        check("gate 3: never two nodes claiming leadership during the failover", not double_leader)

        # A write after the failover, aimed at a SURVIVOR that is not the new leader (so the commit
        # path really is exercised from a non-leader).
        live = [n for n in nodes if n.proc.poll() is None]
        new_leader = next((n for n in live if n.leader() == 200), None)
        follower = next((n for n in live if n is not new_leader), None)
        if follower is not None:
            status, body = http(
                follower.admin_port,
                "POST",
                "/api/v1/providers",
                token=ADMIN_TOKEN,
                body={
                    "id": "p-after-failover",
                    "key": "key-after",
                    "name": "after-failover",
                    "endpoint": "https://api.example.com",
                    "weight": 1,
                    "created_at": "",
                    "updated_at": "",
                },
                timeout=15,
            )
            check(
                f"gate 1: a write on the non-leader survivor {follower.name} succeeds",
                status in (200, 201),
                f"HTTP {status}",
            )

        # ---------------------------------------------------------------- gate 5 (minority)
        victim = next(n for n in live if n is not follower)
        announce("kill a second node (minority left)", victim.name)
        victim.kill()
        survivor = follower

        # The write must be REFUSED, promptly: no quorum means the config cannot be published, and
        # the node must not pretend otherwise. A hang would be worse than either answer.
        t0 = time.time()
        status, body = http(
            survivor.admin_port,
            "POST",
            "/api/v1/providers",
            token=ADMIN_TOKEN,
            body={
                "id": "p-minority",
                "key": "key-minority",
                "name": "minority",
                "endpoint": "https://api.example.com",
                "weight": 1,
                "created_at": "",
                "updated_at": "",
            },
            timeout=20,
        )
        took = time.time() - t0
        check(
            "gate 5: with a minority alive a management write is 503 (fail-closed, no hang)",
            status == 503,
            f"HTTP {status} in {took:.1f}s {body if status != 503 else ''}",
        )
        check(
            "gate 5: the refusal names the publish, not a generic failure",
            status == 503
            and isinstance(body, dict)
            and body.get("error", {}).get("code") == "config_not_published",
            f"code={body.get('error', {}).get('code') if isinstance(body, dict) else body}",
        )
        check(
            "gate 5: /healthz/leader on the survivor is 503 (it cannot commit)",
            survivor.leader() == 503,
            f"HTTP {survivor.leader()}",
        )

        # ...and the DATA plane is unaffected: the survivor still answers from its own materialized
        # state (its own database), which is the whole point of materializing locally.
        status, body = http(survivor.admin_port, "GET", "/api/v1/providers", token=ADMIN_TOKEN)
        check(
            "gate 5: the survivor still serves its materialized config",
            status == 200 and isinstance(body, list) and len(body) > 0,
            f"HTTP {status}, {len(body) if isinstance(body, list) else '?'} providers",
        )
        # ...and the DATA plane is unaffected — asserted with a REAL request through the gate, the
        # router and an upstream, because "the port is open" proves nothing about serving. (The
        # first version of this drill asked the data port for `/healthz`, which is the ADMIN
        # route: a 404 that says only "wrong path", not "the data plane is down".)
        status, body = proxied(survivor)
        check(
            "gate 5: a real request still flows through the data plane on the survivor",
            status == 200,
            f"HTTP {status} {body if status != 200 else ''}",
        )
        if status == 403 and isinstance(body, dict) and \
                body.get("error", {}).get("message") == "tenant_forbidden":
            # `tenant_forbidden` is `router.rs` and has exactly one source: the node's config has no
            # `tenant_providers` entry for the tenant it just resolved. This assertion has failed
            # intermittently on CI runners (measured 2026-10-07: green on most runs, red on a
            # re-run of the same commit) and NOTHING in this drill printed why — while the admin leg
            # directly above it answers 200 with providers, i.e. the node's DATABASE has the fixture.
            # So the two candidate mechanisms are "the published tree never carried the row" and "the
            # database has it but the runtime snapshot does not", and the observations that separate
            # them are printed here: what the survivor's admin API serves per entity kind (its DB),
            # what each node's publish/materialize counters say, and any publish failure in its log.
            print("[arachne] gate 5 got tenant_forbidden — dumping the survivor's state",
                  file=sys.stderr)
            for path in ("providers", "provider-models", "provider-keys", "tenants",
                         "tenant-providers", "tenant-models"):
                st, out = http(survivor.admin_port, "GET", f"/api/v1{path}", token=ADMIN_TOKEN)
                print(f"--- survivor GET /{path} -> HTTP {st} :: {str(out)[:220]}", file=sys.stderr)
            for n in nodes:
                for name in ("hydra_arachne_publish_total",
                             "hydra_arachne_quorum_unavailable_total",
                             "hydra_replica_materialize_retries_total"):
                    st, out = http(n.admin_port, "GET", "/metrics", token=ADMIN_TOKEN)
                    series = [l for l in (out or "").splitlines() if l.startswith(name)]
                    print(f"--- {n.name} {name}: {series or 'NO SERIES (or node down)'}",
                          file=sys.stderr)
            for n in nodes:
                if not os.path.exists(n.log_path):
                    continue
                text = open(n.log_path, errors="replace").read()
                lines = [l for l in text.splitlines() if "PUBLISH FAILED" in l or "not_leader" in l]
                print(f"--- {n.name}.log: publish failures ({len(lines)})", file=sys.stderr)
                for line in lines[:3]:
                    print(f"    {line[:400]}", file=sys.stderr)
                print(f"--- {n.name}.log (last 1200) ---\n{text[-1200:]}", file=sys.stderr)
    finally:
        for n in nodes:
            n.kill()

    print()
    if failures:
        print(f"ARACHNE ACCEPTANCE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print(
        "ARACHNE ACCEPTANCE: PASSED (gates 1, 3, 4, 5 over three real hydra processes: "
        "write-anywhere, failover <=3s, no double leader, one converged config, "
        "minority => 503 fail-closed with the data plane still serving)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
