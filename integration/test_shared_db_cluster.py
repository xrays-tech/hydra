#!/usr/bin/env python3
"""The shared-DB, no-raft cluster drill: THREE real hydra nodes, ONE PostgreSQL/MySQL database,
ONE Redis data plane.

This replaces the raft-era acceptance drills (`test_arachne_control_plane.py`,
`test_tenant_write_publish_failure.py`). Their subject — leader election, quorum, a control-plane
publish that could fail after the DB commit — is gone. The architecture under test now is:

  * every node reads/writes the SAME relational database; there is no leader and no quorum;
  * correctness of a write comes from DB transactions/constraints, not consensus;
  * each process caches the config locally, and after a commit it publishes a Redis pub/sub
    notification so peers reload; a poll fallback (~5 s default) covers a missed message or a
    Redis outage.

What is REAL here: three gateway processes, a real PostgreSQL/MySQL database, a real Redis,
a real mock upstream, per-run ports, and a per-run schema/database. Nothing is SQLite and
nothing is in-process. The developer's `hydra-local` compose stack is not touched: the drill
uses `HYDRA_TEST_DATABASE_URL` / `HYDRA_TEST_REDIS_URL`, a fresh schema/database and a Redis DB
index of its own, and it can cut Redis through a local relay without stopping the server.

Assertions (bounded waits everywhere; every leg prints PASS/FAIL):

  A1  all three admin `/api/v1/health` are 200 and `/healthz/leader` is 404 on every node
  A2  a tenant/provider/model seeded through A is visible to the data-plane model catalog on
      A, B and C (same shared DB, same config)
  A3  a write on B converges on A and C (`config_version` + the renamed tenant), and the
      convergence is carried by Redis pub/sub (the pubsub reload counter moves)
  A4  a write on C converges on A and B
  A5  SIGKILL B; a write on A still converges on C, C still serves `/v1/models`; B restarts and
      catches up from the shared DB (optional in the spec, asserted here because it is cheap)
  A6  with Redis blocked, a write on A still commits (200), C converges through the POLL
      fallback, and restoring Redis does not hurt anything
  A7  every node accepts a management write (no `config_not_published`, no leader)
  A8  cleanup drops the per-run schema/database and removes the temp dir

Run:
  export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
  export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
  # the binary must be built with the cluster features and the matching DB backend:
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse,db-postgres --bin hydra
  python3 integration/test_shared_db_cluster.py
  # MySQL: use HYDRA_TEST_DATABASE_URL=mysql://... and feature db-mysql
Exit 0 pass · 1 an assertion failed · 2 could not verify (missing env/binary/services).
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

from _redis_harness import (
    RedisRelay,
    RedisSetupError,
    db_url,
    endpoint as redis_endpoint,
    flush_db,
    require_url as require_redis_url,
)
from _shared_db import SetupError, isolated
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
DIR = os.path.join(ROOT, ".acceptance", "shared-db-cluster")

ADMIN_TOKEN = "hydra-shared-db-admin-2026"
ENCRYPTION_KEY = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
TENANT_ID = "t-drill"
TENANT_DOMAIN = "drill.local"
MODEL = "echo"

# Short poll so the missed-message leg finishes quickly while still proving the fallback. The
# pub/sub legs assert the pubsub counter, so a fast poll cannot make them green for the wrong reason.
# The drill sets the node's HYDRA_CONFIG_POLL_MS explicitly. 200 ms makes the missed-message
# leg quick; the pub/sub legs assert the pubsub counter, so a fast poll cannot make them green
# for the wrong reason. Override with HYDRA_CONFIG_POLL_MS when investigating convergence.
POLL_MS = int(os.environ.get("HYDRA_CONFIG_POLL_MS", "200"))
STARTUP_BUDGET = float(os.environ.get("HYDRA_SHARED_DB_STARTUP_BUDGET", "60"))
CONVERGE_BUDGET = float(os.environ.get("HYDRA_SHARED_DB_CONVERGE_BUDGET", "10"))
REDIS_DB = int(os.environ.get("HYDRA_SHARED_DB_REDIS_DB", "58"))

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
    """The provider endpoint: a chat completion the data plane could proxy if needed."""

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
                "id": "chatcmpl-shared-db",
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

    def log_message(self, format, *args):  # noqa: A002 - matches BaseHTTPRequestHandler
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
        # Drop every inherited HYDRA_*: a developer shell with a stale value would otherwise
        # silently rewrite the drill (the startup-knobs drill learned the same lesson).
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
                "HYDRA_CONFIG_POLL_MS": str(POLL_MS),
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

    def leader_probe(self):
        # The admin gate runs before routing, so an unauthenticated unknown path is 401. Send the
        # admin token so the assertion observes the ROUTE's absence (404), not the gate.
        return call(
            "GET",
            f"http://127.0.0.1:{self.admin_port}/healthz/leader",
            token=ADMIN_TOKEN,
            timeout=3,
        )[0]

    def catalog(self):
        st, _, out = call(
            "GET",
            f"http://127.0.0.1:{self.data_port}/v1/models",
            timeout=10,
            host=TENANT_DOMAIN,
        )
        try:
            ids = sorted(m["id"] for m in json.loads(out).get("data", []))
        except Exception:
            ids = None
        return st, ids, out

    def status(self):
        st, _, out = self.admin("GET", "/cluster/status", timeout=5)
        try:
            return st, json.loads(out)
        except Exception:
            return st, {}

    def version(self):
        st, body = self.status()
        return int(body.get("config_version", -1)) if st == 200 else -1

    def tenants(self):
        st, _, out = self.admin("GET", "/tenants", timeout=10)
        try:
            return st, json.loads(out)
        except Exception:
            return st, out

    def tenant_name(self, tenant_id=TENANT_ID):
        st, body = self.tenants()
        if st != 200 or not isinstance(body, list):
            return None
        for t in body:
            if t.get("id") == tenant_id:
                return t.get("name")
        return None

    def metric_values(self, name, needle=None):
        st, _, out = call(
            "GET", f"http://127.0.0.1:{self.admin_port}/metrics", token=ADMIN_TOKEN
        )
        if st != 200:
            return []
        values = []
        for line in out.splitlines():
            if not line.startswith(name) or line.startswith("#"):
                continue
            if needle and needle not in line:
                continue
            try:
                values.append(float(line.rsplit(" ", 1)[1]))
            except (ValueError, IndexError):
                pass
        return values

    def reload_source(self, source):
        return sum(self.metric_values("hydra_config_reload_total", f'source="{source}"'))

    def log_tail(self, n=1200):
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
    """Tenant + provider + model through node A's ADMIN API."""
    for path, payload in (
        (
            "/providers",
            {
                "id": "p-drill",
                "key": "p-drill",
                "name": "Drill upstream",
                "endpoint": f"http://127.0.0.1:{UPSTREAM_PORT}",
                "weight": 1,
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/provider-models",
            {
                "id": "pm-drill",
                "key": MODEL,
                "name": "Echo",
                "provider_id": "p-drill",
                "status": 1,
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/provider-keys",
            {
                "id": "pk-drill",
                "provider_id": "p-drill",
                "api_key": "sk-upstream",
                "created_at": "",
            },
        ),
        (
            "/tenants",
            {
                "id": TENANT_ID,
                "name": "Shared DB Drill",
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
                "id": "tp-drill",
                "tenant_id": TENANT_ID,
                "provider_id": "p-drill",
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/tenant-models",
            {
                "id": "tm-drill",
                "tenant_id": TENANT_ID,
                "model_key": MODEL,
                "created_at": "",
                "updated_at": "",
            },
        ),
    ):
        st, _, out = node.admin("POST", path, payload, timeout=25)
        if st not in (200, 201):
            raise RuntimeError(f"seeding {path} -> HTTP {st} {out[:200]}")


def rename_tenant(node, new_name):
    """A management write on `node`: read the tenant, change its name, PUT it back."""
    st, _, out = node.admin("GET", f"/tenants/{TENANT_ID}", timeout=10)
    if st != 200:
        return st, out
    body = json.loads(out)
    body.pop("snapshot_stale", None)
    body["name"] = new_name
    st, _, out = node.admin("PUT", f"/tenants/{TENANT_ID}", body, timeout=25)
    return st, out


def converge(nodes, version, name, budget=CONVERGE_BUDGET):
    def reached():
        for n in nodes:
            if n.version() < version:
                return False
            if n.tenant_name() != name:
                return False
        return True

    return wait_for(reached, budget, f"nodes to converge to v{version} with name {name!r}")


def add_provider(node, suffix):
    st, _, out = node.admin(
        "POST",
        "/providers",
        {
            "id": f"p-{suffix}",
            "key": f"p-{suffix}",
            "name": f"provider-{suffix}",
            "endpoint": f"http://127.0.0.1:{UPSTREAM_PORT}",
            "weight": 1,
            "created_at": "",
            "updated_at": "",
        },
        timeout=25,
    )
    return st, out


def main():
    if not os.path.exists(BIN):
        print(f"[shared-db] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    try:
        redis_url = require_redis_url()
        host, port = redis_endpoint(redis_url)
        note = flush_db(REDIS_DB, redis_url)
    except RedisSetupError as e:
        print(f"[shared-db] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2
    try:
        test_db = isolated("shared_db_cluster")
    except SetupError as e:
        print(f"[shared-db] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    global UPSTREAM_PORT
    UPSTREAM_PORT = free_port()
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM_PORT), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    # All nodes share one isolated DB and the Redis relay. The relay lets A6 cut Redis without
    # touching the server the developer's stack uses.
    relay = RedisRelay(host, port)
    relay_port = relay.start()
    node_redis = db_url(REDIS_DB, f"redis://127.0.0.1:{relay_port}")

    nodes = [
        Node(
            f"shared-{label}",
            test_db.url,
            node_redis,
            os.path.join(DIR, f"{label}.log"),
        )
        for label in ("a", "b", "c")
    ]
    A, B, C = nodes

    print(f"== database: {test_db}")
    print(f"== redis: {note}; nodes use relay 127.0.0.1:{relay_port} (db {REDIS_DB})")
    print(f"== upstream: http://127.0.0.1:{UPSTREAM_PORT}; poll={POLL_MS} ms; binary={BIN}")

    try:
        # ------------------------------------------------------------------ startup (A1)
        # A first so its migrations finish before the others connect; all three still share the DB.
        A.start()
        if not wait_for(A.healthy, STARTUP_BUDGET, "node A admin health"):
            print(f"--- A log ---\n{A.log_tail()}", file=sys.stderr)
            print("[shared-db] CANNOT VERIFY: node A never became healthy", file=sys.stderr)
            return 2
        B.start()
        C.start()
        all_up = wait_for(lambda: all(n.healthy() for n in nodes), STARTUP_BUDGET, "all three admin health")
        check(
            "A1: three nodes start against the SAME database and Redis, and each admin "
            "`/api/v1/health` is 200",
            all_up,
            f"a={A.healthy()} b={B.healthy()} c={C.healthy()} db={test_db.name}",
        )
        if not all_up:
            for n in nodes:
                print(f"--- {n.name} log ---\n{n.log_tail()}", file=sys.stderr)
            return 2
        leader_codes = [n.leader_probe() for n in nodes]
        check(
            "A1: every node's `/healthz/leader` is 404 — there is no leader",
            leader_codes == [404, 404, 404],
            f"codes={leader_codes}",
        )
        statuses = [n.status()[1] for n in nodes]
        check(
            "A1: every node reports `cluster=true`, a role and a config version (no raft/leader field)",
            all(s.get("cluster") is True and s.get("role") == "cluster" for s in statuses)
            and all(isinstance(s.get("config_version"), int) for s in statuses),
            f"statuses={statuses}",
        )
        db_reachable = [s.get("db_reachable") for s in statuses]
        check(
            "A1: every node's DB liveness probe reports the shared database reachable",
            db_reachable == [True, True, True],
            f"db_reachable={db_reachable}",
        )

        # ------------------------------------------------------------------ seed + A2
        seed(A)
        announce("seeded tenant/provider/model through", A.name)
        expected = [MODEL]
        served = wait_for(
            lambda: all(n.catalog()[:2] == (200, expected) for n in nodes),
            CONVERGE_BUDGET,
            "the model catalog on A, B and C",
        )
        catalogs = [n.catalog()[:2] for n in nodes]
        check(
            "A2: the seeded model is served by the data plane on A, B and C",
            served,
            f"catalogs={catalogs}",
        )
        seeded_version = max(n.version() for n in nodes)

        # ------------------------------------------------------------------ A3: write on B
        pubsub_a0, pubsub_c0 = A.reload_source("pubsub"), C.reload_source("pubsub")
        name_b = "Drill renamed by B"
        st_b, out_b = rename_tenant(B, name_b)
        version_b = B.version()
        check(
            "A3: a management write on B succeeds (no leader is needed)",
            st_b == 200 and version_b > seeded_version,
            f"HTTP {st_b} version {seeded_version} -> {version_b} {str(out_b)[:120]}",
        )
        converged_b = converge([A, B, C], version_b, name_b)
        check(
            "A3: A and C converge to B's config_version and see the renamed tenant",
            converged_b,
            f"versions a={A.version()} b={B.version()} c={C.version()} names="
            f"{[n.tenant_name() for n in nodes]}",
        )
        changed_a, changed_c = A.reload_source("pubsub") - pubsub_a0, C.reload_source("pubsub") - pubsub_c0
        check(
            "A3: the convergence is carried by Redis pub/sub (the pubsub reload counter moved on "
            "both peers)",
            changed_a > 0 and changed_c > 0,
            f"a={changed_a:g} c={changed_c:g}",
        )

        # ------------------------------------------------------------------ A4: write on C
        pubsub_a0, pubsub_b0 = A.reload_source("pubsub"), B.reload_source("pubsub")
        name_c = "Drill renamed by C"
        st_c, out_c = rename_tenant(C, name_c)
        version_c = C.version()
        check(
            "A4: a management write on C succeeds",
            st_c == 200 and version_c > version_b,
            f"HTTP {st_c} version {version_b} -> {version_c} {str(out_c)[:120]}",
        )
        converged_c = converge([A, B, C], version_c, name_c)
        check(
            "A4: A and B converge to C's config_version and see the renamed tenant",
            converged_c,
            f"versions a={A.version()} b={B.version()} c={C.version()} names="
            f"{[n.tenant_name() for n in nodes]}",
        )
        check(
            "A4: ...again via pub/sub on both peers",
            A.reload_source("pubsub") > pubsub_a0 and B.reload_source("pubsub") > pubsub_b0,
            f"a={A.reload_source('pubsub') - pubsub_a0:g} "
            f"b={B.reload_source('pubsub') - pubsub_b0:g}",
        )

        # ------------------------------------------------------------------ A5: SIGKILL B
        announce("SIGKILL", f"{B.name} (the data plane and the shared DB keep going)")
        B.kill()
        name_kill = "Drill after B was killed"
        st_a, out_a = rename_tenant(A, name_kill)
        version_kill = A.version()
        check(
            "A5: a write on A still succeeds while B is dead",
            st_a == 200 and version_kill > version_c,
            f"HTTP {st_a} version {version_c} -> {version_kill} {str(out_a)[:120]}",
        )
        converged_kill = converge([A, C], version_kill, name_kill)
        check(
            "A5: C converges and still serves a data-plane request after the kill",
            converged_kill and C.catalog()[:2] == (200, expected),
            f"versions a={A.version()} c={C.version()} c-catalog={C.catalog()[:2]}",
        )
        # Optional in the spec, asserted because a restarted node must load the committed DB state.
        B.start()
        restarted = wait_for(B.healthy, STARTUP_BUDGET, "B to restart")
        caught_up = restarted and wait_for(
            lambda: B.version() >= version_kill and B.tenant_name() == name_kill,
            CONVERGE_BUDGET,
            "B to catch up from the shared DB",
        )
        check(
            "A5: B restarts against the shared DB and catches up without replay/peer repair",
            caught_up,
            f"healthy={restarted} version={B.version()} expected>={version_kill} "
            f"name={B.tenant_name()!r}",
        )

        # ------------------------------------------------------------------ A6: Redis outage
        poll_c0 = C.reload_source("poll")
        announce("blocking Redis for all nodes", f"relay :{relay_port}")
        relay.set_blocked(True)
        time.sleep(0.3)
        name_poll = "Drill converged by the poll fallback"
        st_p, out_p = rename_tenant(A, name_poll)
        code_p = None
        if isinstance(out_p, str):
            try:
                code_p = json.loads(out_p).get("error", {}).get("code")
            except Exception:
                code_p = None
        version_p = A.version()
        check(
            "A6: with Redis down the DB commit still succeeds and the write answers 200 "
            "(no `config_not_published`)",
            st_p == 200 and version_p > version_kill and code_p != "config_not_published",
            f"HTTP {st_p} version {version_kill} -> {version_p} code={code_p!r}",
        )
        converged_poll = converge([A, C], version_p, name_poll, budget=max(5.0, CONVERGE_BUDGET))
        poll_delta = C.reload_source("poll") - poll_c0
        check(
            "A6: C converges through the POLL fallback while Redis is unreachable",
            converged_poll and poll_delta > 0,
            f"versions a={A.version()} c={C.version()} poll-reloads-on-c={poll_delta:g} "
            f"name={C.tenant_name()!r}",
        )
        relay.set_blocked(False)
        time.sleep(0.5)
        check(
            "A6: C still serves the converged config after Redis is restored",
            C.catalog()[:2] == (200, expected) and C.tenant_name() == name_poll,
            f"catalog={C.catalog()[:2]} name={C.tenant_name()!r}",
        )

        # ------------------------------------------------------------------ A7: write anywhere
        write_codes = []
        for i, n in enumerate(nodes):
            st, out = add_provider(n, f"any-{i}")
            code = None
            if isinstance(out, str):
                try:
                    code = json.loads(out).get("error", {}).get("code")
                except Exception:
                    code = None
            write_codes.append((n.name, st, code))
        check(
            "A7: every node accepts a management write (no leader, no `config_not_published`)",
            all(st in (200, 201) and code != "config_not_published" for _n, st, code in write_codes),
            f"writes={write_codes}",
        )
        final_version = max(n.version() for n in nodes)
        check(
            "A7: ...and all three converge to the same config_version and tenant name",
            converge(nodes, final_version, name_poll),
            f"versions={[n.version() for n in nodes]} names={[n.tenant_name() for n in nodes]}",
        )

        # ------------------------------------------------------------------ A8: cleanup check
        check(
            "A8: the per-run database is isolated and will be dropped after the drill",
            test_db.name.startswith("hydra_it_") and test_db.backend in ("postgres", "mysql"),
            f"{test_db}",
        )
    finally:
        for n in nodes:
            n.kill()
        relay.stop()
        try:
            upstream.shutdown()
        except Exception:
            pass
        # A cleanup failure must not mask an assertion result; the helper reports it.
        test_db.cleanup()
        shutil.rmtree(DIR, ignore_errors=True)

    print()
    if failures:
        print(f"SHARED DB CLUSTER: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print(
        "SHARED DB CLUSTER: PASSED (3 nodes on one DB + Redis: no leader, write-anywhere, "
        "pub/sub convergence, node-kill continuity, poll fallback under a Redis outage, "
        "cleanup)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
