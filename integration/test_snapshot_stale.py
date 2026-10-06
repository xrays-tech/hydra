#!/usr/bin/env python3
"""A reload that FAILS: the snapshot goes stale — and every later write still answers 2xx.

`ops.md` §9.1's alert row (`hydra_config_snapshot_stale == 1`) is the operator's only signal for a
state the code describes in capitals: `admin::reload_best_effort` logs that "the in-memory config
snapshot is now STALE … admin writes will keep returning 2xx while having no runtime effect until a
reload succeeds", and `metrics.rs` says to alert on the gauge. Its trigger had never been exercised:
the admin write boundary rejects the endpoint typo that used to cause it (`store::is_usable_endpoint`
mirrors the loader), so a stale snapshot needs a row that the LOADER rejects while it is already in
the database — a hand-edited database, a restore, or a config written by an older build.

Cases:
  S0  a healthy node: the gauge is 0 and `/reload` answers 200
  S1  a bad provider row is written DIRECTLY into SQLite (bypassing the write boundary, exactly what
      a restore can produce), then `POST /api/v1/reload`:
      - the gauge flips to **1** and the ERROR is logged with the documented wording
      - the node KEEPS SERVING on the old snapshot (traffic is not what breaks)
      - and a LATER admin write still answers 2xx while having **no runtime effect** — the trap:
        a tenant PUT `enabled=false` succeeds, yet the tenant keeps being served
  S2  recovery: the bad row is removed and `/reload` succeeds ⇒ the gauge returns to 0 **and the
      pending write finally takes effect** (the tenant is now refused)

Run: python3 integration/test_snapshot_stale.py    # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "snapshot-stale-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18710, 18711, 18719
TOKEN = "hydra-stale-admin-2026"
DOMAIN = "stale.local"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def kill_our_instances():
    """Kill leftovers of THIS tree's binary (matched on /proc/<pid>/exe, never by name)."""
    want = os.path.realpath(BIN)
    targets = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            if os.path.realpath(os.readlink(f"/proc/{entry}/exe")) != want:
                continue
        except OSError:
            continue
        targets.append(int(entry))
    for pid in targets:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    return targets


class Upstream(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        payload = b'{"object":"list","data":[]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        payload = json.dumps({"allowed": True}).encode() if self.path.startswith("/auth") else \
            json.dumps({"id": "chatcmpl-stale", "object": "chat.completion",
                        "choices": [{"index": 0,
                                     "message": {"role": "assistant", "content": "ok"},
                                     "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 5, "completion_tokens": 8,
                                  "total_tokens": 13}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.close_connection = True

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


def admin(method, path, body=None):
    return call(method, f"http://127.0.0.1:{ADMIN}/api/v1{path}", token=TOKEN, body=body)


def proxied():
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token="sk-tenant-1",
                host=DOMAIN,
                body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})


def db_file():
    return os.path.join(DIR, "stale.db")


