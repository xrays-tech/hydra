#!/usr/bin/env python3
"""The STREAMING path, executed — the one proxy behaviour no drill touched.

Round 63 recorded this gap in the plan itself: the load baseline used a single JSON
upstream, so real SSE streaming (incremental delivery, and what happens when a stream
wedges or dies mid-answer) was never measured. `ops.md` §9.1 turns both failure modes into
ALERT CONTRACTS, which means they were documented as signals nobody had ever seen fire:

  * `hydra_upstream_stream_idle_timeout_total` — "the upstream sent response headers (and
    maybe some body) and then no byte for `HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS` … the
    client has already received a 200 plus whatever arrived, so this is a TRUNCATED answer,
    not a retryable failure — and this path also feeds the circuit breaker";
  * `hydra_mid_stream_errors_total` — "a chunk read/write failed AFTER the 200 + first chunk
    was already sent to the client (failover impossible)".

Cases (a throwaway node + a controllable SSE upstream; ports 188x, no Redis):
  S1 incremental delivery: 5 chunks sent with pauses must reach the client ONE BY ONE —
     a proxy that buffers the whole answer would still "pass" a body comparison, so the
     assertion is on ARRIVAL TIMES, not on the body
  S2 a wedged stream: headers + one chunk, then silence past the idle bound -> the client's
     body is TRUNCATED and `hydra_upstream_stream_idle_timeout_total{provider}` fires (§9.1)
  S3 an upstream that dies mid-stream -> `hydra_mid_stream_errors_total{provider}` fires
  S4 a completed stream is still METERED: the usage row exists (billing must not skip streams)

Run: python3 integration/test_streaming_path.py        # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import re
import shutil
import signal
import socket
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "streaming-path-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18810, 18811, 18819
TOKEN = "hydra-streaming-admin-2026"
CLIENT_KEY = "sk-tenant-1"
IDLE_SECS = 2          # HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS for this drill
CHUNK_PAUSE = 0.5      # S1: long enough that buffering is unmistakable

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


# ---------------------------------------------------------------------------
# The SSE upstream: mode switches between incremental / wedged / dying.
# ---------------------------------------------------------------------------
class SseState:
    mode = "incremental"


class SseUpstream(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        if self.path.startswith("/auth"):
            body = json.dumps({"allowed": True, "expires_in": 300}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def chunk(payload: bytes):
            self.wfile.write(b"%x\r\n" % len(payload) + payload + b"\r\n")
            self.wfile.flush()

        try:
            if SseState.mode == "incremental":
                for i in range(5):
                    chunk(f'data: {{"i":{i}}}\n\n'.encode())
                    time.sleep(CHUNK_PAUSE)
                chunk(b"data: [DONE]\n\n")
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            elif SseState.mode == "wedged":
                chunk(b'data: {"first":true}\n\n')
                # Headers + one chunk, then silence far beyond the idle bound.
                time.sleep(IDLE_SECS + 6)
                chunk(b"data: [DONE]\n\n")
                self.wfile.write(b"0\r\n\r\n")
                self.wfile.flush()
            elif SseState.mode == "dying":
                chunk(b'data: {"first":true}\n\n')
                time.sleep(0.2)
                # Kill the connection mid-answer (no final chunk, no terminator).
                self.close_connection = True
                try:
                    self.connection.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
                self.connection.close()
        except (BrokenPipeError, ConnectionResetError, OSError):
            pass

    def log_message(self, *a):
        pass


def read_stream_with_times(timeout=25.0):
    """Raw socket so nothing app-side buffers: returns (status, headers, body, chunk times)."""
    s = socket.create_connection(("127.0.0.1", DATA), timeout=timeout)
    times = []
    body = b""
    eof_at = None
    try:
        body_json = json.dumps({"model": "echo", "stream": True,
                                "messages": [{"role": "user", "content": "hi"}]}).encode()
        req = (b"POST /v1/chat/completions HTTP/1.1\r\n"
               b"Host: stream.local\r\n"
               b"Authorization: Bearer " + CLIENT_KEY.encode() + b"\r\n"
               b"Content-Type: application/json\r\n"
               b"Content-Length: " + str(len(body_json)).encode() + b"\r\n"
               b"Connection: close\r\n\r\n" + body_json)
        s.sendall(req)
        start = time.time()
        while True:
            data = s.recv(4096)
            if not data:
                # Record WHEN the connection ended: that is the only way to tell "the gateway
                # cut a wedged stream at its idle bound" from "the client gave up".
                eof_at = time.time() - start
                break
            times.append((time.time() - start, len(data)))
            body += data
            if time.time() - start > timeout:
                break
    except (socket.timeout, OSError):
        pass
    finally:
        try:
            s.close()
        except OSError:
            pass
    head, _, rest = body.partition(b"\r\n\r\n")
    text = head.decode(errors="replace")
    status = 0
    m = re.match(r"HTTP/1\.1 (\d{3})", text)
    if m:
        status = int(m.group(1))
    headers = {}
    for line in text.split("\r\n")[1:]:
        if ":" in line:
            k, v = line.split(":", 1)
            headers[k.strip().lower()] = v.strip()
    deltas = [t for t, _ in times]
    spans = [round(b - a, 2) for a, b in zip(deltas, deltas[1:])]
    return status, headers, rest, deltas, spans, eof_at


def call(method, url, token=None, body=None, timeout=15):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
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


def metric(name):
    _, body = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
    return [l for l in body.splitlines() if l.startswith(name) and not l.startswith("#")]


def start_node():
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'streaming.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_UPSTREAM_STREAM_IDLE_TIMEOUT_SECS": str(IDLE_SECS),
        "HYDRA_USAGE_SINK": "sqlite",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, "node.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health")[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed():
    admin("POST", "/providers", {"id": "p1", "key": "p1", "name": "P",
                                 "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                                 "created_at": "", "updated_at": ""})
    admin("POST", "/provider-models", {"id": "pm1", "key": "echo", "name": "E",
                                       "provider_id": "p1", "status": 1,
                                       "created_at": "", "updated_at": ""})
    admin("POST", "/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                                     "created_at": ""})
    admin("POST", "/tenants", {"id": "t1", "name": "T", "domain": "stream.local",
                               "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                               "cert_key": None, "cert_file": None,
                               "created_at": "", "updated_at": ""})
    admin("POST", "/tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                                        "created_at": "", "updated_at": ""})
    admin("POST", "/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                                     "created_at": "", "updated_at": ""})
    admin("POST", "/reload", {})
    time.sleep(0.3)


def usage_rows():
    db = os.path.join(DIR, "streaming.db")
    con = sqlite3.connect(f"file:{db}?mode=ro", uri=True, timeout=10)
    try:
        return con.execute("SELECT COUNT(*) FROM usage_record").fetchone()[0]
    except sqlite3.DatabaseError:
        return -1
    finally:
        con.close()


def main():
    if not os.path.exists(BIN):
        print(f"[streaming] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), SseUpstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    node = start_node()
    try:
        if not wait_healthy():
            print("[streaming] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "node.log")).read()[-600:], file=sys.stderr)
            return 2
        seed()

        # ---- S1: incremental delivery ------------------------------------------
        SseState.mode = "incremental"
        st, hdrs, body, deltas, spans, eof_at = read_stream_with_times()
        text = body.decode(errors="replace")
        chunks = text.count("data: ")
        announce("S1 arrival times (s)", f"{[round(d, 2) for d in deltas]}")
        check("S1: the streamed response is 200 with `content-type: text/event-stream`",
              st == 200 and "text/event-stream" in hdrs.get("content-type", ""),
              f"HTTP {st} ct={hdrs.get('content-type')!r}")
        check("S1: every chunk of the upstream's stream arrives (5 + [DONE])",
              chunks == 6 and "[DONE]" in text, f"chunks={chunks} tail={text[-30:]!r}")
        # THE point of the case: a buffering proxy would deliver everything in one read at
        # the end. Assert both the spread of arrivals and that the last one is late.
        check("S1: the chunks arrive INCREMENTALLY, not buffered until the end",
              len(deltas) >= 6 and deltas[-1] >= CHUNK_PAUSE * 3 and max(spans) >= CHUNK_PAUSE * 0.5,
              f"reads={len(deltas)} last={deltas[-1]:.2f}s spans={spans}")
        check("S1: ...and the whole stream completes in about the upstream's own duration",
              2.0 <= deltas[-1] <= 6.0, f"last read at {deltas[-1]:.2f}s")

        # ---- S2: a wedged stream -------------------------------------------------
        SseState.mode = "wedged"
        before = metric("hydra_upstream_stream_idle_timeout_total")
        st, hdrs, body, deltas, spans, eof_at = read_stream_with_times(timeout=IDLE_SECS + 8)
        text = body.decode(errors="replace")
        after = metric("hydra_upstream_stream_idle_timeout_total")
        announce("S2 wedged stream",
                 f"HTTP {st} bytes={len(body)} closed_after={deltas[-1] if deltas else 0:.2f}s")
        announce("§9.1 metric before/after", f"{before} -> {after}")
        check("S2: the client got a 200 and the FIRST chunk before the upstream wedged",
              st == 200 and '"first":true' in text, f"HTTP {st} body={text[:60]!r}")
        check("S2: the answer is TRUNCATED — no `[DONE]` ever arrives (not a retryable failure)",
              "[DONE]" not in text, f"tail={text[-40:]!r}")
        check("S2: the GATEWAY ended the connection at its idle bound (~2s), not the client "
              "and not the upstream's 8s silence",
              eof_at is not None and IDLE_SECS - 0.7 <= eof_at <= IDLE_SECS + 3.5,
              f"connection ended after {eof_at if eof_at is None else round(eof_at, 2)}s "
              f"(idle bound {IDLE_SECS}s, upstream would have stayed quiet for {IDLE_SECS + 6}s)")
        check("S2: §9.1's `hydra_upstream_stream_idle_timeout_total{provider=\"p1\"}` fires",
              any('provider="p1"' in l for l in after) and after != before,
              f"before={before} after={after}")

        # ---- S3: an upstream that dies mid-stream --------------------------------
        SseState.mode = "dying"
        before_mid = metric("hydra_mid_stream_errors_total")
        st, hdrs, body, deltas, spans, eof_at = read_stream_with_times(timeout=15)
        text = body.decode(errors="replace")
        # Give the node a moment to record the failure.
        time.sleep(1.0)
        after_mid = metric("hydra_mid_stream_errors_total")
        announce("S3 upstream died mid-stream",
                 f"HTTP {st} bytes={len(body)} mid_stream_metric {before_mid} -> {after_mid}")
        check("S3: the connection ends promptly when the upstream dies mid-answer",
              eof_at is not None and eof_at < 5.0,
              f"connection ended after {eof_at if eof_at is None else round(eof_at, 2)}s")
        check("S3: the client sees a truncated answer and no terminator",
              st == 200 and '"first":true' in text and "[DONE]" not in text,
              f"HTTP {st} body={text[:60]!r}")
        check("S3: `hydra_mid_stream_errors_total{provider=\"p1\"}` fires (failover is impossible here)",
              any('provider="p1"' in l for l in after_mid) and after_mid != before_mid,
              f"before={before_mid} after={after_mid}")

        # ---- S4: a completed stream is still metered -----------------------------
        SseState.mode = "incremental"
        rows_before = usage_rows()
        read_stream_with_times()
        deadline = time.time() + 10
        rows_after = rows_before
        while time.time() < deadline:
            rows_after = usage_rows()
            if rows_after > rows_before:
                break
            time.sleep(0.5)
        announce("S4 usage rows", f"{rows_before} -> {rows_after}")
        check("S4: a streamed request is METERED (billing must not skip streams)",
              rows_after > rows_before, f"rows {rows_before} -> {rows_after}")
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"STREAMING PATH: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("STREAMING PATH: PASSED (incremental delivery, wedged stream truncates + fires the "
          "§9.1 idle metric, mid-stream death fires its metric, streams are metered)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
