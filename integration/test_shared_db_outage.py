#!/usr/bin/env python3
"""A real DATABASE outage: admin writes fail closed, the data plane keeps serving the snapshot.

The shared-DB architecture makes the database authoritative for config, but every process keeps an
in-process snapshot. A partition must therefore look like this:

  1. before the cut: a seeded tenant/provider/model is served by the data plane.
  2. DB unreachable: an ADMIN WRITE answers non-2xx (never 2xx while the DB is down) and the data
     plane still serves the last committed snapshot (200).
  3. DB restored: the pool reconnects, a write succeeds and advances `config_version`, and the
     data plane reflects the new config.
  4. the process never exits; recovery is bounded.

The DB is reached through a raw TCP relay (`_db_harness.DbRelay`), so the node sees its normal
database name and credentials while the relay can cut and restore the socket. The per-run
schema/database comes from `_shared_db.py`; PostgreSQL and MySQL use the same relay.

Run:
  export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse,db-postgres --bin hydra
  python3 integration/test_shared_db_outage.py
  # MySQL: mysql://... with the db-mysql feature.
Exit 0 pass · 1 an assertion failed · 2 could not verify (missing env/binary/services).
"""

from __future__ import annotations

import json
import os
import shutil
import sys
import threading
import time
from http.server import ThreadingHTTPServer

from _cluster_harness import Node, Upstream, call, free_port, seed_base, wait_for
from _db_harness import DbRelay, db_host_port, rewrite_db_url
from _shared_db import SetupError, isolated

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
DIR = os.path.join(ROOT, ".acceptance", "shared-db-outage")

ADMIN_TOKEN = "hydra-outage-admin-2026"
ENCRYPTION_KEY = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
TENANT_ID = "t-outage"
TENANT_DOMAIN = "outage.local"
MODEL = "echo"
NEW_MODEL = "m-outage"

STARTUP_BUDGET = float(os.environ.get("HYDRA_OUTAGE_STARTUP_BUDGET", "60"))
CONVERGE_BUDGET = float(os.environ.get("HYDRA_OUTAGE_CONVERGE_BUDGET", "15"))
RECOVERY_BUDGET = float(os.environ.get("HYDRA_OUTAGE_RECOVERY_BUDGET", "30"))

failures: list[str] = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail=""):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def status_code(body):
    try:
        return json.loads(body).get("error", {}).get("code")
    except Exception:
        return None


def catalog(node):
    st, ids, out = node.catalog(TENANT_DOMAIN)
    return st, ids, out