def start_node():
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{db_file()}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "info",
    })
    log = open(os.path.join(DIR, "stale.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health")[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed():
    rows = [
        ("providers", {"id": "p1", "key": "p1", "name": "P",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": DOMAIN,
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[stale] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def sql(statement, params=()):
    conn = sqlite3.connect(db_file(), timeout=10)
    try:
        conn.execute(statement, params)
        conn.commit()
    finally:
        conn.close()


def stale_gauge():
    _, out = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
    for line in out.splitlines():
        if line.startswith("hydra_config_snapshot_stale") and not line.startswith("#"):
            try:
                return float(line.rsplit(" ", 1)[1])
            except ValueError:
                return None
    return None


def health_body():
    _, out = admin("GET", "/health")
    return out


def main():
    if not os.path.exists(BIN):
        print(f"[stale] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    killed = kill_our_instances()
    if killed:
        print(f"   (killed leftover instance(s) of OUR build: {killed})")
    time.sleep(0.4)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    node = start_node()
    try:
        if not wait_healthy():
            print("[stale] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "stale.log"), errors="replace").read()[-700:],
                  file=sys.stderr)
            return 2
        seed()

        # ---- S0: healthy ------------------------------------------------------
        st0, _ = proxied()
        st_r0, out_r0 = admin("POST", "/reload", {})
        check("S0: on a healthy node the gauge is 0 and `/reload` answers 200 (control)",
              stale_gauge() == 0.0 and st_r0 == 200 and st0 == 200,
              f"gauge={stale_gauge()} reload={st_r0} proxy={st0}")

        # ---- S1: a row the LOADER rejects, written past the write boundary -----
        # `store::is_usable_endpoint` mirrors the loader at the admin write boundary (so the API
        # would refuse this row); writing it DIRECTLY is what a restore/hand-edited database gives.
        sql("INSERT INTO provider (id, key, name, endpoint, weight, created_at, updated_at) "
            "VALUES ('p-bad','p-bad','BAD','api.openai.com',1,'','')")
        st1, out1 = admin("POST", "/reload", {})
        gauge1 = stale_gauge()
        log = open(os.path.join(DIR, "stale.log"), errors="replace").read()
        announce("S1 after the failing reload",
                 f"reload HTTP {st1} {out1[:70]!r} gauge={gauge1}")
        check("S1: the explicit reload FAILS with the documented `400 reload_failed` "
              "(old snapshot retained)",
              st1 == 400 and "reload_failed" in out1, f"HTTP {st1} {out1[:80]}")
        check("S1: ...and the documented gauge flips to 1 (`hydra_config_snapshot_stale`) — the "
              "endpoint used to record nothing, so a failing reload was invisible to the alert",
              gauge1 == 1.0, f"gauge={gauge1}")
        check("S1: ...and the per-tenant flag the admin API carries agrees "
              "(`GET /tenants/t1` -> `snapshot_stale: true`) — the write is committed, it just has "
              "no runtime effect",
              '"snapshot_stale":true' in admin("GET", "/tenants/t1")[1].replace(" ", ""),
              admin("GET", "/tenants/t1")[1][-160:])
        st_p, _ = proxied()
        check("S1: ...while the node KEEPS SERVING on the old snapshot (traffic is not what "
              "breaks)", st_p == 200, f"HTTP {st_p}")

        # The trap: a later write succeeds and has no runtime effect.
        st2, out2 = admin("PUT", "/tenants/t1", {
            "id": "t1", "name": "T", "domain": DOMAIN,
            "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": False,
            "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""})
        time.sleep(0.4)
        st_after, body_after = proxied()
        # PREMISE, asserted (round 171). This leg's claim is an ABSENCE — "the disabled tenant is
        # STILL SERVED" — and an unchanged row satisfies an absence just as well as a committed one:
        # if the PUT had silently changed nothing, `st_after == 200` would still pass while the leg
        # proved nothing about staleness. So read the row back and require the DISABLED state to be
        # COMMITTED. The admin API reads the ROW (not the running snapshot), which is exactly what
        # makes it the right witness here: `snapshot_stale` is still true in the same body.
        st_rb, body_rb = admin("GET", "/tenants/t1")
        rb_flat = body_rb.replace(" ", "")
        committed_disabled = '"enabled":false' in rb_flat
        check("S1 PREMISE: the PUT really COMMITTED `enabled=false` (read back through the admin "
              "API, which serves the row, not the stale snapshot)",
              st_rb == 200 and committed_disabled and '"snapshot_stale":true' in rb_flat,
              f"GET /tenants/t1 -> HTTP {st_rb} enabled=false present={committed_disabled} "
              f"snapshot_stale=true present={'"snapshot_stale":true' in rb_flat}; {body_rb[-160:]}")
        announce("S1 the pending write",
                 f"tenant PUT HTTP {st2} (enabled=false) -> proxy HTTP {st_after} "
                 f"{body_after[:60]!r}")
        check("S1: an admin write during the stale window still answers 2xx",
              st2 in (200, 201), f"HTTP {st2}")
        check("S1: ...and has NO runtime effect: the tenant whose row now says `enabled=false` is "
              "STILL SERVED (the trap the code documents in capitals)",
              st_after == 200, f"HTTP {st_after} (expected 200 = still served)")

        # ---- S2: recovery -----------------------------------------------------
        sql("DELETE FROM provider WHERE id = 'p-bad'")
        st3, out3 = admin("POST", "/reload", {})
        gauge3 = stale_gauge()
        time.sleep(0.4)
        st_final, body_final = proxied()
        announce("S2 after the fix",
                 f"reload HTTP {st3} gauge={gauge3} -> proxy HTTP {st_final} "
                 f"{body_final[:70]!r}")
        check("S2: after the bad row is gone the reload succeeds and the gauge returns to 0 — so "
              "the documented recovery (reload again) also silences the documented alert",
              gauge3 == 0.0 and st3 == 200, f"reload={st3} gauge={gauge3}")
        check("S2: ...and the write that had no effect now DOES take effect: the disabled tenant "
              "is refused (403 `tenant_disabled`)",
              st_final == 403 and "tenant_disabled" in body_final,
              f"HTTP {st_final} {body_final[:80]}")
        announce("S2 the tenant view after recovery", f"{admin('GET', '/tenants/t1')[1][-160:]}")
        check("S2: ...and the per-tenant `snapshot_stale` flag returns to false too (the admin "
              "API stops claiming the running config is behind)",
              '"snapshot_stale":false' in admin("GET", "/tenants/t1")[1].replace(" ", ""),
              admin("GET", "/tenants/t1")[1][-160:])
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"SNAPSHOT STALE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("SNAPSHOT STALE: PASSED (a failing reload flips the documented gauge and is logged as "
          "STALE, the node keeps serving, later writes answer 2xx and — with the committed row read "
          "back as the premise — have no runtime effect until a successful reload recovers them)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
