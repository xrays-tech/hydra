#!/usr/bin/env python3
"""An upstream whose CONNECT never completes — does the request fail over, or fail?

`ops.md` §9.1 documents `hydra_upstream_first_byte_timeout_total` as "the upstream **accepted
the connection** and then sent no response headers", and `proxy.rs` decides failover from the
same distinction: only a **connect error** proves the upstream never saw the request
(`never_reached_upstream`), while a first-byte timeout means "the request was written", so
replaying it could double-bill and the client gets `502 upstream_transport_error` instead of
another provider.

But `ProviderClient::send` wraps the WHOLE `req.send()` — connect included — in the
first-byte bound, and the client is built with **no `connect_timeout`**. So a provider whose
SYN goes nowhere (dead route, security-group change, black-holed AZ) burns the whole
first-byte bound and is then classified as a POST-SEND failure: the gateway refuses to fail
over to a healthy provider that is sitting right there. This drill measures that, using
TEST-NET-1 (`192.0.2.1`, RFC 5737 — reserved for documentation, dropped by every router) as
the black hole, so nothing on the network is actually contacted.

Cases:
  U0  control: a healthy provider answers 200
  U1  the SAME model is offered by the healthy provider and by a black-holed one: with
      weighted round-robin, every request must still answer 200 — the dead route must be
      failed over, not surfaced
  U2  the attribution: a connect that never completes must NOT be reported as a first-byte
      timeout (§9.1's stated meaning), and the failover must be counted
  U3  the bound really bounds: each attempt costs at most the connect bound, and the whole
      request stays well inside it
  U4  CONTRAST — an upstream that ACCEPTS the connection and then says nothing must stay a
      first-byte timeout with NO failover (the documented double-billing policy): the two
      classes must not be collapsed by the fix
  U5  a connect bound that cannot work (`>=` the first-byte bound, so the first-byte timeout
      always wins) is refused at startup instead of silently doing nothing

Run: python3 integration/test_upstream_connect_bound.py   # needs target/debug/hydra
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
DIR = os.path.join(ROOT, ".acceptance", "upstream-connect-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM, SILENT = 18850, 18851, 18859, 18858
TOKEN = "hydra-connect-admin-2026"
# RFC 5737 TEST-NET-1: reserved for documentation, never routed. A connect here hangs.
BLACK_HOLE = "192.0.2.1:9"
FIRST_BYTE = 3          # seconds: keep the drill fast, the bound is what matters
CONNECT = 2             # seconds: must be < FIRST_BYTE to take effect

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Upstream(BaseHTTPRequestHandler):
    """The healthy provider: any key, a normal chat completion."""

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


class SilentUpstream(BaseHTTPRequestHandler):
    """Accepts the connection and never answers — the ORIGINAL documented symptom."""

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        time.sleep(30)

    def log_message(self, *a):
        pass


def call(method, url, token=None, body=None, timeout=40, host=None):
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


def proxied(model="echo", host="connect.local", timeout=40, port=DATA):
    st, body, elapsed = call("POST", f"http://127.0.0.1:{port}/v1/chat/completions",
                             token="sk-tenant-1", host=host, timeout=timeout,
                             body={"model": model, "messages": [{"role": "user", "content": "hi"}]})
    return st, body, elapsed


def start_node(label, extra, admin_port=ADMIN, data_port=DATA):
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin_port}",
        "HYDRA_LISTEN": f"127.0.0.1:{data_port}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "info",
    })
    env.update(extra)
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


def metric(port, name, needle=None):
    _, body, _ = call("GET", f"http://127.0.0.1:{port}/metrics", token=TOKEN)
    return [l for l in body.splitlines()
            if l.startswith(name) and not l.startswith("#") and (needle is None or needle in l)]


def metric_sum(port, name, needle=None):
    total = 0.0
    for line in metric(port, name, needle):
        try:
            total += float(line.rsplit(" ", 1)[1])
        except (ValueError, IndexError):
            pass
    return total


def seed(dead_endpoint, silent=False):
    rows = [
        ("providers", {"id": "p-live", "key": "p-live", "name": "LIVE",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("providers", {"id": "p-dead", "key": "p-dead", "name": "DEAD",
                       "endpoint": dead_endpoint, "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm-live", "key": "echo", "name": "E", "provider_id": "p-live",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm-dead", "key": "echo", "name": "E", "provider_id": "p-dead",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk-live", "provider_id": "p-live", "api_key": "sk-live",
                           "created_at": ""}),
        ("provider-keys", {"id": "pk-dead", "provider_id": "p-dead", "api_key": "sk-dead",
                           "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": "connect.local",
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp-live", "tenant_id": "t1", "provider_id": "p-live",
                              "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp-dead", "tenant_id": "t1", "provider_id": "p-dead",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[connect-bound] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[connect-bound] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    # The black hole must really be a black hole on THIS host before anything is concluded:
    # a refused or unreachable route would produce a fast connect ERROR (a different case).
    probe = socket.socket()
    probe.settimeout(2)
    t0 = time.time()
    try:
        probe.connect(("192.0.2.1", 9))
        print("[connect-bound] CANNOT VERIFY: 192.0.2.1:9 accepted a connection; it is not a "
              "black hole here", file=sys.stderr)
        return 2
    except socket.timeout:
        pass                      # hung: exactly what is needed
    except OSError as e:
        print(f"[connect-bound] CANNOT VERIFY: 192.0.2.1:9 failed fast ({e}); this host does "
              f"not black-hole TEST-NET-1", file=sys.stderr)
        return 2
    finally:
        probe.close()
    print(f"== black hole confirmed: connect to 192.0.2.1:9 still hanging after "
          f"{time.time()-t0:.1f}s")

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    silent = ThreadingHTTPServer(("127.0.0.1", SILENT), SilentUpstream)
    silent.daemon_threads = True
    threading.Thread(target=silent.serve_forever, daemon=True).start()

    node = start_node("connect", {
        "HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS": str(FIRST_BYTE),
        "HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS": str(CONNECT),
    })
    try:
        if not wait_healthy():
            print("[connect-bound] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "connect.log"), errors="replace").read()[-700:],
                  file=sys.stderr)
            return 2
        seed(f"http://{BLACK_HOLE}")

        # ---- U0/U1: both providers serve `echo`; every request must be a 200 ----
        codes, latencies = [], []
        for _ in range(6):
            st, body, elapsed = proxied()
            codes.append(st)
            latencies.append(elapsed)
        announce("U0/U1 the six requests", f"codes={codes} "
                 f"latencies={[round(x, 2) for x in latencies]}")
        # The control is "the healthy provider serves when chosen", not "the first request
        # succeeds": weighted round-robin may pick the dead provider first, and that IS the
        # failure U1 measures.
        st_live, body_live, _ = proxied()
        check("U0: the healthy provider serves the tenant (control)",
              200 in codes and "chatcmpl-live" in body_live,
              f"codes={codes} live-body={body_live[:60]}")
        check("U1: a request offered by a black-holed provider and a healthy one still "
              "answers 200 — a connect that never completes must FAIL OVER, not surface",
              all(c == 200 for c in codes), f"codes={codes}")
        if not all(c == 200 for c in codes):
            _, body, _ = proxied()
            announce("U1 the body of a failing attempt", body[:160])

        # ---- U2: attribution -------------------------------------------------
        fb = metric_sum(ADMIN, "hydra_upstream_first_byte_timeout_total", 'provider="p-dead"')
        retries = metric_sum(ADMIN, "hydra_retries_total")
        announce("U2 the metrics", f"first_byte_timeouts(p-dead)={fb} retries={retries}")
        check("U2: a connect that never completed is NOT reported as a first-byte timeout "
              "(§9.1 defines that counter as 'accepted the connection and then sent no "
              "headers')", fb == 0.0, f"got {fb}")
        check("U2: ...and the failover to the healthy provider is counted as a retry",
              retries >= 1.0, f"retries={retries}")
        # MUST discriminate: a dead attempt has to end BEFORE the first-byte bound, which is
        # only possible if the connect bound is what fired. (`< first_byte + 2` passed even
        # with the bound removed — the attempt then takes exactly the first-byte bound.)
        check("U3: every attempt ends BEFORE the first-byte bound, i.e. the connect bound is "
              "what fired",
              bool(latencies) and max(latencies) < FIRST_BYTE - 0.5,
              f"max={max(latencies) if latencies else None:.2f}s vs first-byte bound "
              f"{FIRST_BYTE}s (connect bound {CONNECT}s)")
    finally:
        stop(node)

    # ---- U4: the contrast — connected, then silent (the documented symptom) ----
    node2 = start_node("silent", {
        "HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS": str(FIRST_BYTE),
        "HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS": str(CONNECT),
    }, admin_port=ADMIN + 10, data_port=DATA + 10)
    try:
        if not wait_healthy(ADMIN + 10):
            check("U4: the second node became healthy", False, "never healthy")
        else:
            rows = [
                ("providers", {"id": "p-live", "key": "p-live", "name": "LIVE",
                               "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                               "created_at": "", "updated_at": ""}),
                ("providers", {"id": "p-mute", "key": "p-mute", "name": "MUTE",
                               "endpoint": f"http://127.0.0.1:{SILENT}", "weight": 1,
                               "created_at": "", "updated_at": ""}),
                ("provider-models", {"id": "pm-live", "key": "echo", "name": "E",
                                     "provider_id": "p-live", "status": 1,
                                     "created_at": "", "updated_at": ""}),
                ("provider-models", {"id": "pm-mute", "key": "echo", "name": "E",
                                     "provider_id": "p-mute", "status": 1,
                                     "created_at": "", "updated_at": ""}),
                ("provider-keys", {"id": "pk-live", "provider_id": "p-live", "api_key": "sk-live",
                                   "created_at": ""}),
                ("provider-keys", {"id": "pk-mute", "provider_id": "p-mute", "api_key": "sk-mute",
                                   "created_at": ""}),
                ("tenants", {"id": "t1", "name": "T", "domain": "connect.local",
                             "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                             "cert_key": None, "cert_file": None, "created_at": "",
                             "updated_at": ""}),
                ("tenant-providers", {"id": "tp-live", "tenant_id": "t1", "provider_id": "p-live",
                                      "created_at": "", "updated_at": ""}),
                ("tenant-providers", {"id": "tp-mute", "tenant_id": "t1", "provider_id": "p-mute",
                                      "created_at": "", "updated_at": ""}),
                ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                                   "created_at": "", "updated_at": ""}),
            ]
            ok_seed = True
            for path, body in rows:
                st, out = admin("POST", f"/{path}", body, port=ADMIN + 10)
                if st not in (200, 201):
                    ok_seed = False
                    check(f"U4: seeding {path}", False, f"HTTP {st} {out[:100]}")
            admin("POST", "/reload", {}, port=ADMIN + 10)
            time.sleep(0.3)
            if ok_seed:
                codes4 = [proxied(port=DATA + 10, timeout=25)[0] for _ in range(4)]
                fb_mute = metric_sum(ADMIN + 10, "hydra_upstream_first_byte_timeout_total",
                                     'provider="p-mute"')
                announce("U4 the mixed codes", f"codes={codes4} "
                         f"first_byte_timeouts(p-mute)={fb_mute}")
                check("U4: an upstream that ACCEPTS and then says nothing stays a first-byte "
                      "timeout with NO failover (the documented double-billing policy)",
                      502 in codes4 and fb_mute >= 1.0,
                      f"codes={codes4} first_byte_timeouts={fb_mute}")
    finally:
        stop(node2)

    # ---- U5: a connect bound that cannot take effect is refused --------------
    proc = start_node("mismatch", {
        "HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS": str(FIRST_BYTE),
        "HYDRA_UPSTREAM_CONNECT_TIMEOUT_SECS": str(FIRST_BYTE + 1),
    }, admin_port=ADMIN + 20, data_port=DATA + 20)
    try:
        time.sleep(2.5)
        rc = proc.poll()
        log = open(os.path.join(DIR, "mismatch.log"), errors="replace").read()
        announce("U5 the mismatch node", f"exit={rc}")
        check("U5: a connect bound >= the first-byte bound is refused at startup (the "
              "first-byte timeout would always win, so the bound would silently do nothing)",
              rc is not None and rc != 0 and "CONNECT_TIMEOUT" in log,
              f"exit={rc} log={log.strip().splitlines()[-1][:150] if log.strip() else '<empty>'}")
    finally:
        stop(proc)
        upstream.shutdown()
        silent.shutdown()

    print()
    if failures:
        print(f"UPSTREAM CONNECT BOUND: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("UPSTREAM CONNECT BOUND: PASSED (a dead route fails over instead of surfacing, the "
          "attribution is honest, the bound bounds, the silent-upstream contrast holds, and a "
          "useless connect bound is refused)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
