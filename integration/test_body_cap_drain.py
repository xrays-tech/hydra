#!/usr/bin/env python3
"""The 1 MiB body caps: who DRAINS the rest of an oversized upload, and who resets the client.

`ops.md` §6.7 records a deliberate asymmetry as a known limitation:

  > **Known limitation (no fix promised):** when a tenant-API request body exceeds the 1 MiB cap,
  > the node replies `413` and closes the connection **without draining the rest of the body** — a
  > client still uploading a large body may observe a connection reset before it reads the `413`
  > body.

and the code agrees (`tenant_api/mod.rs::read_body` returns the 413 tuple straight away), while the
DATA plane's hard-cap path *does* call `session.as_downstream_mut().drain_request_body()` before
answering `413` (`proxy.rs`). So one plane is polite to a client that is still uploading and the
other is documented not to be. Nothing had measured either, and "may observe a connection reset"
is exactly the kind of claim that is either true on the wire or not:

  T1  the tenant API with a body that FITS in the socket buffers: the `413 payload_too_large` is
      readable (the doc's `Known limitation` wording is about a client *still uploading*)
  T2  the tenant API with a body that is DRIBBLED in: what the client actually observes — a
      readable `413`, or a broken write before it can read one — AND how much of the upload the node
      consumed before it answered (at the 1 MiB cap, or only after draining all 2 MiB)
  T3  the CONTRAST: the data plane with an oversized drippled body drains, so the client can always
      read its `413 request_body_too_large`
  T4  after either outcome a FRESH connection is served normally (no lingering damage)

Run: python3 integration/test_body_cap_drain.py    # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import errno
import json
import os
import select
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
DIR = os.path.join(ROOT, ".acceptance", "cap-drain-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18760, 18761, 18769
TOKEN = "hydra-capdrain-admin-2026"
DOMAIN = "capdrain.local"
TENANT_TOKEN = "tenant-cap-drain-token-2026"
BIG = 2 * 1024 * 1024          # 2 MiB: twice the tenant-API cap, and 32x the data-plane cap below
TENANT_CAP = 1024 * 1024       # the tenant API cap: tenant_api/mod.rs::read_body -> MAX_BODY
DATA_CAP = 64 * 1024           # HYDRA_MAX_REQUEST_BODY_HARD for this drill

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
        payload = json.dumps({"allowed": True}).encode() if self.path.startswith("/auth") else \
            json.dumps({"id": "chatcmpl-cap", "object": "chat.completion",
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


def call(method, url, token=None, body=None, timeout=25, host=None, raw=None):
    data = raw if raw is not None else (None if body is None else json.dumps(body).encode())
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
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'cap.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_MAX_REQUEST_BODY_HARD": str(DATA_CAP),
        "RUST_LOG": "info",
    })
    log = open(os.path.join(DIR, "cap.log"), "w")
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
                     "access_token": TENANT_TOKEN, "cert_key": None, "cert_file": None,
                     "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[capdrain] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def dribble(port, path, host, token, total, chunk=64 * 1024, pause=0.01, first_wait=0.0,
            method="POST"):
    """Send `total` bytes slowly and report what the CLIENT observes.

    Returns a dict: whether a response was readable, its status line and body, the bytes the
    client managed to write, and the write error (if the connection broke mid-upload).
    """
    body = b'{"name":"' + b"x" * (total - 16) + b'"}'
    body = body[:total]
    sock = socket.create_connection(("127.0.0.1", port), timeout=20)
    sock.settimeout(20)
    # The METHOD matters: the tenant API keys its write routes on (method, path), so a POST on a
    # PUT-shaped path is answered 404 *before* the body is read (measured; round 89 documented the
    # two-shape routing) — the first version of T2 did exactly that and never reached the cap.
    headers = (f"{method} {path} HTTP/1.1\r\nHost: {host}\r\n"
               f"Authorization: Bearer {token}\r\nContent-Type: application/json\r\n"
               f"Content-Length: {len(body)}\r\n\r\n").encode()
    written = 0
    write_error = None
    response = b""
    try:
        sock.sendall(headers)
        if first_wait:
            time.sleep(first_wait)
        offset = 0
        while offset < len(body):
            piece = body[offset:offset + chunk]
            try:
                sock.sendall(piece)
                written += len(piece)
                offset += len(piece)
            except OSError as e:
                write_error = f"{type(e).__name__}: {e}"
                break
            # Is there already a response to read while we are still uploading?
            ready, _, _ = select.select([sock], [], [], 0.02)
            if ready:
                try:
                    response += sock.recv(65536)
                except OSError:
                    pass
                break
            time.sleep(pause)
        # Read whatever the server sends until EOF (bounded).
        deadline = time.time() + 8
        while time.time() < deadline and b"\r\n\r\n" not in response:
            ready, _, _ = select.select([sock], [], [], 0.3)
            if not ready:
                break
            try:
                chunk_in = sock.recv(65536)
            except OSError as e:
                write_error = write_error or f"{type(e).__name__}: {e} (read)"
                break
            if not chunk_in:
                break
            response += chunk_in
    finally:
        sock.close()
    status_line = response.split(b"\r\n", 1)[0].decode(errors="replace") if response else ""
    return {
        "status_line": status_line,
        "body": response.decode(errors="replace"),
        "written": written,
        "total": len(body),
        "write_error": write_error,
    }


def main():
    if not os.path.exists(BIN):
        print(f"[capdrain] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    node = start_node()
    try:
        if not wait_healthy():
            print("[capdrain] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "cap.log"), errors="replace").read()[-600:],
                  file=sys.stderr)
            return 2
        seed()
        tenant_path = "/tenant/t1/api/v1/sub-tenants/s1"

        # ---- T1: the tenant API, body uploaded as fast as the socket takes it ----
        body1 = b'{"name":"' + b"x" * (BIG - 16) + b'"}'
        st1, out1 = call("PUT", f"http://127.0.0.1:{DATA}{tenant_path}", token=TENANT_TOKEN,
                         host=DOMAIN, raw=body1[:BIG], timeout=30)
        announce("T1 the tenant API with a 2 MiB body written fast",
                 f"HTTP {st1} {out1[:90]}")
        check("T1: the tenant API's cap answers `413 payload_too_large`",
              st1 == 413 and "payload_too_large" in out1, f"HTTP {st1} {out1[:90]}")

        # ---- T2: the same request, DRIBBLED (the documented "still uploading" case) --
        seen2 = dribble(DATA, tenant_path, DOMAIN, TENANT_TOKEN, BIG, chunk=32 * 1024, pause=0.02,
                        method="PUT")
        announce("T2 the tenant API with a drippled body",
                 f"wrote {seen2['written']}/{seen2['total']} bytes, write_error="
                 f"{seen2['write_error']!r}, response={seen2['status_line']!r}")
        readable2 = seen2["status_line"].startswith("HTTP/1.1 413") and "payload_too_large" in seen2["body"]
        # The DETERMINISTIC discriminator is how much of the body the node consumed before it
        # answered: ~1 MiB (the cap) means it answered WITHOUT draining, all of it means it drained.
        # BOTH bounds of that window are needed, and the REPLY is part of the claim: `written < total`
        # alone is ALSO true when the connection broke mid-upload and no reply was ever sent — the
        # check would then pass while proving the opposite of what it says (round 168). The lower
        # bound is not a measurement but a consequence of the code: `tenant_api::read_body` answers
        # only once `buf.len() + chunk.len() > MAX_BODY`, so the node has READ more than 1 MiB before
        # the 413 exists at all, and a client cannot have written less than the node read.
        check("T2: MEASURED — the tenant API answers `413 payload_too_large` WITHOUT draining: it "
              "replied after only ~1 MiB of the 2 MiB upload (at the cap, not after the whole body)",
              readable2 and TENANT_CAP <= seen2["written"] < seen2["total"],
              f"wrote {seen2['written']}/{seen2['total']} "
              f"(need {TENANT_CAP} <= written < {seen2['total']}), reply={seen2['status_line']!r}, "
              f"write_error={seen2['write_error']!r}")
        # Distinct from the check above: this one is about the CLIENT's experience — the reply is
        # readable and the connection was NOT reset under a client that stops pushing as soon as the
        # response appears. Whether the client then observes a reset is a race the doc itself hedges
        # ("may observe"): measured here, the failure needs a client that keeps pushing.
        check("T2: ...and a client that stops pushing when it sees the response DOES read its "
              "`413 payload_too_large` with no write error (so the documented 'may observe a reset' "
              "is the keep-pushing case, not the ordinary one)",
              readable2 and seen2["write_error"] is None,
              f"{seen2['status_line']!r} write_error={seen2['write_error']!r}")

        # ---- T3: the data plane's contrast (it DRAINS before answering) -------------
        body3 = b'{"model":"echo","messages":[{"role":"user","content":"' + b"y" * (DATA_CAP * 2) + b'"}]}'
        seen3 = dribble(DATA, "/v1/chat/completions", DOMAIN, "sk-tenant-1", len(body3),
                        chunk=8 * 1024, pause=0.02)
        announce("T3 the DATA plane with a drippled oversized body",
                 f"wrote {seen3['written']}/{seen3['total']} bytes, write_error="
                 f"{seen3['write_error']!r}, response={seen3['status_line']!r}")
        check("T3: the data plane DRAINS the rest of the upload first (it consumed ALL "
              "131125 bytes before answering), so the client can always read its "
              "`413 request_body_too_large`",
              seen3["status_line"].startswith("HTTP/1.1 413")
              and "request_body_too_large" in seen3["body"]
              and seen3["written"] == seen3["total"],
              f"{seen3['status_line']!r} wrote {seen3['written']}/{seen3['total']}")

        # ---- T4: a fresh connection works on both planes ---------------------------
        st4a, out4a = call("GET", f"http://127.0.0.1:{DATA}{tenant_path.rsplit('/', 1)[0]}",
                           token=TENANT_TOKEN, host=DOMAIN)
        st4b, out4b = call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token="sk-tenant-1",
                           host=DOMAIN,
                           body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})
        announce("T4 fresh requests afterwards", f"tenant API HTTP {st4a}; data plane HTTP {st4b}")
        check("T4: no lingering damage — a fresh connection is served on both planes",
              st4a == 200 and st4b == 200, f"tenant={st4a} data={st4b}")
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"BODY CAP DRAIN: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("BODY CAP DRAIN: PASSED (the tenant API's non-draining 413 is real on the wire, the data "
          "plane drains and lets the client read its 413, and neither leaves damage behind)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
