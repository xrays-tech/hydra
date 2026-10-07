#!/usr/bin/env python3
"""Where the master key comes from — and what happens when it is wrong.

`ops.md` §1.2 documents the material rather than the mechanism: `HYDRA_ENCRYPTION_KEY` is
"**Base64 of 32 bytes** … Generate with `openssl rand 32 | base64`", and "a matching
`HYDRA_ENCRYPTION_KEY_FILE` (**raw 32-byte file**) is also accepted". Two forms, one keystream —
and the inline form is base64 while the file form is raw, which is exactly the kind of asymmetry
an operator gets wrong at 3 a.m. (`openssl rand 32 > /run/secrets/key` is the natural sibling of
`openssl rand 32 | base64`, and a Kubernetes secret file ends with a newline).

Nothing tested any of it: round 106 rotated keys through `HYDRA_ENCRYPTION_KEY` only, and the
unit tests in `crypto.rs` call `StaticKeyProvider::new([7u8; 32], 1)` directly, i.e. they never
touch the environment or the filesystem. Cases:

  S1  the inline base64 form serves traffic, and the upstream sees the decrypted provider key
  S2  the FILE form is byte-identical to it: rows sealed under the inline key open under the file,
      and (the mirror) rows sealed under the file open under the inline key
  S3  a file ending in `\\n` — what `echo`/`openssl rand 32 > f`/a K8s secret volume actually
      produces — is accepted (the trailing newline is trimmed)
  S4  ...and so is `\\r\\n`; but a trailing SPACE is not, and the error says how many bytes arrived
  S5  when BOTH are set the FILE wins (documented "preferring the file form"), proven in both
      directions: file=A+inline=B opens A-sealed rows; file=A+inline=B refuses B-sealed rows
  S6  neither set ⇒ the binary refuses to start (fail-closed) with the documented message
  S7  `HYDRA_ENCRYPTION_KEY_FILE` pointing at a missing path ⇒ a startup error that NAMES the path
  S8  a file holding BASE64 text (the classic mix-up) fails with a length error — and the message
      should tell the operator which form is which, since "got 44" is otherwise a puzzle

Run: python3 integration/test_master_key_sources.py    # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import base64
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
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "master-key-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18790, 18791, 18799
TOKEN = "hydra-masterkey-admin-2026"
DOMAIN = "masterkey.local"
SECRET = "sk-master-key-probe-7"

RAW_A = b"A" * 32
RAW_B = b"B" * 32
B64_A = base64.b64encode(RAW_A).decode()
B64_B = base64.b64encode(RAW_B).decode()

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
        self.auths = []

    def record(self, auth):
        with self.lock:
            self.auths.append(auth)

    def last(self):
        with self.lock:
            return self.auths[-1] if self.auths else None


RECEIVED = Received()


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
        if self.path.startswith("/auth"):
            payload = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            RECEIVED.record(self.headers.get("Authorization"))
            payload = json.dumps({"id": "chatcmpl-mk", "object": "chat.completion",
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


def status_of():
    """The probe used by `run_once`: only the status code (the body is not the subject)."""
    return proxied()[0]


def proxied():
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token="sk-tenant-1",
                host=DOMAIN,
                body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})


def db_file(label):
    return os.path.join(DIR, f"{label}.db")


def start(label, key_file_bytes=None, inline=None, db=None, extra=None):
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{db or db_file('shared')}?mode=rwc",
        "RUST_LOG": "info",
    })
    env.pop("HYDRA_ENCRYPTION_KEY_FILE", None)
    env.pop("HYDRA_ENCRYPTION_KEY", None)
    if key_file_bytes is not None:
        path = os.path.join(DIR, f"{label}.key")
        with open(path, "wb") as fh:
            fh.write(key_file_bytes)
        env["HYDRA_ENCRYPTION_KEY_FILE"] = path
    if inline is not None:
        env["HYDRA_ENCRYPTION_KEY"] = inline
    env.update(extra or {})
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(budget=20.0):
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


def run_once(label, key_file_bytes=None, inline=None, db=None, extra=None, probe=None,
             budget=20.0):
    """Start a node, wait for health, optionally PROBE IT WHILE IT IS UP, then stop it.

    Returns (started, exit_code_or_None, log_text, probe_result). The probe has to run here: the
    first version returned after stopping the node, so every caller's request hit a dead port
    (`HTTP 0`) and three legs failed for that reason alone.
    """
    proc = start(label, key_file_bytes, inline, db, extra)
    try:
        started = wait_healthy(budget)
        rc = proc.poll()
        result = probe() if (started and probe is not None) else None
        log = open(os.path.join(DIR, f"{label}.log"), errors="replace").read()
        return started, rc, log, result
    finally:
        stop(proc)


def seed():
    rows = [
        ("providers", {"id": "p1", "key": "p1", "name": "P",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": SECRET, "created_at": ""}),
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
            raise SystemExit(f"[masterkey] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[masterkey] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    # ---- S1: the inline base64 form -------------------------------------------
    db_a = db_file("key_a")
    proc = start("s1_inline", inline=B64_A, db=db_a)
    try:
        if not wait_healthy():
            print("[masterkey] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "s1_inline.log"), errors="replace").read()[-600:],
                  file=sys.stderr)
            return 2
        seed()
        st1, _ = proxied()
        check("S1: the inline base64 form serves, and the upstream gets the decrypted key",
              st1 == 200 and RECEIVED.last() == f"Bearer {SECRET}",
              f"HTTP {st1} auth={RECEIVED.last()!r}")
    finally:
        stop(proc)

    # ---- S2: the FILE form is byte-identical (both directions) ----------------
    ok2, rc2, log2, st2 = run_once("s2_file", key_file_bytes=RAW_A, db=db_a, probe=status_of)
    check("S2: rows sealed under the INLINE key open under the raw 32-byte FILE form",
          ok2 and st2 == 200 and RECEIVED.last() == f"Bearer {SECRET}",
          f"started={ok2} HTTP {st2} auth={RECEIVED.last()!r}")
    db_b = db_file("key_b")
    proc = start("s2b_seal", key_file_bytes=RAW_B, db=db_b)
    try:
        if wait_healthy():
            seed()
            proxied()
        else:
            check("S2: the file form can seal rows in the first place", False, "never healthy")
    finally:
        stop(proc)
    ok2b, _, _, st2b = run_once("s2b_inline", inline=B64_B, db=db_b, probe=status_of)
    check("S2: ...and the mirror holds too — rows sealed through the FILE form open with the "
          "inline base64 form (the two forms are the same keystream)",
          ok2b and st2b == 200 and RECEIVED.last() == f"Bearer {SECRET}",
          f"started={ok2b} HTTP {st2b}")

    # ---- S3/S4: how the file may end ------------------------------------------
    for label, ending, expect in (("s3_lf", b"\n", True), ("s3_crlf", b"\r\n", True),
                                  ("s4_space", b" ", False)):
        ok, rc, log, _ = run_once(label, key_file_bytes=RAW_A + ending, db=db_a)
        if expect:
            check(f"S3: a key file ending in {ending!r} (what `echo`/a K8s secret volume "
                  f"produces) is accepted",
                  ok, f"started={ok} exit={rc} :: {log.strip().splitlines()[-1][:110] if log.strip() else ''}")
        else:
            check(f"S4: a key file ending in a SPACE is NOT silently accepted (only line endings "
                  f"are trimmed) — and the error counts the bytes",
                  (not ok) and rc is not None and "33" in log,
                  f"started={ok} exit={rc} :: {next((l for l in log.splitlines() if '32 bytes' in l), '<no line>')[:120]}")

    # ---- S5: precedence — the FILE wins ---------------------------------------
    # A-sealed DB + (file=A, inline=B): serving proves the FILE was used; if the inline key had
    # won, the rows would not open and the node would refuse to start.
    ok5, rc5, log5, st5 = run_once("s5_precedence", key_file_bytes=RAW_A, inline=B64_B, db=db_a,
                                    probe=status_of)
    check("S5: with BOTH set the FILE wins (documented `preferring the file form`) — an A-sealed "
          "DB opens while the inline key names B",
          ok5 and st5 == 200, f"started={ok5} HTTP {st5}")
    # ...and the mirror: a B-sealed DB with the same pair must REFUSE (the inline B is not used).
    ok5b, rc5b, log5b, _ = run_once("s5_precedence_b", key_file_bytes=RAW_A, inline=B64_B, db=db_b)
    check("S5: ...and the mirror: a B-sealed DB refuses with that same pair, so the inline key is "
          "definitely not the one in use",
          (not ok5b) and rc5b is not None and rc5b != 0, f"started={ok5b} exit={rc5b}")

    # ---- S6/S7/S8: the failure modes ------------------------------------------
    ok6, rc6, log6, _ = run_once("s6_unset", db=db_a)
    check("S6: with NEITHER form set the binary refuses to start (fail-closed) and says which "
          "variables it wants",
          (not ok6) and rc6 is not None and rc6 != 0 and "HYDRA_ENCRYPTION_KEY" in log6,
          f"started={ok6} exit={rc6} :: {next((l for l in log6.splitlines() if 'master key' in l), '<no line>')[:130]}")
    env7 = dict(os.environ)
    proc7 = subprocess.run(
        [BIN], capture_output=True, text=True, timeout=40,
        env={**env7, "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
             "HYDRA_LISTEN": f"127.0.0.1:{DATA}", "HYDRA_DB_URL": f"sqlite://{db_a}?mode=rwc",
             "RUST_LOG": "info",
             "HYDRA_ENCRYPTION_KEY_FILE": os.path.join(DIR, "does-not-exist.key")})
    out7 = (proc7.stdout or "") + (proc7.stderr or "")
    check("S7: a missing `HYDRA_ENCRYPTION_KEY_FILE` fails startup and NAMES the path",
          proc7.returncode != 0 and "does-not-exist.key" in out7,
          f"exit={proc7.returncode} :: {next((l for l in out7.splitlines() if 'master key' in l or 'key file' in l), '<no line>')[:140]}")
    ok8, rc8, log8, _ = run_once("s8_base64_file", key_file_bytes=B64_A.encode(), db=db_a)
    line8 = next((l for l in log8.splitlines() if "32 bytes" in l), "<no line>")
    announce("S8 a file holding base64 text", f"started={ok8} exit={rc8} :: {line8[:140]}")
    check("S8: a key FILE holding base64 text fails (the file form is RAW bytes) — and the error "
          "tells the operator which form is which, instead of only 'got 44'",
          (not ok8) and rc8 is not None and "44" in log8 and ("base64" in line8.lower()
                                                              or "raw" in line8.lower()),
          f"started={ok8} exit={rc8} :: {line8[:140]}")

    stop(None)
    upstream.shutdown()

    print()
    if failures:
        print(f"MASTER KEY SOURCES: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("MASTER KEY SOURCES: PASSED (both forms and their equivalence, the newline handling, "
          "the documented file precedence, and every failure mode named)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
