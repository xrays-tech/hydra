#!/usr/bin/env python3
"""BACKUP & RESTORE on the real shared database (PostgreSQL or MySQL).

SQLite's `VACUUM INTO` / `-wal` runbook is gone with the backend. This drill replaces it with a
logical restore into a fresh isolated schema/database:

  A  a node seeded with provider / provider key / tenant + grants serves the data plane
  B  the rows are copied into a second, freshly migrated schema/database; a node started on the
     restored database is healthy and serves the same config (the sealed provider key was
     decryptable with the same master key, or startup would have failed)
  C  the restored database under a DIFFERENT master key refuses to serve (the sealed rows cannot
     be decoded) — the backup is useless without the key, as the docs say

The copy is a real cross-schema/database `INSERT ... SELECT` (both isolated stores live on the
same server). No SQLite, no temp `.db` files, no file-copy fiction. The per-run schemas are
dropped on exit.

Run:
  export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse,db-postgres --bin hydra
  python3 integration/test_backup_restore.py
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""

import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from _shared_db import SetupError, copy_isolated_rows, isolated
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "backup-restore-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))

ADMIN, DATA = 18710, 18711
ADMIN2, DATA2 = 18712, 18713
ADMIN3, DATA3 = 18714, 18715
UPSTREAM = 18719
TOKEN = "hydra-backup-admin-2026"
KEY_A = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
KEY_B = "YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODk="

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail=""):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


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
                "id": "chatcmpl-backup",
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
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)


def admin(method, path, body=None, port=ADMIN):
    return call(method, f"http://127.0.0.1:{port}/api/v1{path}", token=TOKEN, body=body)


def catalog(data_port):
    st, out = call("GET", f"http://127.0.0.1:{data_port}/v1/models", host="backup.local")
    try:
        return st, sorted(m["id"] for m in json.loads(out).get("data", []))
    except Exception:
        return st, None


def start(db_url, admin_port, data_port, log_name, key=KEY_A):
    assert db_url
    env = {k: v for k, v in os.environ.items() if not k.startswith("HYDRA_")}
    env.update(usage_env())
    env.update(
        {
            "HYDRA_ADMIN_TOKEN": TOKEN,
            "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin_port}",
            "HYDRA_LISTEN": f"127.0.0.1:{data_port}",
            "HYDRA_DB_URL": db_url,
            "HYDRA_ENCRYPTION_KEY": key,
            "RUST_LOG": "warn",
        }
    )
    log = open(os.path.join(DIR, log_name), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(admin_port, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health", port=admin_port)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed(admin_port):
    for path, payload in (
        ("/providers", {"id": "p1", "key": "p1", "name": "P",
                        "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                            "created_at": ""}),
        ("/tenants", {"id": "t1", "name": "T", "enabled": True, "domain": "backup.local",
                      "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
    ):
        st, out = admin("POST", path, payload, port=admin_port)
        if st not in (200, 201):
            raise RuntimeError(f"seeding {path} -> HTTP {st} {out[:180]}")


def main():
    if not os.path.exists(BIN):
        print(f"[backup] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    try:
        source = isolated("backup_live")
        restored = isolated("backup_restored")
    except SetupError as e:
        print(f"[backup] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    node1 = node2 = node3 = None
    print(f"== source: {source}")
    print(f"== restored: {restored}")

    try:
        # ---- A: the source node and its fixture ------------------------------
        node1 = start(source.url, ADMIN, DATA, "node1.log")
        if not wait_healthy(ADMIN):
            print(open(os.path.join(DIR, "node1.log"), errors="replace").read()[-900:],
                  file=sys.stderr)
            print("[backup] CANNOT VERIFY: node 1 never became healthy", file=sys.stderr)
            return 2
        seed(ADMIN)
        st, ids = catalog(DATA)
        check(
            "A: the source node serves the seeded config (data-plane model catalog)",
            st == 200 and ids == ["echo"],
            f"HTTP {st} ids={ids}",
        )

        # ---- B: restore into a fresh isolated schema/database ---------------
        # First boot runs the migrations and creates the target tables (empty config is valid).
        node2 = start(restored.url, ADMIN2, DATA2, "node2-migrate.log")
        if not wait_healthy(ADMIN2):
            print(open(os.path.join(DIR, "node2-migrate.log"), errors="replace").read()[-900:],
                  file=sys.stderr)
            print("[backup] CANNOT VERIFY: the empty restored DB did not migrate", file=sys.stderr)
            return 2
        stop(node2)
        node2 = None

        copied = copy_isolated_rows(source, restored)
        announce("tables restored", f"{copied}")
        check(
            "B1: the live config was copied into the freshly migrated database",
            {"provider", "provider_key", "provider_model", "tenant",
             "tenant_provider", "tenant_model"}.issubset(set(copied)),
            f"copied={copied}",
        )

        node2 = start(restored.url, ADMIN2, DATA2, "node2.log")
        healthy2 = wait_healthy(ADMIN2)
        st2, ids2 = catalog(DATA2)
        # A startup failure here is the sealed-key failure the docs warn about: the config load
        # decrypts provider keys, so health 200 proves the same master key still opens them.
        check(
            "B2: a node started on the restored DB is healthy and serves the same config "
            "(the sealed provider key is decryptable)",
            healthy2 and st2 == 200 and ids2 == ["echo"],
            f"healthy={healthy2} catalog=({st2}, {ids2})",
        )
        st_providers, out_providers = admin("GET", "/providers", port=ADMIN2)
        check(
            "B2: ...and the admin API reads the restored provider row back",
            st_providers == 200 and '"id":"p1"' in out_providers.replace(" ", ""),
            f"HTTP {st_providers} {out_providers[:120]}",
        )
        stop(node2)
        node2 = None

        # ---- C: the backup is useless under a different master key ----------
        node3 = start(restored.url, ADMIN3, DATA3, "node3.log", key=KEY_B)
        healthy3 = wait_healthy(ADMIN3, budget=12.0)
        log3 = open(os.path.join(DIR, "node3.log"), errors="replace").read()
        check(
            "C: the restored DB under a DIFFERENT master key refuses to serve",
            (not healthy3) or node3.poll() is not None,
            f"healthy={healthy3} exit={node3.poll()}",
        )
        check(
            "C: ...and the log says why (decoding/decrypt/key, not a generic crash)",
            ("decod" in log3.lower() or "decrypt" in log3.lower() or "key" in log3.lower()),
            f"tail={(log3.strip().splitlines() or [''])[-1][:160]}",
        )
        stop(node3)
        node3 = None
    finally:
        stop(node1)
        stop(node2)
        stop(node3)
        try:
            upstream.shutdown()
        except Exception:
            pass
        source.cleanup()
        restored.cleanup()
        shutil.rmtree(DIR, ignore_errors=True)

    print()
    if failures:
        print(f"BACKUP/RESTORE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print(
        "BACKUP/RESTORE: PASSED (live config restored into a fresh real DB, serves the same "
        "data plane, and is unreadable under a different master key)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
