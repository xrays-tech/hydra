#!/usr/bin/env python3
"""The CLIENT hangs up mid-stream: is the request still metered, and does the upstream get cancelled?

Every other failure in the streaming path has been measured (round 91: a wedged upstream and an
upstream that dies mid-answer; round 102: a dead route). The one direction nobody measured is the
CLIENT going away while the answer is streaming:

  * `proxy.rs::stream_response` writes each chunk to the downstream and **returns `Err` on a
    downstream write failure**. Its own comment says that path deliberately does **not** feed the
    circuit breaker, "because a downstream write failure usually means the CLIENT went away, which
    must not mark a provider unhealthy" — a claim nothing had tested.
  * the usage record is built in the **logging phase**, from whatever the SSE scanner absorbed, so
    a client that hangs up should still produce a row: the question an operator (and a billing
    reconciliation) actually has is *"did we keep the usage we had already seen, or did the
    disconnect erase it?"*

Cases (a mock SSE upstream that reports `usage` in its FIRST chunk, as some providers do, and then
keeps streaming):
  C0  control: a client that reads the whole stream is served and metered
  C1  a client that reads the first chunk and RESETS the connection (SO_LINGER 0):
      - the upstream's socket is closed by the gateway (the extraction work stops — cost control)
      - the request still produces a usage row, with the tokens that had already been reported
      - the provider is NOT marked unhealthy (dead-set stays empty)
      - the gateway keeps serving other requests
  C2  what the metrics say about it (`hydra_mid_stream_errors_total` is the UPSTREAM-side series
      from round 91; a downstream disconnect must not be attributed to the provider)

Run: python3 integration/test_client_disconnect.py    # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import socket
import sqlite3
import struct
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from _mock_clickhouse import MockClickHouse
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "client-disconnect-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18750, 18751, 18759
TOKEN = "hydra-disconnect-admin-2026"
DOMAIN = "disconnect.local"
CHUNKS = 8                  # chunks the mock tries to send per stream
CHUNK_PAUSE = 0.15

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Streams:
    """What the mock upstream managed to deliver, per streamed request."""

    def __init__(self):
        self.lock = threading.Lock()
        self.sent = []          # chunks delivered before the socket broke (or all of them)
        self.broke = []         # True when a write failed (the gateway went away)

    def start(self):
        with self.lock:
            idx = len(self.sent)
            self.sent.append(0)
            self.broke.append(False)
            return idx

    def delivered(self, idx):
        with self.lock:
            self.sent[idx] += 1

    def broke(self, idx):
        with self.lock:
            self.broke[idx] = True

    def snapshot(self):
        with self.lock:
            return list(self.sent), list(self.broke)


STREAMS = Streams()


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
        body = self.rfile.read(length) if length else b""
        if self.path.startswith("/auth"):
            payload = json.dumps({"allowed": True, "expires_in": 300}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return
        streamed = b'"stream":true' in body or b'"stream": true' in body
        if not streamed:
            payload = json.dumps({"id": "chatcmpl-dc", "object": "chat.completion",
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
            return
        idx = STREAMS.start()
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        try:
            for i in range(CHUNKS):
                # The usage object rides in the FIRST chunk (as some providers do), so a client
                # that hangs up after the first chunk has still made the gateway see it.
                data = {"id": "chatcmpl-dc", "object": "chat.completion.chunk",
                        "choices": [{"index": 0, "delta": {"content": f"tok{i}"},
                                     "finish_reason": None}]}
                if i == 0:
                    data["usage"] = {"prompt_tokens": 5, "completion_tokens": 8,
                                     "total_tokens": 13}
                payload = f"data: {json.dumps(data)}\n\n".encode()
                self.wfile.write(b"%x\r\n" % len(payload) + payload + b"\r\n")
                self.wfile.flush()
                STREAMS.delivered(idx)
                time.sleep(CHUNK_PAUSE)
            end = b"data: [DONE]\n\n"
            self.wfile.write(b"%x\r\n" % len(end) + end + b"\r\n")
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        except OSError:
            STREAMS.broke(idx)
            return
        self.close_connection = True

    def log_message(self, *a):
        pass


def call(method, url, token=None, body=None, timeout=25, host=None):
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


def start_node():
    env = dict(os.environ)
    env.update(usage_env("clickhouse", MOCK.url))
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'dc.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS": "5",
        "RUST_LOG": "info",
    })
    log = open(os.path.join(DIR, "dc.log"), "w")
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
            raise SystemExit(f"[disconnect] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


MOCK = MockClickHouse()


def usage_rows():
    """(count, last row as a dict) as WRITTEN to the ClickHouse double (ADR-0002 T3.10).

    The rows come from a real HTTP server the gateway posted to, so this stays a measurement of
    what the node recorded. (It used to read the local SQLite table; the ClickHouse schema carries
    the same neutral `tokens_in`/`tokens_out` counters plus `trace_id`.)
    """
    rows = MOCK.rows
    return len(rows), (rows[-1] if rows else {})


def wait_for_rows(want, budget=15.0):
    """The sink flushes on a size OR time threshold, so a row appears only after one of them."""
    deadline = time.time() + budget
    last = (-1, {})
    while time.time() < deadline:
        last = usage_rows()
        if last[0] >= want:
            return last
        time.sleep(0.5)
    return last


def stream_request(token="sk-tenant-1"):
    data = json.dumps({"model": "echo", "stream": True,
                       "messages": [{"role": "user", "content": "hi"}]}).encode()
    return urllib.request.Request(f"http://127.0.0.1:{DATA}/v1/chat/completions", data=data,
                                  method="POST",
                                  headers={"Host": DOMAIN,
                                           "Authorization": f"Bearer {token}",
                                           "Content-Type": "application/json"})


def mid_stream_count(text, provider="p1"):
    """The value of `hydra_mid_stream_errors_total{provider="p1"}` in a /metrics body.

    Returns 0.0 when the series is absent — the honest reading for a counter materialised on first
    use (measured: it does not appear at all before the first abort) — and None when the line exists
    but its value cannot be parsed (never silently 0).
    """
    prefix = f'hydra_mid_stream_errors_total{{provider="{provider}"}}'
    for line in text.splitlines():
        if line.startswith(prefix):
            try:
                return float(line.rsplit(" ", 1)[1])
            except ValueError:
                return None
    return 0.0


def read_all_stream():
    with urllib.request.urlopen(stream_request(), timeout=30) as r:
        first = r.read(4096)
        rest = r.read()
        return r.status, first + rest


def read_first_chunk_then_reset():
    """Read the first SSE chunk, then reset the connection (SO_LINGER 0 → RST)."""
    sock = socket.create_connection(("127.0.0.1", DATA), timeout=20)
    sock.settimeout(20)
    req = stream_request()
    body = req.data
    head = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: {DOMAIN}\r\n"
            f"Authorization: {req.get_header('Authorization')}\r\n"
            f"Content-Type: application/json\r\nContent-Length: {len(body)}\r\n\r\n").encode()
    sock.sendall(head + body)
    first = b""
    deadline = time.time() + 15
    while time.time() < deadline and b"\n\n" not in first:
        chunk = sock.recv(4096)
        if not chunk:
            break
        first += chunk
    # Hard reset: SO_LINGER (onoff=1, linger=0).
    sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
    sock.close()
    return first.decode(errors="replace")


def main():
    if not os.path.exists(BIN):
        print(f"[disconnect] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    MOCK.start()
    node = start_node()
    try:
        if not wait_healthy():
            print("[disconnect] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "dc.log"), errors="replace").read()[-600:], file=sys.stderr)
            return 2
        seed()

        # ---- C0: control — the whole stream is read ---------------------------
        st0, body0_bytes = read_all_stream()
        body0 = body0_bytes.decode(errors="replace")
        n0, row0 = wait_for_rows(1)
        deliver0, broke0 = STREAMS.snapshot()
        announce("C0 the complete stream",
                 f"HTTP {st0} chunks={deliver0} broke={broke0} rows={n0} last={row0}")
        check("C0: a fully-read stream is served and metered (control)",
              st0 == 200 and "[DONE]" in body0 and n0 == 1
              and row0.get("tokens_in") == 5 and row0.get("tokens_out") == 8,
              f"HTTP {st0} rows={n0} tokens={row0.get('tokens_in')}/{row0.get('tokens_out')}")

        # ---- C1: the client resets after the first chunk ----------------------
        # Read the counter BEFORE the abort: the C2 leg claims the abort "IS counted", and a bare
        # "the series exists" check cannot tell a fresh increment from a series that was already
        # non-zero (round 173 — the same shape as the C6 tautology and the T2 false pass).
        _, out_mid_before = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
        mid_before = mid_stream_count(out_mid_before)
        first = read_first_chunk_then_reset()
        # Give the gateway a moment to notice, then check the upstream side.
        time.sleep(2.5)
        deliver1, broke1 = STREAMS.snapshot()
        announce("C1 the reset client",
                 f"first-chunk={first[:60]!r} chunks delivered per stream={deliver1} broke={broke1}")
        n1, row1 = wait_for_rows(2)
        _, out_dead = admin("GET", "/breaker")
        announce("C1 after the reset", f"rows={n1} last={row1} breaker={out_dead[:80]}")
        # The reliable detector is how many chunks the upstream got to DELIVER: once the gateway
        # stops pulling, the mock stops at that chunk. (`broke` is not reliable: writes into a
        # closed socket keep succeeding locally until an RST arrives — the first version asserted
        # on it and failed while the cancellation had in fact happened.)
        check("C1: the gateway STOPS PULLING from the upstream once the client is gone (the mock "
              "delivers only the chunks it managed to send before that, not all 8)",
              deliver1[-1] < CHUNKS - 1 and deliver1[-1] >= 1,
              f"delivered {deliver1[-1]}/{CHUNKS} chunks (write errors seen: {broke1[-1]})")
        check("C1: the request is STILL METERED — a usage row exists with the tokens the upstream "
              "had already reported (a hung-up client does not erase usage)",
              n1 >= 2 and row1.get("tokens_in") == 5 and row1.get("tokens_out") == 8,
              f"rows={n1} last={row1}")
        # MEASURED, and it is a documentation nuance rather than a defect: the row looks exactly
        # like a completed answer (status 200, error NULL) even though the client was reset after
        # the first chunk. The tokens were really produced and really reported, so metering them is
        # right — but the usage store cannot tell you the answer was truncated.
        check("C1: ...and the row is indistinguishable from a COMPLETE answer (status 200, "
              "`error` NULL) — the store records the usage, not the truncation",
              row1.get("error") is None and row1.get("status_code") == 200,
              f"status={row1.get('status_code')} error={row1.get('error')!r}")
        check("C1: the provider is NOT marked unhealthy — a downstream write failure must not "
              "trip the breaker (the comment in `stream_response` claims this)",
              "p1" not in out_dead, f"breaker dead-set={out_dead[:80]}")

        # ---- C2: the node keeps serving ---------------------------------------
        st2, body2 = call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token="sk-tenant-1",
                          host=DOMAIN,
                          body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})
        check("C2: the node keeps serving normally afterwards", st2 == 200,
              f"HTTP {st2} {body2[:60]}")
        _, out_mid = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
        mid = [l for l in out_mid.splitlines()
               if l.startswith("hydra_mid_stream_errors_total{") and not l.startswith("#")]
        announce("C2 the upstream-side mid-stream series", f"{mid}")
        # ops.md documents this exactly: "the failure is counted in
        # `hydra_mid_stream_errors_total{provider}`" for ANY mid-stream cause, while only the
        # idle-bound (upstream-silent) case feeds the breaker. The series therefore counts a
        # client-caused abort too — measured here — and the drill asserts the documented
        # behaviour rather than the opposite (the first version expected an empty series).
        mid_after = mid_stream_count(out_mid)
        check("C2: a client-caused mid-stream abort IS counted in `hydra_mid_stream_errors_total"
              "{provider}` (the documented behaviour: the series covers every mid-stream cause) — "
              "asserted as an INCREMENT, so it is attributable to this abort and not to a value that "
              "was already there",
              mid_before is not None and mid_after is not None and mid_after > mid_before,
              f"{mid} (counter {mid_before} before the abort -> {mid_after} after)")
        check("C2: ...while the provider stays out of the dead-set (C1), i.e. metric and breaker "
              "deliberately disagree about the same event — also documented",
              "p1" not in out_dead, f"dead-set={out_dead[:70]}")
    finally:
        stop(node)
        upstream.shutdown()
        MOCK.stop()

    print()
    if failures:
        print(f"CLIENT DISCONNECT: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("CLIENT DISCONNECT: PASSED (the upstream is cancelled, the request stays metered with "
          "the tokens already seen, the provider is not blamed, and the node keeps serving)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
