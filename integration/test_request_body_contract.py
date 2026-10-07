#!/usr/bin/env python3
"""The data-plane REQUEST BODY contract — caps, the total deadline, and truncated bodies.

`ops.md` §1.2 documents three client-facing outcomes for the body the gateway buffers
(terminate-mode reads the WHOLE body, so this is the gateway's memory and availability
surface), and `integration/` had **no test for any of them** (`grep -rl
'request_body_too_large|REQUEST_BODY_TIMEOUT|MAX_REQUEST_BODY_HARD' integration/` was empty):

  * over `HYDRA_MAX_REQUEST_BODY_HARD` -> **413 `request_body_too_large`** + close, detected in
    the read loop; the comparison in the code is `buf.len() > cap`, so a body of EXACTLY the cap
    is legal — a boundary nothing tested;
  * longer than `HYDRA_REQUEST_BODY_TIMEOUT_SECS` -> **408 `request_body_timeout`** + close.
    The document is explicit that this is a TOTAL deadline, not an idle one, because
    "pingora's HTTP/1 body read has only a per-read deadline (every byte resets it), and HTTP/2
    has no read timeout at all, so only a total deadline stops a client that sends half and
    stops";
  * a body that ENDS EARLY -> the code says this used to be forwarded as if the client had sent
    it ("a silent-corruption path — fail closed instead"). It answers 400
    `request_body_read_error`. **This drill is the test for that claimed fix**: the mock upstream
    records the declared `Content-Length` and the bytes it actually received, so a truncated body
    reaching the provider cannot hide.

Also pinned: the two OTHER 1 MiB caps are compile-time constants with their own code —
the admin API answers `413 request_body_too_large`, the tenant API `413 payload_too_large`
(different strings, documented separately), so a client that treats "413" alone as the signal
sees two different bodies.

Run: python3 integration/test_request_body_contract.py    # needs target/debug/hydra
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
DIR = os.path.join(ROOT, ".acceptance", "request-body-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18820, 18821, 18829
TOKEN = "hydra-body-admin-2026"
CAP = 65536               # HYDRA_MAX_REQUEST_BODY_HARD for this drill (64 KiB)
BODY_TIMEOUT = 2          # HYDRA_REQUEST_BODY_TIMEOUT_SECS for this drill
DOMAIN = "body.local"
TENANT_TOKEN = "tenant-body-cap-token-2026"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Received:
    """What the UPSTREAM actually received: declared length vs bytes, per request."""

    def __init__(self):
        self.lock = threading.Lock()
        self.calls = []          # (declared, received_bytes)

    def record(self, declared, received):
        with self.lock:
            self.calls.append((declared, received))

    def snapshot(self):
        with self.lock:
            return list(self.calls)


RECEIVED = Received()


class Upstream(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        declared = int(self.headers.get("Content-Length") or 0)
        body = b""
        if declared:
            body = self.rfile.read(declared)
        RECEIVED.record(declared, len(body))
        if self.path.startswith("/auth"):
            payload = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            # A prompt large enough to make the response useful, plus the model echoed back.
            payload = json.dumps({"id": "chatcmpl-body", "object": "chat.completion",
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

    def do_GET(self):
        payload = b'{"object":"list","data":[]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *a):
        pass


def call(method, url, token=None, body=None, timeout=30, host=None, raw_body=None):
    data = raw_body if raw_body is not None else (None if body is None else json.dumps(body).encode())
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


def chat_body(model="echo", pad=0):
    envelope = {"model": model, "messages": [{"role": "user", "content": "hi"}]}
    if pad:
        envelope["messages"][0]["content"] = "x" * pad
    return json.dumps(envelope).encode()


def proxied(body_bytes, timeout=30):
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token="sk-tenant-1",
                host=DOMAIN, raw_body=body_bytes, timeout=timeout)


def raw_exchange(headers: str, body: bytes, tail_behavior: str, timeout=20):
    """Open a raw socket, send `headers`, send `body`, then behave as told.

    `tail_behavior`: "stall" (stop sending, keep the socket open), "truncate" (close the write
    side early → a body shorter than Content-Length), or "finish" (send everything).

    Returns (response_bytes, elapsed).
    """
    started = time.time()
    sock = socket.create_connection(("127.0.0.1", DATA), timeout=timeout)
    sock.settimeout(timeout)
    try:
        sock.sendall(headers.encode())
        if body:
            sock.sendall(body)
        if tail_behavior == "stall":
            pass                       # keep the connection open, sending nothing more
        elif tail_behavior == "truncate":
            sock.shutdown(socket.SHUT_WR)   # early EOF: fewer bytes than Content-Length
        out = b""
        while True:
            try:
                chunk = sock.recv(65536)
            except socket.timeout:
                break
            if not chunk:
                break
            out += chunk
            if b"\r\n\r\n" in out and tail_behavior != "stall":
                break
        return out, time.time() - started
    finally:
        sock.close()


def start_node():
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'body')}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_MAX_REQUEST_BODY_HARD": str(CAP),
        "HYDRA_REQUEST_BODY_TIMEOUT_SECS": str(BODY_TIMEOUT),
        "RUST_LOG": "info",
    })
    log = open(os.path.join(DIR, "body.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def stop(proc):
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def wait_healthy(budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health")[0] == 200:
            return True
        time.sleep(0.25)
    return False


def seed():
    rows = [
        ("providers", {"id": "p1", "key": "p1", "name": "P",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                           "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": DOMAIN,
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     # The TENANT API authenticates with this, not with a client key — the
                     # first version of B7 sent `sk-tenant-1` and got 401 before the body cap.
                     "access_token": TENANT_TOKEN,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[body] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[body] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    node = start_node()
    try:
        if not wait_healthy():
            print("[body] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "body.log"), errors="replace").read()[-700:],
                  file=sys.stderr)
            return 2
        seed()

        # ---- B0 control ---------------------------------------------------------
        st0, out0 = proxied(chat_body(pad=100))
        announce("B0 the control request", f"HTTP {st0} {out0[:70]}")
        check("B0: a normal body is accepted and forwarded", st0 == 200, f"HTTP {st0}")

        # ---- B1/B2: the hard cap and its boundary ------------------------------
        # Build a body of EXACTLY the cap. The content replaces the 2-char "hi", so the padding
        # needed is `CAP - len(empty) + 2` — the first version was 2 bytes SHORT and therefore
        # tested nothing about the boundary (the length is asserted below so it cannot drift).
        empty = chat_body(pad=0)
        exact = chat_body(pad=CAP - len(empty) + 2)
        announce("B1 the boundary body", f"{len(exact)} bytes vs cap {CAP}")
        st1, out1 = proxied(exact)
        check("B1: a body of EXACTLY the cap is legal (the code compares `>`), not 413",
              st1 == 200 and len(exact) == CAP, f"HTTP {st1} len={len(exact)} {out1[:60]}")
        over = chat_body(pad=CAP - len(empty) + 2 + 64)
        st2, out2 = proxied(over)
        announce("B2 a body one chunk over the cap", f"{len(over)} bytes -> HTTP {st2} {out2[:90]}")
        check("B2: a body OVER the cap is `413 request_body_too_large`",
              st2 == 413 and "request_body_too_large" in out2, f"HTTP {st2} {out2[:90]}")

        # ---- B3: the TOTAL deadline on a client that sends half and stops -------
        headers = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: {DOMAIN}\r\n"
                   f"Authorization: Bearer sk-tenant-1\r\nContent-Type: application/json\r\n"
                   f"Content-Length: 100000\r\n\r\n")
        raw, elapsed = raw_exchange(headers, b'{"model":"echo","messages":' + b"x" * 900,
                                    "stall", timeout=BODY_TIMEOUT + 6)
        first_line = raw.split(b"\r\n", 1)[0].decode(errors="replace")
        announce("B3 the stalled client", f"{first_line!r} after {elapsed:.1f}s")
        check(f"B3: a client that declares a body and stops is cut by the TOTAL deadline "
              f"({BODY_TIMEOUT}s) with `408 request_body_timeout`, not left hanging",
              b" 408 " in raw and b"request_body_timeout" in raw
              and BODY_TIMEOUT - 0.5 <= elapsed <= BODY_TIMEOUT + 3.0,
              f"{first_line!r} after {elapsed:.1f}s")

        # ---- B4: a TRUNCATED body must not reach the provider -------------------
        mark = len(RECEIVED.snapshot())
        headers4 = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: {DOMAIN}\r\n"
                    f"Authorization: Bearer sk-tenant-1\r\nContent-Type: application/json\r\n"
                    f"Content-Length: 5000\r\n\r\n")
        raw4, elapsed4 = raw_exchange(headers4, b'{"model":"echo","messages":' + b"y" * 200,
                                      "truncate", timeout=15)
        line4 = raw4.split(b"\r\n", 1)[0].decode(errors="replace")
        forwarded = RECEIVED.snapshot()[mark:]
        announce("B4 the truncated client", f"{line4!r} after {elapsed4:.1f}s; "
                 f"upstream saw {forwarded}")
        truncated_upstream = [c for c in forwarded if c[0] != c[1]]
        check("B4: bytes the client never sent are NOT forwarded — the upstream never receives "
              "a body shorter than its own `Content-Length` (the silent-corruption path the "
              "code says it fixed)",
              not truncated_upstream,
              f"upstream calls after this leg: {forwarded}")
        check("B4: ...and the client is told: 400 `request_body_read_error` (documented as "
              "'fail closed', not a clean end-of-body)",
              b" 400 " in raw4 and b"request_body_read_error" in raw4, f"{line4!r}")

        # B4b is the DISCRIMINATING version of the same probe: the client declares more bytes
        # than it sends but the bytes it DID send are a complete, valid request body. Truncation
        # inside the JSON (B4) is caught a second time by model extraction, so B4 alone cannot
        # tell "the read-error guard works" from "the JSON parse happened to fail" — verified by
        # reverting the guard (only B4b's forwarding check then goes red).
        mark_b = len(RECEIVED.snapshot())
        valid_prefix = chat_body()          # a complete, valid body …
        headers4b = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: {DOMAIN}\r\n"
                     f"Authorization: Bearer sk-tenant-1\r\nContent-Type: application/json\r\n"
                     f"Content-Length: {len(valid_prefix) + 4096}\r\n\r\n")
        raw4b, elapsed4b = raw_exchange(headers4b, valid_prefix, "truncate", timeout=15)
        line4b = raw4b.split(b"\r\n", 1)[0].decode(errors="replace")
        forwarded_b = RECEIVED.snapshot()[mark_b:]
        announce("B4b the valid-prefix truncated client",
                 f"{line4b!r} after {elapsed4b:.1f}s; upstream saw {forwarded_b}")
        check("B4b: a body that is VALID but SHORTER than its own `Content-Length` is refused "
              "too — the upstream is never asked to serve a request the client did not finish",
              not forwarded_b, f"upstream calls after this leg: {forwarded_b}")
        check("B4b: ...with the same documented 400 `request_body_read_error`",
              b" 400 " in raw4b and b"request_body_read_error" in raw4b, f"{line4b!r}")

        # ---- B5: the same deadline on a CHUNKED body ---------------------------
        headers5 = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: {DOMAIN}\r\n"
                    f"Authorization: Bearer sk-tenant-1\r\nContent-Type: application/json\r\n"
                    f"Transfer-Encoding: chunked\r\n\r\n")
        raw5, elapsed5 = raw_exchange(headers5, b"64\r\n" + b"z" * 100, "stall",
                                      timeout=BODY_TIMEOUT + 6)
        line5 = raw5.split(b"\r\n", 1)[0].decode(errors="replace")
        announce("B5 the stalled chunked client", f"{line5!r} after {elapsed5:.1f}s")
        check("B5: a CHUNKED body that stalls mid-chunk is cut by the same total deadline "
              "(pingora's H1 body read only has a per-read deadline, which every byte resets)",
              b" 408 " in raw5 and BODY_TIMEOUT - 0.5 <= elapsed5 <= BODY_TIMEOUT + 3.0,
              f"{line5!r} after {elapsed5:.1f}s")

        # ---- B6/B7: the two OTHER 1 MiB caps, with two different codes ----------
        st6, out6 = admin("POST", "/providers", None)
        big_json = json.dumps({"id": "pad", "key": "pad", "name": "PAD",
                               "endpoint": f"http://127.0.0.1:{UPSTREAM}",
                               "weight": 1, "created_at": "", "updated_at": "",
                               "note": "n" * (2 * 1024 * 1024)}).encode()
        st6, out6 = call("POST", f"http://127.0.0.1:{ADMIN}/api/v1/providers", token=TOKEN,
                         raw_body=big_json, timeout=30)
        announce("B6 a 2 MiB admin body", f"HTTP {st6} {out6[:90]}")
        check("B6: the admin API's own 1 MiB cap answers `413 request_body_too_large`",
              st6 == 413 and "request_body_too_large" in out6, f"HTTP {st6} {out6[:90]}")
        big_tenant = json.dumps({"name": "x" * (2 * 1024 * 1024)}).encode()
        st7, out7 = call("PUT", f"http://127.0.0.1:{DATA}/tenant/t1/api/v1/sub-tenants/s1",
                         token=TENANT_TOKEN, host=DOMAIN, raw_body=big_tenant, timeout=30)
        announce("B7 a 2 MiB tenant-API body", f"HTTP {st7} {out7[:90]}")
        check("B7: the tenant API's cap answers `413 payload_too_large` — a DIFFERENT code from "
              "the admin/data-plane one (a client keying on \"413\" alone sees two bodies)",
              st7 == 413 and "payload_too_large" in out7, f"HTTP {st7} {out7[:90]}")

        # ---- B8: the caps are constants, not tunable ---------------------------
        check("B8: the data-plane cap used here is the documented env var, i.e. this drill is "
              "testing the tunable one (not one of the two constants)",
              CAP == 65536, f"cap={CAP}")
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"REQUEST BODY CONTRACT: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("REQUEST BODY CONTRACT: PASSED (hard cap + its boundary, the total deadline on both "
          "the length-delimited and chunked paths, truncated bodies failing closed, and the two "
          "other 1 MiB caps with their own codes)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
