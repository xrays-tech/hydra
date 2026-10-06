#!/usr/bin/env python3
"""The listener/SNI SIGNALS — do `ops.md` §9.1's first two alert rows fire when they say they do?

§9.1 opens with a table whose first rows are the operator's only warning that TLS is not really
being served, and they are unusually explicit about how to read them:

  | Certs configured, no TLS listener bound | `hydra_listener_tenant_certs > 0 and hydra_listener_bound{protocol="tls"} == 0` | … **Read the two protocols differently**: `protocol="plain"` is LIVENESS …, while `protocol="tls"` is CONFIGURATION (it is published from the config decision and never revised) |
  | TLS listener configured, no certs | `hydra_listener_bound{protocol="tls"} == 1 and hydra_listener_tenant_certs == 0` | Handshakes will fail until a certificate is written |
  | Invalid listener configuration | `increase(hydra_listener_misconfig_total[10m]) > 0` | Startup-time configuration problem (certs without a port / port without certs) |

All three are reachable by configuration alone and nothing had ever evaluated them against a live
scrape. The listener planner runs ONCE at startup, so each case needs a database that ALREADY holds
the certificate when the measured node starts (measured: seeding a cert into a running node leaves
the planner's note at 0 and — for the TLS case — makes a healthy node look misconfigured).

  L1  no certs at startup, no TLS port ⇒ tls=0, certs=0, misconfig absent; rows 1 and 2 false
  L2  a certificate at startup, NO TLS port ⇒ certs=1, tls=0 ⇒ **row 1 fires**, and
      `misconfig{kind="certs_without_tls_port"}` counts (row 3)
  L3  `HYDRA_TLS_LISTEN` set, no certificate ⇒ tls=1, certs=0 ⇒ **row 2 fires**, and
      `misconfig{kind="tls_port_without_certs"}` counts (row 3)
  L4  TLS port AND a certificate ⇒ tls=1, certs=1, no misconfig ⇒ all three false, `bound{plain}`
      is 1, and an HTTPS request carrying the tenant's SNI is really served

Run: python3 integration/test_listener_signals.py    # needs target/debug/hydra (+ openssl)
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "listener-signals-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
TOKEN = "hydra-listener-admin-2026"
DOMAIN = "listener.local"
UPSTREAM = 18729
CERT = {"pem": None, "key": None}

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def label_admin(label):
    """One admin/data/tls port triple per leg (and clear of every other drill's range)."""
    return 18730 + 10 * int(label[1])


def generate_cert():
    crt = os.path.join(DIR, "tenant.crt")
    key = os.path.join(DIR, "tenant.key")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key,
                    "-out", crt, "-days", "1", "-subj", f"/CN={DOMAIN}",
                    "-addext", f"subjectAltName=DNS:{DOMAIN}"],
                   check=True, capture_output=True)
    CERT["pem"] = open(crt).read()
    CERT["key"] = open(key).read()


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
        payload = json.dumps({"allowed": True}).encode() if self.path.startswith("/auth") else \
            json.dumps({"id": "chatcmpl-ls", "object": "chat.completion",
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


def call(method, url, token=None, body=None, timeout=20, host=None):
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


def start_node(label, admin, data, tls_port=None, db_label=None):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin}",
        "HYDRA_LISTEN": f"127.0.0.1:{data}",
        # `db_label`, not `label`: the SEEDING node and the MEASURING node must open the SAME
        # database file. The first version keyed the DB on the log label, so the certificate was
        # seeded into `l2_seed.db` while the measured node opened an empty `l2.db` — every
        # certificate leg then reported `certs=0` and the TLS handshake was (correctly) refused.
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, db_label or label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "info",
    })
    env.pop("HYDRA_TLS_LISTEN", None)
    if tls_port:
        env["HYDRA_TLS_LISTEN"] = f"127.0.0.1:{tls_port}"
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(admin, budget=20.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{admin}/api/v1/health", token=TOKEN)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def kill_our_instances():
    """Kill leftovers of THIS tree's binary — matched on /proc/<pid>/exe, never by name.

    Measured the hard way: an orphan from an earlier round held 127.0.0.1:18740/18741 for ~7.5
    hours, so this drill's first leg failed to bind and the failure looked like a product problem.
    The dev stack's containers also run a process named `hydra`, so only the EXECUTABLE path may be
    used as the criterion."""
    want = os.path.realpath(BIN)
    targets = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            if os.path.realpath(os.readlink(f"/proc/{entry}/exe")) != want:
                continue
        except OSError:
            continue
        targets.append(int(entry))
    for pid in targets:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    return targets


def seed(admin, with_cert):
    tenant = {"id": "t1", "name": "T", "domain": DOMAIN,
              "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
              "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}
    if with_cert:
        tenant["cert_pem"] = CERT["pem"]
        tenant["cert_key_pem"] = CERT["key"]
    rows = [
        ("providers", {"id": "p1", "key": "p1", "name": "P",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
        ("tenants", tenant),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = call("POST", f"http://127.0.0.1:{admin}/api/v1/{path}", token=TOKEN, body=body)
        if st not in (200, 201):
            raise SystemExit(f"[listener] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    call("POST", f"http://127.0.0.1:{admin}/api/v1/reload", token=TOKEN, body={})
    time.sleep(0.3)


def seed_db(label, with_cert):
    """Seed a database with a throwaway node, then STOP it (see the module docstring)."""
    admin = label_admin(label)
    proc = start_node(label + "_seed", admin, admin + 1, db_label=label)
    try:
        if not wait_healthy(admin):
            raise SystemExit(f"[listener] CANNOT VERIFY: the seeding node for {label} never "
                             f"became healthy")
        seed(admin, with_cert=with_cert)
    finally:
        stop(proc)
        time.sleep(0.4)


def gauge(out, needle):
    """The value of the first sample matching `needle`, or None when the series is absent."""
    for line in out.splitlines():
        if line.startswith(needle) and not line.startswith("#"):
            try:
                return float(line.rsplit(" ", 1)[1])
            except ValueError:
                return None
    return None


def signals(admin):
    """The three documented inputs, read from one scrape."""
    _, out = call("GET", f"http://127.0.0.1:{admin}/metrics", token=TOKEN)
    misconfig = [l for l in out.splitlines()
                 if l.startswith("hydra_listener_misconfig_total{") and not l.startswith("#")]
    return {
        "plain": gauge(out, 'hydra_listener_bound{protocol="plain"}'),
        "tls": gauge(out, 'hydra_listener_bound{protocol="tls"}'),
        "certs": gauge(out, "hydra_listener_tenant_certs"),
        "misconfig": sum(float(l.rsplit(" ", 1)[1]) for l in misconfig),
        "kinds": [l.split('kind="', 1)[1].split('"', 1)[0] for l in misconfig],
    }


def wait_for_plain(admin, budget=15.0):
    """Wait until the startup self-check has published `bound{protocol="plain"}`.

    `/api/v1/health` answers from the ADMIN service, which is up before Pingora binds the data
    plane, so a scrape taken the moment health passes can still miss the plain gauge (measured:
    every leg reported `plain=None` while the node was serving fine). The self-check polls its own
    entry port every 250 ms for up to 20 s and publishes once the listener accepts.
    """
    deadline = time.time() + budget
    while time.time() < deadline:
        if signals(admin)["plain"] is not None:
            return True
        time.sleep(0.25)
    return False


def https_with_sni(port, server_name, key):
    """A minimal HTTPS request that really sends SNI.

    A plain `urlopen("https://127.0.0.1:…")` sends NO SNI (an IP literal never becomes a server
    name): the first version of this leg produced `sni=None … NO_CERTIFICATE_SET` and the node was
    right to refuse the handshake.
    """
    body = json.dumps({"model": "echo",
                       "messages": [{"role": "user", "content": "hi"}]}).encode()
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    try:
        raw = socket.create_connection(("127.0.0.1", port), timeout=15)
        with ctx.wrap_socket(raw, server_hostname=server_name) as tls:
            req = (f"POST /v1/chat/completions HTTP/1.1\r\nHost: {server_name}\r\n"
                   f"Authorization: Bearer {key}\r\nContent-Type: application/json\r\n"
                   f"Content-Length: {len(body)}\r\nConnection: close\r\n\r\n").encode()
            tls.sendall(req + body)
            out = b""
            while True:
                chunk = tls.recv(65536)
                if not chunk:
                    break
                out += chunk
        text = out.decode(errors="replace")
        status = text.split("\r\n", 1)[0]
        code = int(status.split()[1]) if status.startswith("HTTP/") else 0
        return code, text
    except Exception as e:
        return 0, f"<{type(e).__name__}: {e}>"


def main():
    if not os.path.exists(BIN):
        print(f"[listener] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    killed = kill_our_instances()
    if killed:
        print(f"   (killed leftover instance(s) of OUR build: {killed})")
    time.sleep(0.4)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    generate_cert()

    try:
        # ---- L1: no certs at startup, no TLS port ------------------------------
        seed_db("l1", with_cert=False)
        admin = label_admin("l1")
        node = start_node("l1", admin, admin + 1)
        try:
            if not wait_healthy(admin):
                check("L1: the node became healthy", False, "never healthy")
            else:
                wait_for_plain(admin)
                s1 = signals(admin)
                announce("L1 the signals", f"{s1}")
                check("L1: with no certificates and no TLS port, `tenant_certs == 0` and "
                      "`bound{tls} == 0` (row 1 is FALSE — nothing was configured)",
                      s1["certs"] == 0 and s1["tls"] == 0.0, f"{s1}")
                check("L1: ...`bound{plain} == 1` (liveness) and no misconfiguration is reported",
                      s1["plain"] == 1.0 and s1["misconfig"] == 0,
                      f"plain={s1['plain']} misconfig={s1['misconfig']}")
        finally:
            stop(node)

        # ---- L2: a certificate present AT STARTUP, no TLS port -----------------
        seed_db("l2", with_cert=True)
        admin = label_admin("l2")
        node = start_node("l2", admin, admin + 1)
        try:
            if not wait_healthy(admin):
                check("L2: the node became healthy", False, "never healthy")
            else:
                wait_for_plain(admin)
                s2 = signals(admin)
                announce("L2 the signals", f"{s2}")
                check("L2: a startup tenant certificate with NO TLS port makes alert row 1 FIRE "
                      "(`tenant_certs > 0 and bound{tls} == 0`) — TLS is configured but silently "
                      "not served",
                      (s2["certs"] or 0) >= 1 and s2["tls"] == 0.0, f"{s2}")
                check("L2: ...and the misconfiguration is named "
                      '(`misconfig{kind="certs_without_tls_port"}`, alert row 3)',
                      s2["misconfig"] >= 1 and "certs_without_tls_port" in s2["kinds"],
                      f"kinds={s2['kinds']} misconfig={s2['misconfig']}")
                check("L2: ...while `bound{plain}` stays 1 (the data plane is fine; only TLS was "
                      "never configured)",
                      s2["plain"] == 1.0, f"plain={s2['plain']}")
        finally:
            stop(node)

        # ---- L3: a TLS port with NO certificates ------------------------------
        seed_db("l3", with_cert=False)
        admin = label_admin("l3")
        node = start_node("l3", admin, admin + 1, tls_port=admin + 2)
        try:
            if not wait_healthy(admin):
                check("L3: the node became healthy", False, "never healthy")
            else:
                wait_for_plain(admin)
                s3 = signals(admin)
                announce("L3 the signals", f"{s3}")
                check("L3: a TLS port with NO certificates makes alert row 2 FIRE "
                      "(`bound{tls} == 1 and tenant_certs == 0`) — handshakes will fail until a "
                      "certificate is written",
                      s3["tls"] == 1.0 and s3["certs"] == 0, f"{s3}")
                check("L3: ...and the misconfiguration is named "
                      '(`misconfig{kind="tls_port_without_certs"}`, alert row 3)',
                      s3["misconfig"] >= 1 and "tls_port_without_certs" in s3["kinds"],
                      f"kinds={s3['kinds']}")
        finally:
            stop(node)

        # ---- L4: the healthy pair, and all three rows FALSE -------------------
        seed_db("l4", with_cert=True)
        admin = label_admin("l4")
        tls_port = admin + 2
        node = start_node("l4", admin, admin + 1, tls_port=tls_port)
        try:
            if not wait_healthy(admin):
                check("L4: the node became healthy", False, "never healthy")
            else:
                wait_for_plain(admin)
                s4 = signals(admin)
                announce("L4 the signals", f"{s4}")
                check("L4: with BOTH a TLS port and a startup certificate all three documented "
                      "expressions are FALSE — `tls == 1`, `certs >= 1`, no misconfig "
                      "(the no-false-alarms direction)",
                      s4["tls"] == 1.0 and (s4["certs"] or 0) >= 1 and s4["misconfig"] == 0
                      and s4["plain"] == 1.0, f"{s4}")
                st4, out4 = https_with_sni(tls_port, DOMAIN, "sk-tenant-1")
                announce("L4 the HTTPS request with SNI", f"HTTP {st4} {out4[:70]}")
                check("L4: ...and the TLS listener really serves that tenant: an HTTPS request "
                      "carrying its SNI answers 200",
                      st4 == 200, f"HTTP {st4} {out4[:80]}")
        finally:
            stop(node)
    finally:
        upstream.shutdown()

    print()
    if failures:
        print(f"LISTENER SIGNALS: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("LISTENER SIGNALS: PASSED (both SNI alert rows and the misconfig row fire exactly when "
          "the documentation says they do, and none of them fires in the healthy topology)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
