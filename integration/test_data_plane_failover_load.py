#!/usr/bin/env python3
"""Acceptance 2 — the DATA PLANE under load, across a raft failover (ADR-0001).

Why this file exists: the plan's acceptance list has five rows, and row 2 ("20 rps for 60 s across
the failover") was the only one never executed. Its driver, `test_cluster_ha.py`, was retired
because it drove the leader/edge topology (`HYDRA_ROLE=leader|edge` + `HYDRA_CONTROL_URL`), so it
could only report CANNOT VERIFY. The measurement it was meant to reproduce is round 73's: 20 rps
against a NON-leader's data port across a hard failover, 355/355 probes answered 200.

**The claim under test is ADR-0001 D-1/D-2, and it is not a formality.** The control plane moved to
Arachne and the data plane deliberately did not: every node serves from its OWN materialized config,
so killing the raft leader must be invisible to a request that is not a management write. That
"should be trivially true" is exactly why it needs a measurement — the retired topology also claimed
it, and the claim was true there for an entirely different reason (a stateless edge with no local
database), so the old evidence does not transfer.

What it runs: three REAL `hydra` processes (raft members, own DB, own Arachne directory, real Redis),
a real mock upstream, and a client that keeps 20 requests/second going for 60 seconds against a
FOLLOWER's data port while the leader is `kill -9`'d 15 seconds in.

Gates:
  A  the loaded node serves a REAL request before the run (gate -> router -> upstream), so "it kept
     serving" is a statement about a path that worked, not about a port that stayed open
  B  20 rps for the full window, ~1200 requests issued
  C  **ZERO non-200 responses** for the whole run — this is the acceptance criterion
  D  the requests in the ±3 s AROUND the kill are all 200 (the window the row is about; a mean over
     60 s would hide a 2-second outage)
  E  the kill really changed the writer: the killed node WAS the leader at kill time, and a
     different node leads afterwards
  F  the node that was loaded is NOT the node that was killed (otherwise "it kept serving" says
     nothing) AND it was a FOLLOWER when the load started (answering 503 on `/healthz/leader`), so
     the claim is about a node that was not the writer. Whether it later WINS the election is not
     part of the claim — and it happened in the first run of this drill, which is why the assertion
     is on the state at selection time and not on the state afterwards
  G  after the failover the NEW leader's data port serves too — the node that just became the
     writer must not stop proxying

Run:  python3 integration/test_data_plane_failover_load.py
Env:  HYDRA_BIN (default target/debug/hydra), HYDRA_TEST_REDIS_URL (default redis://127.0.0.1:6380),
      HYDRA_LOAD_SECONDS (default 60), HYDRA_LOAD_RPS (default 20)
Exit: 0 pass · 1 an assertion failed · 2 could not verify (no binary / no cluster / no load)
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

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
DIR = os.path.join(ROOT, ".acceptance", "data-plane-failover")

ADMIN_TOKEN = "hydra-load-admin-2026"
ENCRYPTION_KEY = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
# The acceptance row's numbers. Overridable so a developer can iterate in seconds; CI runs the real
# window, and the assertions are on RATIOS (`issued >= 0.9 * planned`) so a shorter run is still a
# real measurement rather than a weaker assertion.
LOAD_SECONDS = float(os.environ.get("HYDRA_LOAD_SECONDS", "60"))
LOAD_RPS = float(os.environ.get("HYDRA_LOAD_RPS", "20"))
KILL_AT_S = float(os.environ.get("HYDRA_LOAD_KILL_AT", "15"))
NEAR_KILL_S = 3.0

UPSTREAM_PORT = None

failures = []


class MockUpstream(BaseHTTPRequestHandler):
    """Answers the tenant's `auth_url` and the upstream chat completion, so a proxied request is real."""

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


