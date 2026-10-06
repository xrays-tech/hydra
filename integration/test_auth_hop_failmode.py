#!/usr/bin/env python3
"""The AUTH hop when the tenant's `auth_url` cannot be reached — and the fail-mode nobody can select.

Every other hop has been measured against a dead route by now (the provider in rounds 101/102, the
Redis bus in round 99). The tenant `auth_url` has not, and it is the one hop where a failure
decides whether a request is served or refused: `hydra-server/src/http.rs` classifies a 401/403 as
a **cached denial**, but a transport failure, a 5xx or an unparseable body goes through
`fail_mode_verdict(fail_mode)` — "**never cache**", `FailMode::Closed` (the default) ⇒ `503
auth_upstream_unavailable`.

Two things are worth measuring there, and one of them looks like a ghost:

  A0  control: a healthy auth service serves the request
  A1  a BLACK-HOLED `auth_url` (TEST-NET-1: SYN dropped, nothing on the network is contacted):
      the request must fail in bounded time with the documented 503, the provider must NEVER be
      called, and the failure must be attributed in the metrics
  A2  that failure is NOT cached (the code says so): every following request pays the round-trip
      timeout again — the measured per-request cost of a dead auth service
  A3  CONTRAST: a REFUSED port (nothing listening) gives the same 503 in milliseconds
  A4  recovery: as soon as the auth service answers again the very next request is served (no
      stale negative verdict)
  A5  `fail_mode`: `AuthConfig.fail_mode` defaults to `Closed` and `main.rs` takes
      `..AuthConfig::default()`, while `FailMode::Open` (documented in `design.md` §11.4 as the
      `[auth] fail_mode` knob, "availability-first") exists and is fully implemented. If no
      environment variable selects it, then the availability-first mode is UNREACHABLE — a
      configuration an operator cannot turn on. Measured by trying the plausible names.

Run: python3 integration/test_auth_hop_failmode.py    # needs target/debug/hydra
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

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "auth-hop-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18780, 18781, 18789
TOKEN = "hydra-authhop-admin-2026"
DOMAIN = "authhop.local"
BLACK_HOLE = "192.0.2.1:9"          # RFC 5737 TEST-NET-1: dropped by every router
DEAD_PORT = 18779                    # nothing listens here: instant connection refused
ROUND_TRIP_TIMEOUT = 2.0             # AuthConfig::timeout default (2000 ms)

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Received:
    def __init__(self):
        self.lock = threading.Lock()
        self.chat_calls = 0
        self.auth_calls = 0

    def record_chat(self):
        with self.lock:
            self.chat_calls += 1

    def record_auth(self):
        with self.lock:
            self.auth_calls += 1

    def snapshot(self):
        with self.lock:
            return self.chat_calls, self.auth_calls


RECEIVED = Received()
AUTH_MODE = {"value": "ok"}         # "ok" | "error"


class Upstream(BaseHTTPRequestHandler):
    """Plays both the tenant auth service and the provider endpoint."""

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
        if self.path.startswith("/auth"):
            RECEIVED.record_auth()
            if AUTH_MODE["value"] == "error":
                payload = b"<html>503 from a WAF</html>"
                status = 503
            else:
                payload = json.dumps({"allowed": True, "expires_in": 300}).encode()
                status = 200
        else:
            RECEIVED.record_chat()
            payload = json.dumps({"id": "chatcmpl-authhop", "object": "chat.completion",
                                  "choices": [{"index": 0,
                                               "message": {"role": "assistant", "content": "ok"},
                                               "finish_reason": "stop"}],
                                  "usage": {"prompt_tokens": 5, "completion_tokens": 8,
                                            "total_tokens": 13}}).encode()
            status = 200
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.close_connection = True

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
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace"), time.time() - started
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace"), time.time() - started
    except Exception as e:
        return 0, str(e), time.time() - started


def admin(method, path, body=None):
    st, out, _ = call(method, f"http://127.0.0.1:{ADMIN}/api/v1{path}", token=TOKEN, body=body)
    return st, out


def proxied(key="sk-authhop-1", timeout=30):
    st, body, elapsed = call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token=key,
                             host=DOMAIN, timeout=timeout,
                             body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})
    return st, body, elapsed


def start_node(extra=None, db="authhop"):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, db)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "info",
    })
    env.update(extra or {})
    log = open(os.path.join(DIR, f"{db}.log"), "w")
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


def put_tenant(auth_url):
    return admin("PUT", "/tenants/t1", {
        "id": "t1", "name": "T", "domain": DOMAIN, "auth_url": auth_url, "enabled": True,
        "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""})


def seed(auth_url):
    rows = [
        ("providers", {"id": "p1", "key": "p1", "name": "P",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                           "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": DOMAIN, "auth_url": auth_url,
                     "enabled": True, "cert_key": None, "cert_file": None,
                     "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[authhop] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def metric_lines(name, needle=None):
    _, out, _ = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
    return [l for l in out.splitlines()
            if l.startswith(name) and not l.startswith("#") and (needle is None or needle in l)]


def black_hole_is_dropped():
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
        print(f"[authhop] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    if not black_hole_is_dropped():
        print("[authhop] CANNOT VERIFY: 192.0.2.1:9 is not a black hole on this host",
              file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    node = start_node()
    try:
        if not wait_healthy():
            print("[authhop] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "authhop.log"), errors="replace").read()[-600:],
                  file=sys.stderr)
            return 2
        seed(f"http://127.0.0.1:{UPSTREAM}/auth")

        # ---- A0 control --------------------------------------------------------
        st0, _, el0 = proxied("sk-authhop-ok")
        chat0, auth0 = RECEIVED.snapshot()
        check("A0: a healthy auth service serves the request (control)",
              st0 == 200 and chat0 == 1 and auth0 == 1,
              f"HTTP {st0} in {el0:.2f}s, auth calls={auth0}, chat calls={chat0}")

        # ---- A1: a black-holed auth_url ---------------------------------------
        st_put, out_put = put_tenant(f"http://{BLACK_HOLE}/auth")
        admin("POST", "/reload", {})
        time.sleep(0.2)
        before_chat = RECEIVED.snapshot()[0]
        errs_before = metric_lines("hydra_auth_upstream_error_total")
        st1, body1, el1 = proxied("sk-authhop-deadroute")
        chat1 = RECEIVED.snapshot()[0]
        announce("A1 the black-holed auth hop",
                 f"HTTP {st1} in {el1:.2f}s body={body1[:80]!r} provider calls +{chat1 - before_chat}")
        check("A1: an unreachable auth service gives the documented 503 "
              "`auth_upstream_unavailable`",
              st1 == 503 and "auth_upstream_unavailable" in body1, f"HTTP {st1} {body1[:90]}")
        check("A1: ...in BOUNDED time (the documented 2000 ms round-trip timeout), not hanging",
              ROUND_TRIP_TIMEOUT - 0.7 <= el1 <= ROUND_TRIP_TIMEOUT + 3.0,
              f"{el1:.2f}s (documented timeout {ROUND_TRIP_TIMEOUT}s)")
        check("A1: ...and the provider is NEVER called (an unauthenticated request must not be "
              "forwarded)",
              chat1 == before_chat, f"provider calls +{chat1 - before_chat}")
        errs_after = metric_lines("hydra_auth_upstream_error_total")
        check("A1: ...and the failure is attributed in `hydra_auth_upstream_error_total`",
              len(errs_after) > len(errs_before) and any(f'tenant="t1"' in l for l in errs_after),
              f"{errs_after}")

        # ---- A2: not cached — every request pays it again ----------------------
        # The SAME key for all three: the property under test is "the outage is not cached", and
        # a rotating key could never hit a cache entry anyway — the first version did exactly
        # that and therefore could not fail when the outage WAS cached (measured by planting the
        # cache write: the leg stayed green).
        times = []
        for _ in range(3):
            st_i, _, el_i = proxied("sk-authhop-same-key")
            times.append((st_i, round(el_i, 2)))
        announce("A2 three more requests against the dead auth service", f"{times}")
        check("A2: a transport failure is NOT cached (the code says `never cache`): each request "
              "pays the round-trip timeout again — that is the measured cost of a dead auth "
              "service",
              len(times) == 3 and all(st == 503 for st, _ in times)
              and all(el >= ROUND_TRIP_TIMEOUT - 0.7 for _, el in times),
              f"{times}")

        # ---- A3: contrast — a refused port costs nothing -----------------------
        put_tenant(f"http://127.0.0.1:{DEAD_PORT}/auth")
        admin("POST", "/reload", {})
        time.sleep(0.2)
        st3, body3, el3 = proxied("sk-authhop-refused")
        announce("A3 the refused auth port", f"HTTP {st3} in {el3:.3f}s body={body3[:60]!r}")
        check("A3: a REFUSED auth port gives the same 503 in milliseconds (the timeout is only "
              "paid when the peer is unreachable, not when it says no)",
              st3 == 503 and el3 < 0.5, f"HTTP {st3} in {el3:.3f}s")

        # ---- A4: recovery ------------------------------------------------------
        put_tenant(f"http://127.0.0.1:{UPSTREAM}/auth")
        admin("POST", "/reload", {})
        time.sleep(0.2)
        # ...and the same key that was refused during the outage must be SERVED now: a cached
        # negative verdict would keep it blocked for the deny TTL.
        st4, _, el4 = proxied("sk-authhop-same-key")
        check("A4: as soon as the auth service answers again the very next request is served (no "
              "stale negative verdict was cached during the outage)",
              st4 == 200, f"HTTP {st4} in {el4:.2f}s")

        # ---- A5: can fail-open be selected at all? ----------------------------
        put_tenant(f"http://127.0.0.1:{DEAD_PORT}/auth")
        admin("POST", "/reload", {})
        time.sleep(0.2)
        unreachable_mode = True
        for name in ("HYDRA_AUTH_FAIL_MODE", "HYDRA_FAIL_MODE", "HYDRA_AUTH_FAILMODE"):
            stop(node)
            node = start_node({name: "open"})
            if not wait_healthy():
                check(f"A5: the node with {name}=open became healthy", False, "never healthy")
                continue
            st5, body5, _ = proxied("sk-authhop-failmode")
            announce(f"A5 {name}=open with an unreachable auth service",
                     f"HTTP {st5} {body5[:60]!r}")
            if st5 == 200:
                unreachable_mode = False
            put_tenant(f"http://127.0.0.1:{DEAD_PORT}/auth")
            admin("POST", "/reload", {})
        check("A5: `FailMode::Open` (implemented, and documented in design.md §11.4 as the "
              "`[auth] fail_mode` knob) cannot be selected by any environment variable — the "
              "availability-first mode is UNREACHABLE in this build",
              unreachable_mode,
              "no env var changed the 503 (tried HYDRA_AUTH_FAIL_MODE / HYDRA_FAIL_MODE / "
              "HYDRA_AUTH_FAILMODE)")
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"AUTH HOP FAIL-MODE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("AUTH HOP FAIL-MODE: PASSED (a dead auth route is bounded, never forwarded, attributed "
          "and NOT cached; a refused port is instant; recovery is immediate; and the documented "
          "fail-open mode is unreachable — recorded as a finding)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
