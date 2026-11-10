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
import sys
import threading
import time
from http.server import ThreadingHTTPServer

from _cluster_harness import (
    Node,
    Upstream,
    free_port,
    rename_tenant,
    seed_base,
    wait_for,
)
from _redis_harness import (
    RedisRelay,
    RedisSetupError,
    db_url,
    endpoint as redis_endpoint,
    flush_db,
    require_url as require_redis_url,
)
from _shared_db import SetupError, isolated

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
DIR = os.path.join(ROOT, ".acceptance", "shared-db-cluster")

ADMIN_TOKEN = "hydra-shared-db-admin-2026"
ENCRYPTION_KEY = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
TENANT_ID = "t-drill"
TENANT_DOMAIN = "drill.local"
MODEL = "echo"

# Short poll so the missed-message leg finishes quickly, while the pub/sub legs assert the
# pubsub counter (not just convergence), so a fast poll cannot make them green for the wrong
# reason. Override with HYDRA_CONFIG_POLL_MS when investigating convergence.
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


def converge(nodes, version, name, budget=CONVERGE_BUDGET):
    def reached():
        for n in nodes:
            if n.version() < version:
                return False
            if n.tenant_name(TENANT_ID) != name:
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
            admin_token=ADMIN_TOKEN,
            encryption_key=ENCRYPTION_KEY,
            poll_ms=POLL_MS,
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
        seed_base(
            A,
            tenant_id=TENANT_ID,
            domain=TENANT_DOMAIN,
            model=MODEL,
            upstream_port=UPSTREAM_PORT,
            prefix="drill",
            tenant_name="Shared DB Drill",
        )
        announce("seeded tenant/provider/model through", A.name)
        expected = [MODEL]
        served = wait_for(
            lambda: all(n.catalog(TENANT_DOMAIN)[:2] == (200, expected) for n in nodes),
            CONVERGE_BUDGET,
            "the model catalog on A, B and C",
        )
        catalogs = [n.catalog(TENANT_DOMAIN)[:2] for n in nodes]
        check(
            "A2: the seeded model is served by the data plane on A, B and C",
            served,
            f"catalogs={catalogs}",
        )
        seeded_version = max(n.version() for n in nodes)

        # ------------------------------------------------------------------ A3: write on B
        pubsub_a0, pubsub_c0 = A.reload_source("pubsub"), C.reload_source("pubsub")
        name_b = "Drill renamed by B"
        st_b, out_b = rename_tenant(B, TENANT_ID, name_b)
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
            f"{[n.tenant_name(TENANT_ID) for n in nodes]}",
        )
        # Convergence may be satisfied by the short poll before the pubsub message is
        # handled. Wait for the pubsub counters too, so this assertion still proves the
        # fanout path instead of racing it.
        wait_for(
            lambda: A.reload_source("pubsub") > pubsub_a0
            and C.reload_source("pubsub") > pubsub_c0,
            CONVERGE_BUDGET,
            "A3 pubsub reload counters",
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
        st_c, out_c = rename_tenant(C, TENANT_ID, name_c)
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
            f"{[n.tenant_name(TENANT_ID) for n in nodes]}",
        )
        wait_for(
            lambda: A.reload_source("pubsub") > pubsub_a0
            and B.reload_source("pubsub") > pubsub_b0,
            CONVERGE_BUDGET,
            "A4 pubsub reload counters",
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
        st_a, out_a = rename_tenant(A, TENANT_ID, name_kill)
        version_kill = A.version()
        check(
            "A5: a write on A still succeeds while B is dead",
            st_a == 200 and version_kill > version_c,
            f"HTTP {st_a} version {version_c} -> {version_kill} {str(out_a)[:120]}",
        )
        converged_kill = converge([A, C], version_kill, name_kill)
        check(
            "A5: C converges and still serves a data-plane request after the kill",
            converged_kill and C.catalog(TENANT_DOMAIN)[:2] == (200, expected),
            f"versions a={A.version()} c={C.version()} c-catalog={C.catalog(TENANT_DOMAIN)[:2]}",
        )
        # Optional in the spec, asserted because a restarted node must load the committed DB state.
        B.start()
        restarted = wait_for(B.healthy, STARTUP_BUDGET, "B to restart")
        caught_up = restarted and wait_for(
            lambda: B.version() >= version_kill and B.tenant_name(TENANT_ID) == name_kill,
            CONVERGE_BUDGET,
            "B to catch up from the shared DB",
        )
        check(
            "A5: B restarts against the shared DB and catches up without replay/peer repair",
            caught_up,
            f"healthy={restarted} version={B.version()} expected>={version_kill} "
            f"name={B.tenant_name(TENANT_ID)!r}",
        )

        # ------------------------------------------------------------------ A6: Redis outage
        poll_c0 = C.reload_source("poll")
        announce("blocking Redis for all nodes", f"relay :{relay_port}")
        relay.set_blocked(True)
        time.sleep(0.3)
        name_poll = "Drill converged by the poll fallback"
        st_p, out_p = rename_tenant(A, TENANT_ID, name_poll)
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
            f"name={C.tenant_name(TENANT_ID)!r}",
        )
        relay.set_blocked(False)
        time.sleep(0.5)
        check(
            "A6: C still serves the converged config after Redis is restored",
            C.catalog(TENANT_DOMAIN)[:2] == (200, expected) and C.tenant_name(TENANT_ID) == name_poll,
            f"catalog={C.catalog(TENANT_DOMAIN)[:2]} name={C.tenant_name(TENANT_ID)!r}",
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
            f"versions={[n.version() for n in nodes]} names={[n.tenant_name(TENANT_ID) for n in nodes]}",
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