def http(port, method, path, token=None, body=None, timeout=10, host="load.local"):
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
    except Exception as e:  # timeout / refused / reset
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
                "HYDRA_ENCRYPTION_KEY": ENCRYPTION_KEY,
                "HYDRA_NODE_ID": name,
                "HYDRA_ARACHNE_LISTEN": f"127.0.0.1:{self.raft_port}",
                "HYDRA_ADMIN_ADDR": f"127.0.0.1:{self.admin_port}",
                "HYDRA_LISTEN": f"127.0.0.1:{self.data_port}",
                "HYDRA_REDIS_URL": os.environ.get(
                    "HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380"
                ).rstrip("/")
                + "/61",
                "HYDRA_REDIS_MODE": "single",
                # ClickHouse, pointing at a DEAD port: cluster mode REFUSES a per-node SQLite sink
                # ("per-node usage records are meaningless across a cluster"), and nothing here
                # asserts usage — the sink is a dependency to satisfy, not a subject.
                "HYDRA_USAGE_SINK": "clickhouse",
                "HYDRA_CLICKHOUSE_URL": "http://127.0.0.1:18897",
                "HYDRA_DB_URL": f"sqlite://{self.db}?mode=rwc",
                "HYDRA_ARACHNE_DATA_DIR": os.path.join(data_dir, f"raft-{name}"),
                "RUST_LOG": "warn",
            }
        )
        # The retired world's variables must not be needed (a node that still required them would
        # start standalone and the first write would fail).
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
        self.proc = subprocess.Popen([BIN], env=self.env, stdout=log, stderr=subprocess.STDOUT)
        return self

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def kill(self):
        """SIGKILL — the acceptance row says failover, not graceful handover."""
        if self.alive():
            os.kill(self.proc.pid, signal.SIGKILL)
            self.proc.wait(timeout=10)

    def leads(self):
        return http(self.admin_port, "GET", "/healthz/leader", timeout=3)[0] == 200

    def serving(self):
        return http(self.admin_port, "GET", "/api/v1/health", token=ADMIN_TOKEN, timeout=3)[0] == 200

    def log_tail(self, n=800):
        try:
            with open(self.log_path, errors="replace") as f:
                return f.read()[-n:]
        except OSError:
            return "<no log>"