def main():
    if not os.path.exists(BIN):
        print(f"[outage] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    base_url = os.environ.get("HYDRA_TEST_DATABASE_URL", "").strip()
    if not base_url:
        print(
            "[outage] CANNOT VERIFY: set HYDRA_TEST_DATABASE_URL to a real PostgreSQL/MySQL base "
            "database (see integration/README.md)", file=sys.stderr
        )
        return 2
    try:
        test_db = isolated("shared_db_outage")
        upstream_host, upstream_port = db_host_port(test_db.url)
    except (SetupError, ValueError) as e:
        print(f"[outage] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    relay = DbRelay(upstream_host, upstream_port)
    relay_port = relay.start()
    relay_url = rewrite_db_url(test_db.url, "127.0.0.1", relay_port)

    mock_port = free_port()
    upstream = ThreadingHTTPServer(("127.0.0.1", mock_port), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    node = Node(
        "outage-node",
        relay_url,
        "",  # single node: no Redis needed
        os.path.join(DIR, "node.log"),
        admin_token=ADMIN_TOKEN,
        encryption_key=ENCRYPTION_KEY,
        poll_ms=200,
        extra_env={
            "HYDRA_DB_MAX_CONNECTIONS": "2",
            "HYDRA_SHUTDOWN_DRAIN_SECS": "1",
        },
    )
    # Node always sets HYDRA_REDIS_URL from the constructor; an empty value is how single-node is
    # expressed (main.rs treats an empty URL as "not configured").
    print(f"== database: {test_db}")
    print(f"== relay: 127.0.0.1:{relay_port} -> {upstream_host}:{upstream_port}")
    print(f"== upstream: http://127.0.0.1:{mock_port}; binary={BIN}")

    try:
        node.start()
        if not wait_for(node.healthy, STARTUP_BUDGET, "node admin health"):
            print(f"--- node log ---\n{node.log_tail()}", file=sys.stderr)
            print("[outage] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            return 2
        seed_base(
            node,
            tenant_id=TENANT_ID,
            domain=TENANT_DOMAIN,
            model=MODEL,
            upstream_port=mock_port,
            prefix="outage",
            tenant_name="Outage tenant",
        )
        if not wait_for(lambda: catalog(node)[:2] == (200, [MODEL]), CONVERGE_BUDGET,
                        "the seeded model on the data plane"):
            print(f"catalog={catalog(node)[:2]}", file=sys.stderr)
            print("[outage] CANNOT VERIFY: the seeded config is not served", file=sys.stderr)
            return 2
        st, status_before = node.status()
        version_before = int(status_before.get("config_version", -1))
        check("1: before the outage the data plane serves the seeded model",
              catalog(node)[:2] == (200, [MODEL]),
              f"catalog={catalog(node)[:2]} version={version_before}")

        # ---------------------------------------------------------------- cut
        announce("cut the DB relay", "existing pool sockets are severed; new connects refused")
        relay.cut()
        t0 = time.monotonic()
        failed_id = "p-outage-down"
        st_fail, headers, body = node.admin(
            "POST",
            "/providers",
            {
                "id": failed_id,
                "key": failed_id,
                "name": "must not be created while the DB is down",
                "endpoint": f"http://127.0.0.1:{mock_port}",
                "weight": 1,
                "created_at": "",
                "updated_at": "",
            },
            timeout=45,
        )
        fail_elapsed = time.monotonic() - t0
        code = status_code(body)
        check("2a: an admin write while the DB is unreachable FAILS (500/503, never 2xx)",
              500 <= st_fail < 600,
              f"HTTP {st_fail} in {fail_elapsed:.2f}s code={code!r} body={body[:140]!r}")
        check("2b: ...and the failure is not a `config_not_published` write lie",
              code != "config_not_published", f"code={code!r}")
        check("2c: ...and the process is still alive", node.alive(),
              f"alive={node.alive()} rc={node.proc.poll() if node.proc else None}")
        st_stale, ids_stale, _ = catalog(node)
        check("2d: the data plane still serves the stale committed snapshot (200)",
              st_stale == 200 and ids_stale == [MODEL],
              f"HTTP {st_stale} ids={ids_stale}")
        gauge = node.metric_values("hydra_config_version")
        version_down = int(gauge[-1]) if gauge else None
        check("2e: ...and the failed write did not advance the local config version",
              version_down is None or version_down == version_before,
              f"hydra_config_version={version_down} expected={version_before}")

        # ---------------------------------------------------------------- restore
        announce("restore the DB relay", "poll the same write until the pool reconnects")
        relay.restore()
        t_restore = time.monotonic()
        recovered = False
        st_ok = 0
        body_ok = ""
        while time.monotonic() - t_restore < RECOVERY_BUDGET:
            st_ok, _, body_ok = node.admin(
                "POST",
                "/providers",
                {
                    "id": failed_id,
                    "key": failed_id,
                    "name": "created after recovery",
                    "endpoint": f"http://127.0.0.1:{mock_port}",
                    "weight": 1,
                    "created_at": "",
                    "updated_at": "",
                },
                timeout=15,
            )
            if st_ok in (200, 201):
                recovered = True
                break
            time.sleep(0.5)
        recovery_s = time.monotonic() - t_restore
        check(f"3a: after restore the SAME admin write succeeds within {RECOVERY_BUDGET:.0f}s",
              recovered, f"HTTP {st_ok} after {recovery_s:.2f}s {body_ok[:120]!r}")
        check("3b: ...and recovery is bounded", recovery_s <= RECOVERY_BUDGET,
              f"{recovery_s:.2f}s <= {RECOVERY_BUDGET:.0f}s")
        stv, status_after = node.status()
        version_after = int(status_after.get("config_version", -1))
        check("3c: ...and `config_version` advanced",
              stv == 200 and version_after > version_before,
              f"version {version_before} -> {version_after}")

        # A data-plane-visible write: grant a new model to the tenant.
        for path, payload in (
            ("/provider-models", {"id": "pm-outage-new", "key": NEW_MODEL, "name": "New",
                                  "provider_id": "p-outage", "status": 1,
                                  "created_at": "", "updated_at": ""}),
            ("/tenant-models", {"id": "tm-outage-new", "tenant_id": TENANT_ID,
                                "model_key": NEW_MODEL, "created_at": "", "updated_at": ""}),
        ):
            st, _, out = node.admin("POST", path, payload, timeout=20)
            if st not in (200, 201):
                check(f"4: data-plane write {path} succeeds after recovery", False,
                      f"HTTP {st} {out[:140]}")
                break
        else:
            check("4: data-plane writes succeed after recovery", True, "provider-model + grant 2xx")
        revised = wait_for(
            lambda: catalog(node)[:2] == (200, sorted([MODEL, NEW_MODEL])),
            CONVERGE_BUDGET,
            "the data plane to reflect the post-recovery write",
        )
        check("5: the data plane reflects the post-recovery write",
              revised, f"catalog={catalog(node)[:2]}")
        check("6: the node never crashed and is still healthy", node.alive() and node.healthy(),
              f"alive={node.alive()} healthy={node.healthy()}")
        announce("outage timings", f"failed-write={fail_elapsed:.2f}s recovery={recovery_s:.2f}s")
    finally:
        node.kill()
        relay.stop()
        try:
            upstream.shutdown()
        except Exception:
            pass
        test_db.cleanup()
        shutil.rmtree(DIR, ignore_errors=True)

    print()
    if failures:
        print(f"DB OUTAGE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print(
        "DB OUTAGE: PASSED (admin write fails 5xx while the DB is cut, data plane serves the stale "
        "snapshot, pool reconnects, version advances, post-recovery config is served)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
