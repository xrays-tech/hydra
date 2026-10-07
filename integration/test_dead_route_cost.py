#!/usr/bin/env python3
"""What a DEAD ROUTE costs with the shipped defaults — and does the breaker stop it?

Round 101 established that a connect which never completes now fails over instead of
surfacing as `502 upstream_transport_error`. That fix has a price, and the price is the
subject of this drill: every attempt on the dead provider now waits
`HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS` (**default 10 s**, the value an operator actually
runs) before failing over. `ops.md` §9.1 claims the breaker is what stops a provider from
"quietly leaving the rotation" (`hydra_candidate_skipped_total{reason="breaker_dead"}`), and
`breaker_wrap.rs` trips at **5** consecutive failures — but nobody has measured the two
together for THIS failure class (`test_breaker_lifecycle.py` uses a closed port, i.e. a
failure that returns instantly).

So, with the DEFAULT bounds (`connect = 10 s`, `first-byte = 30 s` — this drill sets
neither) and TEST-NET-1 (`192.0.2.1`, RFC 5737 — dropped by every router) as the dead route:

  D1 the requests that land on the dead provider are still SERVED (failover), and each one
     pays about the connect bound — this is the measured "dead route costs N seconds per
     affected request" number nobody had
  D2 the breaker TRIPS on those failures: `GET /api/v1/breaker` lists the provider, and the
     documented meter `hydra_candidate_skipped_total{reason="breaker_dead"}` starts moving
  D3 once it is in the dead-set the penalties STOP: later requests are fast again and the
     dead provider is no longer chosen (that is the breaker's whole purpose)
  D4 the documented manual reset (`DELETE /api/v1/breaker/{id}`) is a real lever: it clears
     the dead-set, so the cost can come back (an operator who resets without fixing the route
     buys the 10 s penalties again)
  D5 a tenant whose ONLY provider is dead fails in bounded time instead of hanging forever,
     and after the trip it fails FAST without paying the connect bound at all

Run: python3 integration/test_dead_route_cost.py    # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
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
DIR = os.path.join(ROOT, ".acceptance", "dead-route-cost-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18840, 18841, 18849
ADMIN2, DATA2 = ADMIN + 10, DATA + 10
TOKEN = "hydra-deadroute-admin-2026"
BLACK_HOLE = "192.0.2.1:9"
CONNECT_DEFAULT = 10          # seconds — the SHIPPED default, deliberately not overridden
THRESHOLD = 5                 # breaker_wrap.rs / design §8.4

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Upstream(BaseHTTPRequestHandler):
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
        if self.path.startswith("/auth"):
            payload = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            payload = json.dumps({"id": "chatcmpl-live", "object": "chat.completion",
                                  "choices": [{"index": 0,
                                               "message": {"role": "assistant", "content": "ok"},
                                               "finish_reason": "stop"}],
                                  "usage": {"prompt_tokens": 5, "completion_tokens": 8,
                                            "total_tokens": 13}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *a):
        pass


def call(method, url, token=None, body=None, timeout=90, host=None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if host:
        headers["Host"] = host
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace"), time.time() - started
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace"), time.time() - started
    except Exception as e:
        return 0, str(e), time.time() - started


def admin(method, path, body=None, port=ADMIN):
    st, out, _ = call(method, f"http://127.0.0.1:{port}/api/v1{path}", token=TOKEN, body=body)
    return st, out


def proxied(host, port=DATA, timeout=90):
    st, body, elapsed = call("POST", f"http://127.0.0.1:{port}/v1/chat/completions",
                             token="sk-tenant-1", host=host, timeout=timeout,
                             body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})
    return st, body, elapsed


def start_node(label, admin_port=ADMIN, data_port=DATA):
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin_port}",
        "HYDRA_LISTEN": f"127.0.0.1:{data_port}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "info",
    })
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(port=ADMIN, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health", port=port)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def dead_set(port=ADMIN):
    st, out = admin("GET", "/breaker", port=port)
    try:
        return json.loads(out).get("dead", [])
    except Exception:
        return [f"<unparseable: HTTP {st} {out[:60]}>"]


def metric_sum(port, name, needle=None):
    _, body, _ = call("GET", f"http://127.0.0.1:{port}/metrics", token=TOKEN)
    total = 0.0
    for line in body.splitlines():
        if line.startswith(name) and not line.startswith("#") and (needle is None or needle in line):
            try:
                total += float(line.rsplit(" ", 1)[1])
            except (ValueError, IndexError):
                pass
    return total


def provider(pid, endpoint, admin_port=ADMIN):
    return admin("POST", "/providers", {"id": pid, "key": pid, "name": pid.upper(),
                                        "endpoint": endpoint, "weight": 1,
                                        "created_at": "", "updated_at": ""}, port=admin_port)


def seed(dead_endpoint, dead_only=False, admin_port=ADMIN):
    rows = [("providers", {"id": "p-dead", "key": "p-dead", "name": "DEAD",
                           "endpoint": dead_endpoint, "weight": 1,
                           "created_at": "", "updated_at": ""}),
            ("provider-models", {"id": "pm-dead", "key": "echo", "name": "E", "provider_id": "p-dead",
                                 "status": 1, "created_at": "", "updated_at": ""}),
            ("provider-keys", {"id": "pk-dead", "provider_id": "p-dead", "api_key": "sk-dead",
                               "created_at": ""})]
    if not dead_only:
        rows += [("providers", {"id": "p-live", "key": "p-live", "name": "LIVE",
                                "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                                "created_at": "", "updated_at": ""}),
                 ("provider-models", {"id": "pm-live", "key": "echo", "name": "E",
                                      "provider_id": "p-live", "status": 1,
                                      "created_at": "", "updated_at": ""}),
                 ("provider-keys", {"id": "pk-live", "provider_id": "p-live", "api_key": "sk-live",
                                    "created_at": ""})]
    rows += [("tenants", {"id": "t1", "name": "T", "domain": "deadroute.local",
                          "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                          "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
             ("tenant-providers", {"id": "tp-dead", "tenant_id": "t1", "provider_id": "p-dead",
                                   "created_at": "", "updated_at": ""}),
             ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                                "created_at": "", "updated_at": ""})]
    if not dead_only:
        rows.append(("tenant-providers", {"id": "tp-live", "tenant_id": "t1",
                                          "provider_id": "p-live", "created_at": "",
                                          "updated_at": ""}))
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body, port=admin_port)
        if st not in (200, 201):
            raise SystemExit(f"[dead-route] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {}, port=admin_port)
    time.sleep(0.3)


def black_hole_confirmed():
    probe = socket.socket()
    probe.settimeout(2)
    try:
        probe.connect(("192.0.2.1", 9))
        return False
    except socket.timeout:
        return True
    except OSError:
        return False
    finally:
        probe.close()


def main():
    if not os.path.exists(BIN):
        print(f"[dead-route] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    if not black_hole_confirmed():
        print("[dead-route] CANNOT VERIFY: 192.0.2.1:9 is not a black hole on this host",
              file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    node = start_node("cost")
    try:
        if not wait_healthy():
            print("[dead-route] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "cost.log"), errors="replace").read()[-700:],
                  file=sys.stderr)
            return 2
        seed(f"http://{BLACK_HOLE}")

        # ---- D1/D2: the penalty per affected request, and the trip ---------------
        # The weight-1/weight-1 SWRR alternates, so the number of REQUESTS needed to reach the
        # documented failure threshold (5) is not bounded by the threshold itself — drive until
        # the breaker actually trips, with a budget. (The first version stopped after 7
        # requests, i.e. after FOUR dead hits, and then reported "the breaker never trips": a
        # harness bug, not a product finding.)
        rows = []
        penalties = 0
        tripped_after = None
        for i in range(1, 16):
            st, body, elapsed = proxied("deadroute.local")
            hit_dead = elapsed >= CONNECT_DEFAULT - 2.0
            rows.append((st, round(elapsed, 1), hit_dead))
            if hit_dead:
                penalties += 1
            dead = dead_set()
            announce(f"D1 request {i}", f"HTTP {st} in {elapsed:.1f}s dead-hit={hit_dead} "
                     f"failures-so-far={penalties} dead-set={dead}")
            if "p-dead" in dead:
                tripped_after = penalties
                break
        penalised = [el for _, el, hit in rows if hit]
        announce("D1 every request", f"{[(st, el) for st, el, _ in rows]}")
        check("D1: the requests are still SERVED while the dead route is chosen (failover)",
              all(st == 200 for st, _, _ in rows), f"codes={[st for st, _, _ in rows]}")
        check(f"D1: ...and each one that lands on the dead provider pays about the shipped "
              f"connect bound ({CONNECT_DEFAULT}s) — the measured cost of a dead route",
              penalties >= THRESHOLD
              and all(5.0 <= el <= CONNECT_DEFAULT + 3 for el in penalised),
              f"dead hits={penalties} latencies={penalised}")
        check(f"D1: ...and it costs exactly the documented {THRESHOLD} consecutive failures to "
              f"take the provider out of the rotation",
              tripped_after == THRESHOLD,
              f"tripped after {tripped_after} failures (threshold {THRESHOLD})")
        check("D2: the breaker TRIPS on those failures (the whole purpose: stop choosing a "
              "provider that cannot be reached)",
              "p-dead" in dead_set(), f"dead-set={dead_set()} after {len(rows)} requests")

        # ---- D3: the penalties stop ---------------------------------------------
        after = []
        for _ in range(4):
            st, _, elapsed = proxied("deadroute.local")
            after.append((st, round(elapsed, 1)))
        announce("D3 the four requests after the trip", f"{after}")
        skipped = metric_sum(ADMIN, "hydra_candidate_skipped_total", 'reason="breaker_dead"')
        check("D2: the documented meter for 'it left the rotation' is moving "
              "(`hydra_candidate_skipped_total{reason=\"breaker_dead\"}`)",
              skipped >= 1.0, f"skipped={skipped}")
        check("D3: once the provider is in the dead-set the penalties STOP — every request is "
              "served fast again (that is what the breaker is for)",
              all(st == 200 and el < 2.0 for st, el in after), f"{after}")

        # ---- D4: the manual reset is a real lever -------------------------------
        st_r, out_r, _ = call("DELETE", f"http://127.0.0.1:{ADMIN}/api/v1/breaker/p-dead",
                              token=TOKEN)
        announce("D4 the reset", f"HTTP {st_r} {out_r[:110]}")
        check("D4: `DELETE /api/v1/breaker/{id}` clears the dead-set (documented operator lever)",
              st_r == 200 and dead_set() == [], f"dead-set={dead_set()}")
        # SWRR alternates, so a single request may land on the healthy provider: look at a few
        # and require the penalty to come BACK (the reset is honest, not cosmetic).
        back = [(st, round(el, 1)) for st, _, el in
                ((lambda r: (r[0], None, r[2]))(proxied("deadroute.local")) for _ in range(3))]
        announce("D4 the requests after the reset", f"{back}")
        check("D4: ...so an operator who resets WITHOUT fixing the route buys the connect-bound "
              "penalty again (the reset is honest, not cosmetic)",
              any(st == 200 and el >= CONNECT_DEFAULT - 2.0 for st, el in back), f"{back}")
    finally:
        stop(node)

    # ---- D5: a tenant whose ONLY provider is dead ------------------------------
    node2 = start_node("deadonly", ADMIN2, DATA2)
    try:
        if not wait_healthy(ADMIN2):
            check("D5: the second node became healthy", False, "never healthy")
        else:
            seed(f"http://{BLACK_HOLE}", dead_only=True, admin_port=ADMIN2)
            st1, body1, el1 = proxied("deadroute.local", port=DATA2)
            announce("D5 the first request (nothing healthy to fail over to)",
                     f"HTTP {st1} in {el1:.1f}s {body1[:80]}")
            check("D5: fails in BOUNDED time (about the connect bound) instead of hanging",
                  st1 in (502, 503, 504) and el1 <= CONNECT_DEFAULT + 5,
                  f"HTTP {st1} in {el1:.1f}s")
            for _ in range(THRESHOLD):
                proxied("deadroute.local", port=DATA2)
            st2, body2, el2 = proxied("deadroute.local", port=DATA2)
            announce("D5 after the trip", f"HTTP {st2} in {el2:.1f}s dead={dead_set(ADMIN2)}")
            check("D5: ...and after the breaker trips it fails FAST — the connect bound is not "
                  "paid at all once the provider is out of the rotation",
                  st2 in (502, 503, 504) and el2 < 2.0,
                  f"HTTP {st2} in {el2:.1f}s (dead-set={dead_set(ADMIN2)})")
    finally:
        stop(node2)
        upstream.shutdown()

    print()
    if failures:
        print(f"DEAD ROUTE COST: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("DEAD ROUTE COST: PASSED (the per-request cost of a dead route with shipped "
          "defaults, the breaker trip that stops it, the manual reset as a real lever, and "
          "bounded failure when there is nothing to fail over to)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
