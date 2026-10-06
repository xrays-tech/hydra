#!/usr/bin/env python3
"""Does a dead provider come back ON ITS OWN — and does the documented probe blind spot bite?

`design.md` §8.4 promises: "恢复：后台探活任务每 `probe_interval`（默认 10s）对 dead provider 做
轻量探测（`GET {endpoint}/v1/models` 或 TCP 探活）；成功 → 移出 dead-set 并清零计数". A probe
task does exist (`breaker_wrap.rs::probe_task`, spawned in `main.rs`, 10 s interval) and it is
revival-by-`<500` on that ONE path — which `ops.md` §9.1 records as a **known blind spot**
("a revived-by-probe decision treats any response status `< 500` as healthy, and the probe hits
a path that a real inference request never uses ... recorded here rather than silently fixed").

Neither half has ever been observed on a live node. Round 102 showed a dead route STAYS dead
until `DELETE /api/v1/breaker/{id}` — but that provider never recovered, so revival was never
exercised. This drill exercises it, with one provider behind a mock whose answers can be
switched (so the symptom is not masked by failover):

  P0  control: a healthy provider serves the tenant
  P1  the promised AUTOMATIC revival: a recovered provider (its probe path answers again) leaves
      the dead-set with NO traffic and NO manual reset — and how long it takes
  P2  the blind spot, measured: a provider whose CHAT path is 100 % broken but whose
      `/v1/models` answers **404** is revived anyway, so real requests keep failing after a
      "successful" revival (this is the tripping/reviving flap an operator sees)
  P3  the other direction (no false revival): a provider answering **5xx** on the probe path
      stays dead — the code says reviving on any response made a 500-ing provider flap

Run: python3 integration/test_breaker_probe_revival.py    # needs target/debug/hydra
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

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "breaker-probe-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18830, 18831, 18839
TOKEN = "hydra-probe-admin-2026"
THRESHOLD = 5
PROBE_INTERVAL = 10          # design §8.4 / hydra_core::breaker::BreakerConfig::default
REVIVAL_BUDGET = PROBE_INTERVAL * 2.5

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


MODE = {"value": "ok"}          # switchable: "ok" | "all503" | "models404_chat503" | "all500"


class Upstream(BaseHTTPRequestHandler):
    """One provider whose probe path and chat path can fail independently."""

    def _payload(self, mode, path):
        if mode == "ok":
            if path.startswith("/v1/models"):
                return 200, json.dumps({"object": "list", "data": []}).encode()
            if path.startswith("/auth"):
                return 200, json.dumps({"allowed": True, "expires_in": 300}).encode()
            return 200, json.dumps({"id": "chatcmpl-probe", "object": "chat.completion",
                                    "choices": [{"index": 0,
                                                 "message": {"role": "assistant", "content": "ok"},
                                                 "finish_reason": "stop"}],
                                    "usage": {"prompt_tokens": 5, "completion_tokens": 8,
                                              "total_tokens": 13}}).encode()
        if mode == "all503":
            return 503, b'{"error":"upstream unavailable"}'
        if mode == "models404_chat503":
            if path.startswith("/v1/models"):
                return 404, b'{"error":"no such route"}'
            if path.startswith("/auth"):
                return 200, json.dumps({"allowed": True, "expires_in": 300}).encode()
            return 503, b'{"error":"upstream unavailable"}'
        if mode == "all500":
            return 500, b'{"error":"boom"}'
        return 200, b"{}"

    def do_GET(self):
        status, payload = self._payload(MODE["value"], self.path)
        self.send_response(status)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        status, payload = self._payload(MODE["value"], self.path)
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *a):
        pass


def call(method, url, token=None, body=None, timeout=30, host=None):
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
                host="probe.local",
                body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})


def dead_set():
    st, out = admin("GET", "/breaker")
    try:
        return json.loads(out).get("dead", [])
    except Exception:
        return [f"<unparseable HTTP {st}: {out[:60]}>"]


def wait_for(fn, budget, step=0.5):
    deadline = time.time() + budget
    while time.time() < deadline:
        if fn():
            return True
        time.sleep(step)
    return False


def start_node():
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'probe')}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "info",
    })
    log = open(os.path.join(DIR, "probe.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def stop(proc):
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed():
    rows = [
        ("providers", {"id": "p-trap", "key": "p-trap", "name": "TRAP",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p-trap",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p-trap", "api_key": "sk-up",
                           "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": "probe.local",
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p-trap",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[breaker-probe] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def trip():
    """Drive the provider into the dead-set; returns the statuses seen."""
    codes = [proxied()[0] for _ in range(THRESHOLD)]
    return codes


def main():
    if not os.path.exists(BIN):
        print(f"[breaker-probe] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    node = start_node()
    try:
        if not wait_for(lambda: admin("GET", "/health")[0] == 200, budget=25):
            print("[breaker-probe] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "probe.log"), errors="replace").read()[-700:],
                  file=sys.stderr)
            return 2
        seed()

        # ---- P0 control ---------------------------------------------------------
        st0, body0 = proxied()
        check("P0: the healthy provider serves the tenant (control)",
              st0 == 200 and "chatcmpl-probe" in body0, f"HTTP {st0} {body0[:60]}")

        # ---- P1: the promised automatic revival for a RECOVERED provider --------
        MODE["value"] = "all503"
        codes = trip()
        announce("P1 the tripping requests", f"codes={codes}")
        check("P1: a provider failing on both paths is taken out after 5 failures",
              "p-trap" in dead_set(), f"codes={codes} dead={dead_set()}")
        MODE["value"] = "ok"
        t0 = time.time()
        revived = wait_for(lambda: dead_set() == [], budget=REVIVAL_BUDGET)
        took = time.time() - t0
        st1, body1 = proxied()
        announce("P1 revival", f"dead-set empty={revived} after {took:.1f}s; next HTTP {st1}")
        check("P1: a RECOVERED provider leaves the dead-set ON ITS OWN — no traffic, no manual "
              "reset (design §8.4's probe promise)",
              revived, f"took {took:.1f}s (probe interval {PROBE_INTERVAL}s)")
        check("P1: ...and it serves traffic again", st1 == 200 and "chatcmpl-probe" in body1,
              f"HTTP {st1} {body1[:60]}")
        check("P1: ...within a couple of probe intervals",
              took <= REVIVAL_BUDGET, f"{took:.1f}s vs budget {REVIVAL_BUDGET:.0f}s")

        # ---- P2: the documented blind spot, measured ---------------------------
        MODE["value"] = "models404_chat503"
        codes2 = trip()
        check("P2: a provider whose CHAT path always fails is taken out",
              "p-trap" in dead_set(), f"codes={codes2} dead={dead_set()}")
        t0 = time.time()
        revived2 = wait_for(lambda: dead_set() == [], budget=REVIVAL_BUDGET)
        took2 = time.time() - t0
        codes3 = [proxied()[0] for _ in range(THRESHOLD)]
        announce("P2 the blind spot", f"revived={revived2} after {took2:.1f}s "
                 f"(its /v1/models answers 404, chat answers 503); then codes={codes3}")
        check("P2: ...but it is REVIVED anyway, because the probe hits `/v1/models` and treats "
              "any status < 500 as healthy (the blind spot §9.1 records)",
              revived2, f"revived after {took2:.1f}s while chat was still 100% broken")
        check("P2: ...so the very next requests fail again (trip → revive → trip: the flap an "
              "operator sees, with no way to tell it from a real recovery)",
              all(c != 200 for c in codes3), f"codes={codes3} dead={dead_set()}")

        # ---- P3: the other direction — 5xx on the probe path must NOT revive ----
        MODE["value"] = "all500"
        trip()
        check("P3: a provider answering 500 on BOTH paths is in the dead-set",
              "p-trap" in dead_set(), f"dead={dead_set()}")
        time.sleep(REVIVAL_BUDGET)
        announce("P3 after waiting one revival budget with no traffic", f"dead={dead_set()}")
        check("P3: ...and it STAYS dead: a 5xx on the probe path is not a revival (otherwise a "
              "500-ing provider would flap)",
              "p-trap" in dead_set(), f"dead={dead_set()}")
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"BREAKER PROBE REVIVAL: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("BREAKER PROBE REVIVAL: PASSED (automatic revival of a recovered provider, the "
          "measured 404 blind spot that revives a broken one, and no revival on 5xx)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