def seed_proxy_fixture(node):
    upstream = f"http://127.0.0.1:{UPSTREAM_PORT}"
    for path, payload in (
        ("/providers", {"id": "p-load", "key": "p-load", "name": "Load",
                        "endpoint": upstream, "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm-echo", "key": "echo", "name": "Echo",
                              "provider_id": "p-load", "status": 1,
                              "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk-load", "provider_id": "p-load", "api_key": "sk-up",
                            "created_at": ""}),
        ("/tenants", {"id": "t-load", "name": "Load", "domain": "load.local",
                      "auth_url": f"{upstream}/auth", "enabled": True,
                      "cert_key": None, "cert_file": None,
                      "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp-load", "tenant_id": "t-load", "provider_id": "p-load",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm-load", "tenant_id": "t-load", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
    ):
        status, body = http(node.admin_port, "POST", f"/api/v1{path}", token=ADMIN_TOKEN, body=payload)
        if status not in (200, 201):
            raise RuntimeError(f"seeding {path} -> HTTP {status} {body}")


def proxied(node, timeout=20):
    """One real request through the data plane: gate -> router -> upstream."""
    return http(
        node.data_port,
        "POST",
        "/v1/chat/completions",
        token="sk-client",
        body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]},
        timeout=timeout,
        host="load.local",
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


def run_load(node, seconds, rps):
    """Drive `rps` requests/second at `node`'s DATA port for `seconds`, in the background.

    A fixed-rate scheduler rather than a thread pool: the row is "20 rps for 60 s", so the thing to
    hold constant is the RATE, and a pool would instead push as hard as it can (which measures
    throughput, not the survival of a request stream). Each request is issued sequentially — at
    20 rps that is ~50 ms of headroom per request, and a request that takes longer than its slot
    delays the next one rather than being dropped, which is the honest behaviour: a slow node then
    shows up as a lower issued COUNT instead of a hidden failure.
    """
    results = []
    stop = threading.Event()

    def loop():
        interval = 1.0 / rps
        started = time.monotonic()
        deadline = started + seconds
        next_at = started
        while not stop.is_set():
            now = time.monotonic()
            if now >= deadline:
                break
            if now < next_at:
                time.sleep(min(next_at - now, 0.05))
                continue
            t0 = time.monotonic()
            status, _body = proxied(node)
            results.append((t0, status, time.monotonic() - t0))
            next_at += interval
            # Catch up rather than burst: if a request overran its slot, do not issue the missed
            # ones back to back (that would turn a stall into a spike and hide it).
            if next_at < time.monotonic():
                next_at = time.monotonic()

    thread = threading.Thread(target=loop, daemon=True)
    started = time.monotonic()
    thread.start()
    # `started` travels back so the KILL can be scheduled against the same clock the loader uses:
    # deriving it from the first sample meant that a loader whose first request timed out never
    # looked "due" for the kill, and the leg then reported CANNOT VERIFY instead of a verdict.
    return thread, stop, results, started


def main():
    if not os.path.exists(BIN):
        print(f"[load] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    global UPSTREAM_PORT
    UPSTREAM_PORT = free_port()
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM_PORT), MockUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    print(f"== mock upstream on :{UPSTREAM_PORT}")

    nodes = [Node(f"n{i + 1}", DIR) for i in range(3)]
    spec = ",".join(f"{n.name}=127.0.0.1:{n.raft_port}" for n in nodes)
    for n in nodes:
        n.env["HYDRA_CLUSTER_PEERS"] = spec
    print(f"== 3 nodes: {spec}")
    print(f"== binary: {BIN}")
    print(f"== load: {LOAD_RPS:.0f} rps for {LOAD_SECONDS:.0f}s, leader killed at t={KILL_AT_S:.0f}s")
    for n in nodes:
        n.start()

    try:
        # ------------------------------------------------------------------ startup
        if not wait_for(lambda: all(n.serving() for n in nodes), 30, "all three admin APIs"):
            for n in nodes:
                print(f"--- {n.name} ---", file=sys.stderr)
                print(n.log_tail(), file=sys.stderr)
            print("[load] CANNOT VERIFY: the cluster did not come up", file=sys.stderr)
            return 2
        if not wait_for(lambda: sum(1 for n in nodes if n.leads()) == 1, 20, "exactly one leader"):
            print("[load] CANNOT VERIFY: no single leader — the failover leg cannot start",
                  file=sys.stderr)
            return 2
        leader = next(n for n in nodes if n.leads())
        announce("cluster up", f"leader = {leader.name}")

        # ------------------------------------------------------------------ seed
        seed_proxy_fixture(leader)
        # Every node must be serving the seeded config before the load starts, otherwise the first
        # requests would 404 and the run would measure materialization, not the data plane.
        if not wait_for(
            lambda: all(proxied(n)[0] == 200 for n in nodes), 30,
            "every node proxying the seeded config",
        ):
            for n in nodes:
                print(f"--- {n.name}: {proxied(n)[0]} ---", file=sys.stderr)
            print("[load] CANNOT VERIFY: the seeded config is not being served everywhere",
                  file=sys.stderr)
            return 2
        announce("fixture seeded and materialized on all three")

        # ------------------------------------------------------------------ gate A
        loaded = next(n for n in nodes if n is not leader)
        # Recorded BEFORE the load: `leads()` is a live question and the answer changes if the
        # loaded node wins the election the kill triggers (measured: it did).
        loaded_was_follower = not loaded.leads()
        status, body = proxied(loaded)
        check("A: the loaded node serves a REAL request before the run "
              "(gate -> router -> upstream, not merely an open port)",
              status == 200, f"HTTP {status} {str(body)[:80]}")

        # ------------------------------------------------------------------ B/C/D: the run
        thread, stop, results, started = run_load(loaded, LOAD_SECONDS, LOAD_RPS)
        planned = int(LOAD_SECONDS * LOAD_RPS)
        killed_at = None
        # The kill happens INSIDE the window, from this thread, while the loader keeps going.
        while thread.is_alive():
            if killed_at is None and time.monotonic() - started >= KILL_AT_S:
                announce("kill -9 the leader",
                         f"{leader.name} at t≈{time.monotonic() - started:.1f}s")
                killed_at = time.monotonic()
                leader.kill()
            time.sleep(0.05)
        thread.join(timeout=10)
        stop.set()

        if killed_at is None:
            print("[load] CANNOT VERIFY: the run finished before the kill was issued",
                  file=sys.stderr)
            return 2

        issued = len(results)
        bad = [r for r in results if r[1] != 200]
        durations = sorted(d for _t, _s, d in results)
        p99 = durations[min(len(durations) - 1, int(len(durations) * 0.99))] if durations else 0.0
        lat = f"p50={durations[len(durations) // 2] * 1000:.0f}ms p99={p99 * 1000:.0f}ms max={durations[-1] * 1000:.0f}ms" if durations else "no samples"

        check(f"B: {LOAD_RPS:.0f} rps for {LOAD_SECONDS:.0f}s was really driven "
              f"(issued {issued} of ~{planned})",
              issued >= planned * 0.9, f"issued={issued} planned={planned} {lat}")
        check("C: ZERO non-200 responses for the whole run — the data plane did not notice the "
              "failover",
              not bad,
              (f"{len(bad)} failure(s), first: t+{bad[0][0] - killed_at:.1f}s HTTP {bad[0][1]}"
               if bad else f"{issued}/{issued} answered 200"))

        near = [r for r in results if abs(r[0] - killed_at) <= NEAR_KILL_S]
        check(f"D: the {len(near)} request(s) within ±{NEAR_KILL_S:.0f}s of the kill are all 200 "
              "(a 60s mean would hide a two-second outage)",
              bool(near) and all(r[1] == 200 for r in near),
              f"statuses={sorted({r[1] for r in near})} window={len(near)} request(s)")

        # ------------------------------------------------------------------ E/F
        new_leader = None
        wait_for(lambda: any(n.leads() for n in nodes if n.alive()), 10, "a new leader")
        live = [n for n in nodes if n.alive()]
        leaders = [n for n in live if n.leads()]
        new_leader = leaders[0] if len(leaders) == 1 else None
        check("E: the kill really moved the writer (the killed node WAS the leader, and a different "
              "node leads now)",
              not leader.alive() and new_leader is not None and new_leader is not leader,
              f"killed={leader.name} new_leader={new_leader.name if new_leader else '<none/split>'}")
        check("F: the node under load is not the killed node, and was a FOLLOWER when the run began",
              loaded is not leader and loaded_was_follower,
              f"loaded={loaded.name} (follower at t=0: {loaded_was_follower}) killed={leader.name} "
              f"new_leader={new_leader.name if new_leader else '?'}"
              + (" — the loaded node won the election, which is allowed and does not weaken C"
                 if new_leader is loaded else ""))

        # ------------------------------------------------------------------ G
        if new_leader is not None:
            status, body = proxied(new_leader)
            check("G: the node that just BECAME the writer still proxied a request",
                  status == 200, f"HTTP {status} {str(body)[:80]}")
        else:
            check("G: the node that just BECAME the writer still proxied a request", False,
                  "no single new leader to ask")
    finally:
        for n in nodes:
            n.kill()

    print()
    if failures:
        print(f"DATA PLANE UNDER FAILOVER: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("DATA PLANE UNDER FAILOVER: PASSED (acceptance 2 — 20 rps across a raft failover with "
          "zero failed requests)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
