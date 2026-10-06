#!/usr/bin/env python3
"""The MASTER-KEY ROTATION flow on the real binary — `ops.md` §1.2/§3's one-shot switch.

`HYDRA_RESEAL_SECRETS=1` is documented as an operator maintenance mode: "set to `1` (or `true`) to
re-seal every stored secret under `HYDRA_ENCRYPTION_KEY`/`_VERSION` and EXIT — the process does not
serve traffic in this mode", with a report line
`reseal: provider_keys=… tenant_certs=… already_current=… failed=…` deciding the exit code, and a
hard rule: "`failed` non-empty means **do not delete the previous key**". `ops.md` §3 additionally
promises that an already-current row is re-verified by actually OPENING it, and that any
`reseal FAILED: …` line leaves that row untouched.

`crates/hydra-server/tests/key_rotation.rs` covers the re-seal logic at the LIBRARY level
(`StaticKeyProvider`, no process, no env, no report, no exit code). Nothing had ever run the switch
the documentation tells an operator to run inside a container — i.e. the wiring that decides
whether a rotation succeeds or loses every provider credential:

  K0  a node sealed under key A serves, and the upstream receives the DECRYPTED provider key
  K1  `HYDRA_RESEAL_SECRETS=1` rewrites the rows under key B (previous key A) and exits 0 with the
      documented report; it binds no listener
  K2  the node then starts with key B ALONE and still presents the same decrypted key upstream
  K3  a SECOND re-seal under key B (no previous key) reports `already_current` — the documented
      "re-verified by opening it" path
  K4  the OLD key alone no longer opens the rows: the node refuses to start (fail loud, not serve
      garbage)
  K5  a re-seal that CANNOT open a row (key C, no previous key) exits **1**, prints
      `reseal FAILED: …`, and leaves the row untouched — the "do not delete the previous key"
      contract

Run: python3 integration/test_key_rotation_live.py    # needs target/debug/hydra
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

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "key-rotation-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18800, 18801, 18809
TOKEN = "hydra-keyrot-admin-2026"
DOMAIN = "keyrot.local"
# Two well-formed 32-byte keys (the loader requires a 32-byte key, base64 or hex).
KEY_A = base64.b64encode(b"A" * 32).decode()
KEY_B = base64.b64encode(b"B" * 32).decode()
KEY_C = base64.b64encode(b"C" * 32).decode()
SECRET = "sk-provider-secret-42"

failures = []

CERT_DIR = {"pem": None, "key": None}


def generate_cert():
    """A throwaway self-signed cert/key pair (openssl CLI; no Python crypto dependency)."""
    pem = os.path.join(DIR, "tenant.crt")
    key = os.path.join(DIR, "tenant.key")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
                    "-keyout", key, "-out", pem, "-days", "1", "-subj", "/CN=keyrot.local"],
                   check=True, capture_output=True)
    CERT_DIR["pem"] = open(pem).read()
    CERT_DIR["key"] = open(key).read()


def cert_pem():
    return CERT_DIR["pem"]


def cert_key_pem():
    return CERT_DIR["key"]


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Received:
    """`Authorization` header the upstream saw: the DECRYPTED provider key, end to end."""

    def __init__(self):
        self.lock = threading.Lock()
        self.auths = []

    def record(self, auth):
        with self.lock:
            self.auths.append(auth)

    def snapshot(self):
        with self.lock:
            return list(self.auths)


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
            payload = json.dumps({"id": "chatcmpl-keyrot", "object": "chat.completion",
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


def proxied():
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token="sk-tenant-1",
                host=DOMAIN,
                body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})


def db_path():
    return os.path.join(DIR, "rotate.db")


def base_env(**extra):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{db_path()}?mode=rwc",
        "RUST_LOG": "info",
    })
    env.update(extra)
    return env


def start_node(key, label, version="1", previous=None, previous_version=None):
    env = base_env(**{
        "HYDRA_ENCRYPTION_KEY": key,
        "HYDRA_ENCRYPTION_KEY_VERSION": version,
    })
    if previous:
        env["HYDRA_ENCRYPTION_KEY_PREVIOUS"] = previous
    if previous_version:
        env["HYDRA_ENCRYPTION_KEY_PREVIOUS_VERSION"] = previous_version
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def reseal(key, label, version="2", previous=None, previous_version="1"):
    """Run the documented one-shot maintenance mode and return (rc, stdout+stderr, seconds)."""
    env = base_env(**{
        "HYDRA_ENCRYPTION_KEY": key,
        "HYDRA_ENCRYPTION_KEY_VERSION": version,
        "HYDRA_RESEAL_SECRETS": "1",
    })
    if previous:
        env["HYDRA_ENCRYPTION_KEY_PREVIOUS"] = previous
    if previous_version:
        env["HYDRA_ENCRYPTION_KEY_PREVIOUS_VERSION"] = previous_version
    started = time.time()
    p = subprocess.run([BIN], env=env, capture_output=True, text=True, timeout=60)
    out = (p.stdout or "") + (p.stderr or "")
    with open(os.path.join(DIR, f"{label}.log"), "w") as fh:
        fh.write(out)
    return p.returncode, out, time.time() - started


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
        # The SECRET that gets sealed: the provider's upstream API key.
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": SECRET,
                           "created_at": ""}),
        # The certificate's private key is SEALED, exactly like a provider key, and `cert_pem`/
        # `cert_key_pem` are fields of the tenant body itself (there is no separate cert route —
        # measured: `POST /api/v1/tenants/t1/cert` is 404 `unknown path`). Without a cert the
        # report's `tenant_certs` column would be untested.
        ("tenants", {"id": "t1", "name": "T", "domain": DOMAIN,
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": "",
                     "cert_pem": cert_pem(), "cert_key_pem": cert_key_pem()}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[keyrot] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[keyrot] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    # ---- K0: sealed under key A, and the upstream sees the DECRYPTED key ------
    node = start_node(KEY_A, "node_a", version="1")
    try:
        if not wait_healthy():
            print("[keyrot] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            with open(os.path.join(DIR, "node_a.log"), errors="replace") as fh:
                print(fh.read()[-700:], file=sys.stderr)
            return 2
        generate_cert()
        seed()
        st0, body0 = proxied()
        auth0 = RECEIVED.snapshot()[-1] if RECEIVED.snapshot() else None
        announce("K0 the first request", f"HTTP {st0}; upstream saw {auth0!r}")
        check("K0: the node serves and the upstream receives the DECRYPTED provider key",
              st0 == 200 and auth0 == f"Bearer {SECRET}", f"HTTP {st0} auth={auth0!r}")
    finally:
        stop(node)

    # ---- K1: the documented one-shot re-seal ---------------------------------
    rc1, out1, took1 = reseal(KEY_B, "reseal_b", version="2", previous=KEY_A,
                              previous_version="1")
    report = next((l for l in out1.splitlines() if l.startswith("reseal: ")), "<no report line>")
    announce("K1 the reseal report", f"exit={rc1} in {took1:.1f}s :: {report}")
    check("K1: `HYDRA_RESEAL_SECRETS=1` rewrites the rows and exits 0",
          rc1 == 0, f"exit={rc1} :: {out1.strip()[-160:]}")
    check("K1: ...printing the documented report line with `failed=0`",
          report != "<no report line>" and "failed=0" in report, report)
    check("K1: ...and it re-sealed BOTH sealed row kinds (provider_keys=1, tenant_certs=1)",
          "provider_keys=1" in report and "tenant_certs=1" in report, report)
    check("K1: ...in maintenance mode it does NOT bind the data plane (the doc says it does not "
          "serve traffic in this mode)",
          took1 < 30 and "startup self-check" not in out1 and "listening" not in out1.lower(),
          f"ran {took1:.1f}s; log mentions a listener: "
          f"{'yes' if 'listening' in out1.lower() else 'no'}")

    # ---- K2: the node starts with key B ALONE and still presents the key ------
    node_b = start_node(KEY_B, "node_b", version="2")
    try:
        if not wait_healthy():
            check("K2: the node starts with the NEW key alone", False, "never healthy")
        else:
            st2, _ = proxied()
            auth2 = RECEIVED.snapshot()[-1]
            announce("K2 after rotation", f"HTTP {st2}; upstream saw {auth2!r}")
            check("K2: under key B ALONE the row opens and the upstream still gets the same "
                  "decrypted provider key",
                  st2 == 200 and auth2 == f"Bearer {SECRET}", f"HTTP {st2} auth={auth2!r}")
    finally:
        stop(node_b)

    # ---- K3: re-sealing an already-current row (documented: it re-OPENS it) ---
    rc3, out3, _ = reseal(KEY_B, "reseal_again", version="2")
    report3 = next((l for l in out3.splitlines() if l.startswith("reseal: ")), "<no report line>")
    announce("K3 the second reseal", f"exit={rc3} :: {report3}")
    check("K3: a second re-seal under the SAME key reports both rows as `already_current` and "
          "still exits 0 (the documented 're-verified by opening it' path)",
          rc3 == 0 and "already_current=2" in report3 and "failed=0" in report3, report3)

    # ---- K4: the OLD key alone must not open the rows ------------------------
    node_a2 = start_node(KEY_A, "node_a2", version="1")
    try:
        healthy = wait_healthy(budget=12)
        rc4 = node_a2.poll()
        log4 = open(os.path.join(DIR, "node_a2.log"), errors="replace").read()
        announce("K4 starting with the retired key", f"healthy={healthy} exit={rc4}")
        check("K4: the retired key no longer opens the rotated rows — the node refuses to start "
              "rather than serving with unusable credentials",
              (not healthy) and rc4 is not None and rc4 != 0,
              f"healthy={healthy} exit={rc4} :: "
              f"{next((l for l in log4.splitlines() if 'decrypt' in l.lower() or 'fatal' in l.lower()), '<no line>')[:150]}")
    finally:
        stop(node_a2)

    # ---- K6: "already_current" must mean it OPENED, not that the versions match -----
    # The rows are sealed under key B at version 2. Re-seal with a DIFFERENT key at the SAME
    # version (no previous key): if the code compared version labels it would report
    # `already_current=2` and exit 0 — an operator would believe the rotation was complete while
    # both rows are still sealed under the retired key. The docs promise the stronger semantics
    # ("A row whose version already equals the current one is re-verified by actually opening
    # it"). This is that promise's only live test.
    rc6, out6, _ = reseal(KEY_C, "reseal_same_version", version="2", previous=None)
    report6 = next((l for l in out6.splitlines() if l.startswith("reseal: ")), "<no report line>")
    failed6 = [l for l in out6.splitlines() if l.startswith("reseal FAILED:")]
    announce("K6 the same-version wrong-key reseal", f"exit={rc6} :: {report6} :: {failed6[:1]}")
    check("K6: a row whose VERSION matches but whose KEY does not is reported as failed, not as "
          "`already_current` — the documented 're-verified by opening it' semantics",
          rc6 == 1 and "failed=2" in report6 and "already_current=0" in report6,
          f"exit={rc6} :: {report6}")

    # ---- K7: RETIRED (2026-10-05) ---------------------------------------------
    # K7 pinned the documented "needs a local database (this is an edge node)" refusal by starting a
    # node with `HYDRA_ROLE=edge`. BOTH halves of that premise are gone: the `edge` role was retired
    # with the homogeneous topology (ADR-0001 D-2), so the env no longer yields a node without a local
    # database — every node has one — and the refusal is therefore UNREACHABLE by configuration.
    #
    # Measured 2026-10-05: with the retired env set, the node boots as a single node with a local
    # database and the reseal RUNS, so the leg failed for the right reason (its premise, not its
    # subject). The code path it covered is still there as defence in depth —
    # `main.rs` builds the pool with `let pool = if false { None } else { … Some(p) }`, a leftover of
    # the role retirement, and `ConfigStore::from_snapshot` / `StoreError::NoDatabase` / the
    # `not_ready` guard in `tenant_api::handlers::local_write` exist for the case it would have
    # produced — but nothing can reach them now. That dead defensive path (and the `if false` a reader
    # has to decode) is recorded as its own item rather than deleted silently here.
    #
    # What still guards the switch itself is K8 below (a typo must not silently mean "no rotation"),
    # and what guards the ROTATION is K1–K6: those are the ones the documented procedure depends on.

    # K8: THE SWITCH'S OWN TYPO. The documented procedure is "run this one-shot and read the
    # report + exit code"; a value the parser does not recognise must not silently turn into
    # "serve traffic normally" — the operator would see a node come up and believe the rotation
    # ran. `upgrade_requested` is deliberately strict about exactly this ("an operator who typos
    # the flag gets the loud refusal instead of a silent non-upgrade").
    # `on`/`TRUE`/`Yes` are now VALID ON values (case-insensitive, documented), so the invalid
    # probes are the values outside the documented vocabulary.
    for value in ("reseal", "2", "enabled", "y", "1x", "ture"):
        env8 = base_env(**{"HYDRA_ENCRYPTION_KEY": KEY_B, "HYDRA_ENCRYPTION_KEY_VERSION": "2",
                           "HYDRA_RESEAL_SECRETS": value})
        p8 = subprocess.Popen([BIN], env=env8, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                              text=True)
        served = False
        deadline = time.time() + 6
        while time.time() < deadline:
            if call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/health", token=TOKEN)[0] == 200:
                served = True
                break
            if p8.poll() is not None:
                break
            time.sleep(0.25)
        rc8 = p8.poll()
        stop(p8)
        time.sleep(0.2)
        announce(f"K8 HYDRA_RESEAL_SECRETS={value!r}", f"served={served} exit={rc8}")
        check(f"K8: an unrecognised value ({value!r}) is REFUSED at startup, not silently treated "
              f"as 'serve traffic normally' (a typo must not look like a finished rotation)",
              (not served) and rc8 is not None and rc8 != 0,
              f"served={served} exit={rc8}")

    # ...and the OFF values must still mean "serve normally" (they are documented too).
    for value in ("0", "false", "no", "off"):
        env9 = base_env(**{"HYDRA_ENCRYPTION_KEY": KEY_B, "HYDRA_ENCRYPTION_KEY_VERSION": "2",
                           "HYDRA_RESEAL_SECRETS": value})
        p9 = subprocess.Popen([BIN], env=env9, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                              text=True)
        served9 = False
        deadline = time.time() + 6
        while time.time() < deadline:
            if call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/health", token=TOKEN)[0] == 200:
                served9 = True
                break
            time.sleep(0.25)
        stop(p9)
        time.sleep(0.2)
        announce(f"K8b HYDRA_RESEAL_SECRETS={value!r}", f"served={served9}")
        check(f"K8: the documented OFF value {value!r} still serves traffic normally (the switch "
              f"is strict, not paranoid)",
              served9, f"served={served9}")

    # ---- K5: a re-seal that cannot open a row (key C, no previous key) -------
    rc5, out5, _ = reseal(KEY_C, "reseal_c", version="3", previous=None)
    failed_lines = [l for l in out5.splitlines() if l.startswith("reseal FAILED:")]
    report5 = next((l for l in out5.splitlines() if l.startswith("reseal: ")), "<no report line>")
    announce("K5 the impossible reseal", f"exit={rc5} :: {report5} :: "
             f"{failed_lines[:1]}")
    check("K5: a re-seal that cannot open a row exits **1** and names it (`reseal FAILED: …`) — "
          "the documented 'do not delete the previous key' contract",
          rc5 == 1 and bool(failed_lines), f"exit={rc5} failed-lines={len(failed_lines)}")
    check("K5: ...and the report says `failed=2` (never 0) — BOTH rows could not be opened — so "
          "an incomplete rotation cannot be mistaken for a finished one",
          "failed=2" in report5, report5)
    # ...and the row was LEFT UNTOUCHED: key B still opens it.
    node_b2 = start_node(KEY_B, "node_b2", version="2")
    try:
        if not wait_healthy():
            check("K5: the untouched row still opens under key B", False, "never healthy")
        else:
            st6, _ = proxied()
            auth6 = RECEIVED.snapshot()[-1] if RECEIVED.snapshot() else None
            check("K5: ...the failed re-seal left the row UNTOUCHED (key B still opens it end to "
                  "end)",
                  st6 == 200 and auth6 == f"Bearer {SECRET}", f"HTTP {st6} auth={auth6!r}")
    finally:
        stop(node_b2)
        upstream.shutdown()

    print()
    if failures:
        print(f"KEY ROTATION LIVE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("KEY ROTATION LIVE: PASSED (the one-shot re-seal reports and exits 0, the new key alone "
          "opens the rows end to end, already-current rows are re-opened, the retired key fails "
          "loud, and an impossible re-seal exits 1 and leaves the row untouched)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
