#!/usr/bin/env python3
"""The `ops.md` §1.4 BACKUP & RESTORE runbook, executed instead of trusted.

§1.4 makes four claims, all of them measured once by hand and (until now) pinned by
nothing:

  A. ❌ `cp data/hydra.db /backup/x.db` while the node runs can produce a copy that is
     structurally EMPTY, because committed-but-uncheckpointed transactions live in
     `hydra.db-wal` ("the copy was 4096 bytes and every table was unreadable");
  B. ✅ `sqlite3 "$DB_PATH" "VACUUM INTO '/backup/x.db'"` on a LIVE database yields a
     consistent snapshot, and a second node booted from that snapshot serves a proxied
     request with HTTP 200 (config + SEALED provider key both survived);
  C. restore = stop the node, put the snapshot in place, **delete any stale
     `hydra.db-wal` / `hydra.db-shm` beside it**, start with the same key;
  D. ⚠️ the snapshot is useless under a DIFFERENT master key: the node "refuses to
     serve" (`fatal startup error … error occurred while decoding …`).

The first run of this drill found two things worth knowing:
  * the naive copy is not necessarily TINY — it was 221 184 bytes (a full-looking file)
    and still silently MISSED 86 of 301 providers, which is worse than the 4096-byte
    empty copy §1.4 records: nothing about the file looks wrong;
  * the "delete any stale -wal/-shm" instruction is **load-bearing**: leaving the old
    write-ahead log beside the restored snapshot makes SQLite report
    `database disk image is malformed` and the node REFUSES TO START.

Cases (each prints the raw measurement, not just pass/fail):
  A  naive copy with pending WAL      -> the copy is missing committed rows
  B1 VACUUM INTO on a live node       -> the snapshot holds every row
  B2 a SECOND node booted from it     -> serves 200 through the proxy
  C1 stale -wal/-shm left beside it   -> malformed; the node refuses to start
  C2 the runbook's instruction        -> delete them, and the same file serves 200
  D  same snapshot, different key     -> the node refuses to serve
  E  when is a -wal actually left?    -> clean SIGTERM vs SIGKILL, measured

`sqlite3` CLI is not installed in this environment, so the SQL statement is issued
through Python's bundled SQLite (`VACUUM INTO` is the same statement the runbook runs;
both are SQLite ≥ 3.27).

Run: python3 integration/test_backup_restore.py    # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import io
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "backup-restore-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))

ADMIN, DATA = 18710, 18711          # node 1 (the one that gets backed up)
ADMIN2, DATA2 = 18712, 18713        # node 2 (booted FROM the snapshot)
ADMIN3, DATA3 = 18714, 18715        # node 3 (snapshot under a DIFFERENT master key)
UPSTREAM = 18719
TOKEN = "hydra-backup-admin-2026"
KEY_A = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="          # 32 bytes, base64
KEY_B = "YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODk="          # a different 32-byte key

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    """A MEASUREMENT (not an assertion): printed so the runbook claim can be re-read."""
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


class Upstream(BaseHTTPRequestHandler):
    """Answers the auth hop with a verdict and the chat path with a completion."""

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        if self.path.startswith("/auth"):
            body = json.dumps({"allowed": True, "reason": "backup-drill", "expires_in": 60}).encode()
        else:
            body = json.dumps({"id": "chatcmpl-b", "object": "chat.completion",
                               "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
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


def node_env(db_name, admin, data, key=KEY_A, extra=None):
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin}",
        "HYDRA_LISTEN": f"127.0.0.1:{data}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, db_name)}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": key,
        "RUST_LOG": "warn",
    })
    if extra:
        env.update(extra)
    return env


def start(db_name, admin, data, log_name, key=KEY_A, extra=None):
    log = open(os.path.join(DIR, log_name), "w")
    return subprocess.Popen([BIN], env=node_env(db_name, admin, data, key, extra),
                            stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(admin, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{admin}/api/v1/health", token=TOKEN)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def wait_dead(proc, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if proc.poll() is not None:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed(admin):
    """One tenant, one provider, one MODEL, one sealed provider key, the links — plus a
    burst of extra providers so the write-ahead log is guaranteed to hold data."""
    for path, payload in [
        ("providers", {"id": "p1", "key": "p1", "name": "P", "endpoint": f"http://127.0.0.1:{UPSTREAM}",
                       "weight": 1, "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": "load.local",
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]:
        st, _, out = call("POST", f"http://127.0.0.1:{admin}/api/v1/{path}", token=TOKEN, body=payload)
        if st not in (200, 201):
            raise SystemExit(f"[backup] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")
    # The burst is what makes case A deterministic: SQLite's default auto-checkpoint
    # fires at ~1000 pages, so a few hundred small rows stay in the WAL.
    for i in range(300):
        call("POST", f"http://127.0.0.1:{admin}/api/v1/providers", token=TOKEN,
             body={"id": f"burst{i:03d}", "key": f"k{i:03d}", "name": f"B{i:03d}",
                   "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 0,
                   "created_at": "", "updated_at": ""})
    call("POST", f"http://127.0.0.1:{admin}/api/v1/reload", token=TOKEN, body={})
    time.sleep(0.3)


def proxied(data_port):
    body = {"model": "echo", "messages": [{"role": "user", "content": "hi"}]}
    req = urllib.request.Request(
        f"http://127.0.0.1:{data_port}/v1/chat/completions",
        data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", "Host": "load.local",
                 "Authorization": "Bearer sk-tenant-1"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)


TABLES = ("provider", "provider_key", "provider_model", "tenant", "tenant_provider",
          "tenant_model")


def table_counts(path):
    """Row counts per table, read with SQLite itself (read-only, no side effects)."""
    out = {}
    con = sqlite3.connect(f"file:{path}?mode=ro", uri=True, timeout=10)
    try:
        for t in TABLES:
            try:
                out[t] = con.execute(f"SELECT COUNT(*) FROM {t}").fetchone()[0]
            except sqlite3.DatabaseError as e:
                out[t] = f"unreadable ({type(e).__name__})"
    finally:
        con.close()
    return out


def main():
    if not os.path.exists(BIN):
        print(f"[backup] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    import threading
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    live_db = os.path.join(DIR, "hydra.db")
    naive = os.path.join(DIR, "naive-copy.db")
    snapshot = os.path.join(DIR, "vacuum-into.db")

    node1 = start("hydra.db", ADMIN, DATA, "node1.log")
    node2 = None
    node3 = None
    try:
        if not wait_healthy(ADMIN):
            print("[backup] CANNOT VERIFY: node 1 never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "node1.log")).read()[-800:], file=sys.stderr)
            return 2
        seed(ADMIN)

        live = table_counts(live_db)
        announce("live database row counts", f"{live}")
        wal = live_db + "-wal"
        wal_size = os.path.getsize(wal) if os.path.exists(wal) else 0
        announce("write-ahead log beside the live database", f"{wal_size} bytes")
        if wal_size == 0:
            # Do not quietly pass: the whole case-A claim is about a NON-EMPTY WAL.
            print("[backup] CANNOT VERIFY: the WAL is empty (SQLite checkpointed), so the "
                  "naive-copy case cannot be measured; raise the burst size", file=sys.stderr)
            return 2

        # ---- A: the WRONG way, on a live node with pending WAL -------------------
        shutil.copyfile(live_db, naive)
        copy_counts = table_counts(naive)
        announce("naive `cp` copy of the live db file", f"{os.path.getsize(naive)} bytes, {copy_counts}")
        missing = [t for t in TABLES
                   if isinstance(copy_counts[t], int) and isinstance(live[t], int)
                   and copy_counts[t] < live[t]]
        unreadable = [t for t in TABLES if not isinstance(copy_counts[t], int)]
        check("A: the naive copy is NOT a usable backup (rows missing / tables unreadable)",
              bool(missing or unreadable),
              f"missing={ {t: (copy_counts[t], live[t]) for t in missing} } unreadable={unreadable}")

        # ---- B1: the RIGHT way — VACUUM INTO, while the node keeps serving --------
        status_before = call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/health", token=TOKEN)[0]
        con = sqlite3.connect(live_db, timeout=10)
        try:
            con.execute(f"VACUUM INTO '{snapshot}'")
        finally:
            con.close()
        status_after = call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/health", token=TOKEN)[0]
        snap = table_counts(snapshot)
        snap_missing = {t: (snap[t], live[t]) for t in TABLES if snap[t] != live[t]}
        announce("VACUUM INTO snapshot", f"{os.path.getsize(snapshot)} bytes, {snap}")
        # NOTE: this must NOT reuse case A's `missing` — the first version did, so a
        # PERFECT snapshot was reported as a failure (left: the naive copy's 215/301).
        check("B1: the snapshot holds EVERY row the live database had",
              not snap_missing, f"differing={snap_missing} snapshot={snap} live={live}")
        check("B1: VACUUM INTO needs no downtime (the node kept answering 200)",
              status_before == 200 and status_after == 200,
              f"before={status_before} after={status_after}")

        # ---- B2: a SECOND node booted from the snapshot serves traffic -----------
        shutil.copyfile(snapshot, os.path.join(DIR, "restored.db"))
        node2 = start("restored.db", ADMIN2, DATA2, "node2.log")
        if not wait_healthy(ADMIN2):
            check("B2: a node booted from the snapshot becomes healthy", False, "never healthy")
        else:
            st, body = proxied(DATA2)
            # 200 proves BOTH halves: the routing config came back AND the sealed provider
            # key was decryptable (a key the node could not open would be a 503).
            check("B2: the restored node serves a proxied request with 200 (config + sealed key survived)",
                  st == 200, f"HTTP {st} {body[:80]}")
        stop(node2)
        node2 = None

        # ---- C1: a stale -wal/-shm left beside the restored snapshot --------------
        # §1.4 tells the operator to delete them. Measure what happens if they don't:
        # this is the realistic mistake (the node was killed, or is still running, so a
        # -wal exists next to the database the operator is about to REPLACE).
        restored = os.path.join(DIR, "stale.db")
        shutil.copyfile(snapshot, restored)
        shutil.copyfile(wal, restored + "-wal")
        shutil.copyfile(live_db + "-shm", restored + "-shm")
        stale_counts = table_counts(restored)
        announce("snapshot + the OLD -wal/-shm left beside it", f"{stale_counts}")
        check("C1: a stale -wal makes the restored file UNREADABLE (the runbook's warning is load-bearing)",
              any(not isinstance(v, int) for v in stale_counts.values()),
              f"{stale_counts}")
        node2 = start("stale.db", ADMIN2, DATA2, "node2-stale.log")
        stale_died = wait_dead(node2, budget=20.0)
        # The health probe must run BEFORE the node is stopped. The first version asked
        # `not wait_healthy(...)` AFTER `stop(node2)`, so it passed even when the node was
        # perfectly healthy — a vacuous assertion (found by the "leave no stale WAL" probe).
        stale_healthy = (not stale_died) and wait_healthy(ADMIN2, budget=3.0)
        stale_rc = node2.returncode
        stop(node2)
        node2 = None
        stale_log = open(os.path.join(DIR, "node2-stale.log"), errors="replace").read()
        announce("the stale-WAL node's exit", f"died={stale_died} healthy={stale_healthy} rc={stale_rc}")
        check("C1: ...and the NODE refuses to start on it (not just the reader)",
              stale_died or not stale_healthy,
              f"died={stale_died} healthy={stale_healthy} log tail={stale_log.strip().splitlines()[-1][:120] if stale_log.strip() else 'no log'}")
        check("C1: the error names the cause (`malformed`), so an operator knows what to delete",
              "malformed" in stale_log.lower(),
              f"tail={stale_log.strip().splitlines()[-1][:120] if stale_log.strip() else 'no log'}")

        # ---- C2: the runbook's instruction, followed exactly ---------------------
        os.remove(restored + "-wal")
        if os.path.exists(restored + "-shm"):
            os.remove(restored + "-shm")
        clean_counts = table_counts(restored)
        announce("the same snapshot after deleting the stale -wal/-shm", f"{clean_counts}")
        check("C2: deleting them makes the file readable with every row intact",
              all(clean_counts[t] == live[t] for t in TABLES),
              f"cleaned={clean_counts} live={live}")
        node2 = start("stale.db", ADMIN2, DATA2, "node2-clean.log")
        if not wait_healthy(ADMIN2):
            check("C2: a node booted from it becomes healthy", False, "never healthy")
        else:
            st, body = proxied(DATA2)
            check("C2: ...and serves a proxied request with 200", st == 200, f"HTTP {st} {body[:60]}")
        stop(node2)
        node2 = None

        # ---- D: same snapshot, DIFFERENT master key ------------------------------
        shutil.copyfile(snapshot, os.path.join(DIR, "wrongkey.db"))
        node3 = start("wrongkey.db", ADMIN3, DATA3, "node3.log", key=KEY_B)
        died = wait_dead(node3, budget=20.0)
        log3 = open(os.path.join(DIR, "node3.log"), errors="replace").read()
        announce("node 3 (different HYDRA_ENCRYPTION_KEY) exit", f"died={died} rc={node3.poll()}")
        check("D: a node pointed at the snapshot under a DIFFERENT key refuses to serve",
              died or node3.poll() is not None,
              f"exit={node3.poll()}")
        check("D: ...and the log says WHY (a decoding/decrypt error, not a generic crash)",
              ("decod" in log3.lower() or "decrypt" in log3.lower() or "key" in log3.lower()),
              f"tail={log3.strip().splitlines()[-1][:120] if log3.strip() else 'no log'}")
        stop(node3)
        node3 = None
        announce("node 3 log tail", (log3.strip().splitlines() or [""])[-1][:160])

        # ---- E: WHEN is a -wal actually left behind? ------------------------------
        # C1/C2 only matter if an operator can really meet a stale -wal. Measure both
        # exits: a graceful SIGTERM (the drain in §13.5b) and a hard SIGKILL (crash,
        # `docker kill`, OOM). The drain is shortened with HYDRA_SHUTDOWN_DRAIN_SECS so
        # this case costs ~1s instead of ~25s.
        stop(node1)  # SIGKILL (the finally block would do it anyway)
        node1 = None
        node1 = start("hydra.db", ADMIN, DATA, "node1-term.log", extra={"HYDRA_SHUTDOWN_DRAIN_SECS": "1"})
        if not wait_healthy(ADMIN):
            check("E: the node came back up for the clean-shutdown measurement", False, "never healthy")
        else:
            call("POST", f"http://127.0.0.1:{ADMIN}/api/v1/providers", token=TOKEN,
                 body={"id": "extra1", "key": "x1", "name": "X", "endpoint": f"http://127.0.0.1:{UPSTREAM}",
                       "weight": 0, "created_at": "", "updated_at": ""})
            node1.send_signal(signal.SIGTERM)
            node1.wait(timeout=40)
            node1 = None
            left = [f for f in ("hydra.db-wal", "hydra.db-shm") if os.path.exists(os.path.join(DIR, f))]
            announce("after a CLEAN SIGTERM stop", f"left behind: {left or 'nothing'}")
            # Measured truth, and the opposite of what SQLite normally does: Pingora ends
            # with `process::exit(0)` (main.rs), so the connection pool is never closed
            # and the WAL is never checkpointed away. That makes C1's trap the NORMAL
            # case: every restart (the documented upgrade path included) leaves a -wal
            # beside `hydra.db`.
            check("E: a clean SIGTERM stop LEAVES the -wal/-shm behind (no clean SQLite close)",
                  left == ["hydra.db-wal", "hydra.db-shm"], f"left={left}")
            # ...but a leftover WAL beside its OWN database is not a problem: the next
            # start recovers it. Only REPLACING the database under it is fatal (C1).
            node1 = start("hydra.db", ADMIN, DATA, "node1-restart.log")
            if not wait_healthy(ADMIN):
                check("E: the node restarts on its own leftover -wal and becomes healthy", False, "never healthy")
            else:
                st, _, out = call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/providers/extra1", token=TOKEN)
                check("E: ...and the row written just before the stop is still there (the WAL was replayed)",
                      st == 200, f"HTTP {st} {out[:80]}")
                check("E: the database is intact after the clean stop + restart",
                      all(isinstance(v, int) for v in table_counts(live_db).values()),
                      f"{table_counts(live_db)}")

    finally:
        stop(node1)
        if node2 is not None:
            stop(node2)
        if node3 is not None:
            stop(node3)
        upstream.shutdown()

    print()
    if failures:
        print(f"BACKUP/RESTORE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("BACKUP/RESTORE: PASSED (naive copy loses rows, VACUUM INTO snapshot complete and "
          "serves, stale -wal beside a REPLACED db = malformed and the node refuses to "
          "start while deleting it fixes it, wrong master key refused, a clean stop leaves "
          "the -wal)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
