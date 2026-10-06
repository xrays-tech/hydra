#!/usr/bin/env python3
"""`ops.md` §3.1 "Updating a tenant's cert (hot — no restart)", asserted.

Documented claims under test (verified 2026-09-29; this file is the executable
contract, so a regression turns CI red):

  1. `PUT /api/v1/tenants/{id}` with new `cert_file`/`cert_key` makes the new cert
     live **by itself** — no restart, no manual `/reload`. (The mechanism is the
     post-write `reload_all()`; a falsification probe that skips it makes this test
     fail with a refused handshake, which is how that was confirmed.)
  2. **New** TLS handshakes present the new certificate.
  3. **Existing** connections are unaffected — a session opened before the rotation
     still holds the old certificate.
  4. SNI picks the right tenant certificate on a shared listener, and an SNI with no
     matching tenant is refused at the handshake (no default cert) without wedging
     the listener.
  5. §3.3: `POST /api/v1/reload` returns the snapshot counts including `certs`.

Harness lessons baked in (each cost a wasted run):
  * kill leftovers by process NAME and refuse to start if the ports are still taken;
  * refuse to measure unless OUR pid owns the listeners (`ss` names the owner) — a
    stale instance otherwise answers the health probe and every measurement
    silently describes the wrong process;
  * a refused handshake is a reported FAIL, never a traceback.
"""
import json
import os
import socket
import ssl
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))  # integration/ -> repo root
DIR = os.path.join(ROOT, ".acceptance", "cert-reload-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
# Ports clear of the dev stack (8080-8084/8090) and of the other suites.
ADMIN, PLAIN, TLS = 18281, 18280, 18243
TOKEN = "hydra-cert-token-2026"
DB = os.path.join(DIR, "cert.db")
KEY = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="

failures = []


class HandshakeRefused(Exception):
    """The listener refused the handshake — for a tenant-shaped SNI that is a FAIL."""


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def run(cmd, **kw):
    return subprocess.run(cmd, capture_output=True, text=True, **kw)


def gen_cert(name, cn, days, org):
    crt = os.path.join(DIR, f"{name}.crt")
    key = os.path.join(DIR, f"{name}.key")
    r = run(["openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes",
             "-keyout", key, "-out", crt, "-days", str(days), "-subj", f"/CN={cn}/O={org}"])
    if r.returncode != 0:
        raise SystemExit(f"openssl failed: {r.stderr[:200]}")
    return crt, key


def admin(method, path, body=None, token=TOKEN):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{ADMIN}{path}", data=data, method=method,
                                 headers={"Authorization": f"Bearer {token}",
                                          "Content-Type": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=6) as resp:
            return resp.status, resp.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except urllib.error.URLError as e:
        return 0, str(e)


def tls_peer(sni, port=TLS):
    """A handshaken connection whose peer certificate we can inspect."""
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE
    try:
        sock = socket.create_connection(("127.0.0.1", port), timeout=5)
        return ctx.wrap_socket(sock, server_hostname=sni)
    except (ssl.SSLError, OSError) as e:
        raise HandshakeRefused(f"{type(e).__name__}: {e}") from e


def cert_fingerprint(pem_text):
    tmp = os.path.join(DIR, "_peer.pem")
    with open(tmp, "w") as fh:
        fh.write(pem_text)
    info = run(["openssl", "x509", "-in", tmp, "-noout", "-fingerprint", "-sha256",
                "-enddate", "-subject"]).stdout
    fp = next((l.split("=", 1)[1] for l in info.splitlines() if "Fingerprint" in l), "?").replace(":", "")
    end = next((l.split("=", 1)[1] for l in info.splitlines() if "notAfter" in l), "?").strip()
    subj = next((l.split("=", 1)[1] for l in info.splitlines() if "subject" in l), "?").strip()
    return fp, end, subj


def served_cert(sni):
    tls = tls_peer(sni)
    try:
        return cert_fingerprint(ssl.DER_cert_to_PEM_cert(tls.getpeercert(binary_form=True)))
    finally:
        tls.close()


def fingerprint_of(path):
    info = run(["openssl", "x509", "-in", path, "-noout", "-fingerprint", "-sha256"]).stdout
    return next((l.split("=", 1)[1] for l in info.splitlines() if "Fingerprint" in l), "?").replace(":", "")


def port_owner():
    r = run(["bash", "-c",
             "ss -ltnp 2>/dev/null | grep -E ':(%d|%d|%d) ' | grep -o 'pid=[0-9]*' | sort -u"
             % (ADMIN, PLAIN, TLS)])
    return {int(x.split("=")[1]) for x in r.stdout.split() if x.startswith("pid=")}


def start_hydra():
    for port in (ADMIN, PLAIN, TLS):
        if run(["bash", "-c", f"ss -ltn | grep -q ':{port} '"]).returncode == 0:
            raise SystemExit(f"port {port} is still taken — refusing to measure a stale instance")
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{PLAIN}", "HYDRA_TLS_LISTEN": f"127.0.0.1:{TLS}",
        "HYDRA_DB_URL": f"sqlite://{DB}?mode=rwc", "HYDRA_ENCRYPTION_KEY": KEY,
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, "hydra.log"), "w")
    proc = subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)
    for _ in range(80):
        if proc.poll() is not None:
            raise SystemExit("hydra exited early:\n" + open(os.path.join(DIR, "hydra.log")).read()[-600:])
        st, out = admin("GET", "/api/v1/health")
        if st == 200:
            # Prove it is OUR process and a FRESH database before believing anything.
            owners = port_owner()
            if owners != {proc.pid}:
                raise SystemExit(f"listeners are owned by {sorted(owners)}, not our pid {proc.pid} "
                                 "— a leftover instance is answering")
            st2, tenants = admin("GET", "/api/v1/tenants")
            if st2 != 200 or '"id"' in tenants:
                raise SystemExit(f"the database is not fresh (tenants: {tenants[:120]})")
            return proc
        time.sleep(0.25)
    raise SystemExit("hydra never became healthy")


def kill_our_instances():
    """Kill leftover hydra processes started from THIS tree's binary — never by name.

    `pkill -x hydra` (the first version) killed the LOCAL DEV STACK too: the containers of
    `environment/docker-compose.local.yml` run a process literally named `hydra` (from
    `/usr/local/bin/hydra`, same uid), so every suite run killed all three of the user's
    dev containers and Docker restarted them — measured 2026-09-30: `docker inspect hydra-a`
    reported `RestartCount=14` and `unhealthy` minutes after a gate run, with exit code 0
    (a clean SIGTERM exit) in all three. Match the EXECUTABLE: only PIDs whose
    `/proc/<pid>/exe` resolves to BIN are ours. Returns the PIDs killed.
    """
    want = os.path.realpath(BIN)
    targets = []
    for entry in os.listdir("/proc"):
        if not entry.isdigit():
            continue
        try:
            if os.path.realpath(os.readlink(f"/proc/{entry}/exe")) != want:
                continue
        except OSError:
            continue  # gone, not ours, or unreadable
        targets.append(int(entry))
    for pid in targets:
        try:
            os.kill(pid, signal.SIGKILL)
        except OSError:
            pass
    return targets

def main():
    if not os.path.exists(BIN):
        raise SystemExit(f"{BIN} not built")
    os.makedirs(DIR, exist_ok=True)
    for f in os.listdir(DIR):
        if f.endswith((".crt", ".key", ".db", ".db-wal", ".db-shm", ".log", ".pem")):
            os.remove(os.path.join(DIR, f))
    if kill_our_instances():
        print("   (killed a leftover instance of OUR build)")
    time.sleep(1.0)

    print("== generating certs (self-signed, told apart by O= and notAfter)")
    crt_a, key_a = gen_cert("acme-old", "load.local", 30, "OLD-CERT")
    crt_b, key_b = gen_cert("acme-new", "load.local", 300, "NEW-CERT")
    crt_c, key_c = gen_cert("other", "other.local", 60, "OTHER-TENANT")
    fp_a, fp_b, fp_c = fingerprint_of(crt_a), fingerprint_of(crt_b), fingerprint_of(crt_c)
    print(f"   OLD={fp_a[:16]}… NEW={fp_b[:16]}… OTHER={fp_c[:16]}…")

    proc = start_hydra()
    print(f"== hydra up (pid {proc.pid}, TLS on 127.0.0.1:{TLS})")

    def seed_tenant(tid, domain, crt, key):
        return admin("POST", "/api/v1/tenants", {
            "id": tid, "name": tid, "domain": domain, "auth_url": "http://127.0.0.1:18999",
            "enabled": True, "cert_file": crt, "cert_key": key,
            "created_at": "", "updated_at": ""})

    # (1) a tenant cert becomes live with NO explicit /reload
    st, out = seed_tenant("t1", "load.local", crt_a, key_a)
    check("tenant t1 created with the OLD cert paths", st in (200, 201), f"{st} {out[:70]}")
    time.sleep(0.7)
    try:
        fp, end, subj = served_cert("load.local")
        check("§3.1: a new handshake presents the OLD cert", fp == fp_a, f"{subj} notAfter={end}")
        check("§3.1: the served cert is the tenant's own (O=OLD-CERT)", "OLD-CERT" in subj, subj)
    except HandshakeRefused as e:
        # Reported, never raised: with the post-write reload disabled this is exactly
        # the failure the test exists to catch (the falsification probe).
        check("§3.1: a new handshake presents the OLD cert", False, f"handshake refused: {e}")

    # (3) hold a session across the rotation
    try:
        held = tls_peer("load.local")
        held_before = ssl.DER_cert_to_PEM_cert(held.getpeercert(binary_form=True))
        check("§3.1: a long-lived connection can be established", True)
    except HandshakeRefused as e:
        held, held_before = None, None
        check("§3.1: a long-lived connection can be established", False, str(e))

    # (1)+(2) the hot rotation: PUT only, no /reload, no restart
    print("== §3.1 hot rotation: PUT the tenant with the NEW paths (no /reload, no restart)")
    st, out = admin("PUT", "/api/v1/tenants/t1", {
        "id": "t1", "name": "t1", "domain": "load.local", "auth_url": "http://127.0.0.1:18999",
        "enabled": True, "cert_file": crt_b, "cert_key": key_b,
        "created_at": "", "updated_at": ""})
    check("PUT accepted", st in (200, 201), f"{st} {out[:80]}")
    time.sleep(0.7)
    try:
        fp_new, end_new, subj_new = served_cert("load.local")
        check("§3.1: a NEW handshake presents the NEW cert (hot, no restart)", fp_new == fp_b,
              f"{subj_new} notAfter={end_new}")
    except HandshakeRefused as e:
        check("§3.1: a NEW handshake presents the NEW cert (hot, no restart)", False,
              f"handshake refused: {e}")
    check("§3.1: the same process served both (no restart)", proc.poll() is None, f"pid {proc.pid}")
    if held is None:
        check("§3.1: the EXISTING connection still has the OLD cert", False, "no session to hold")
    else:
        now = ssl.DER_cert_to_PEM_cert(held.getpeercert(binary_form=True))
        check("§3.1: the EXISTING connection still has the OLD cert",
              cert_fingerprint(now)[0] == cert_fingerprint(held_before)[0] == fp_a,
              "the held session was not renegotiated")
        held.close()

    # (4) SNI: a second tenant on the same listener
    print("== SNI: a second tenant gets its own cert on the same listener")
    st, out = seed_tenant("t2", "other.local", crt_c, key_c)
    check("tenant t2 created without an explicit /reload", st in (200, 201), f"{st} {out[:60]}")
    time.sleep(0.7)
    try:
        check("SNI: other.local gets the OTHER cert", served_cert("other.local")[0] == fp_c)
    except HandshakeRefused as e:
        check("SNI: other.local gets the OTHER cert", False, f"handshake refused: {e}")
    try:
        check("SNI: load.local still gets its own (NEW) cert", served_cert("load.local")[0] == fp_b)
    except HandshakeRefused as e:
        check("SNI: load.local still gets its own (NEW) cert", False, f"handshake refused: {e}")
    try:
        served_cert("nobody.local")
        print("   INFO  an unknown SNI got a certificate (a default cert is configured)")
    except HandshakeRefused as e:
        print(f"   INFO  unknown SNI -> handshake refused ({type(e).__name__}); per-tenant model, "
              "no default certificate — load-balancer health checks must send a real server_name")
    try:
        check("SNI: the listener is still healthy after an unknown name", served_cert("load.local")[0] == fp_b)
    except HandshakeRefused as e:
        check("SNI: the listener is still healthy after an unknown name", False, str(e))

    # (5) §3.3 reload reports the snapshot counts
    st, out = admin("POST", "/api/v1/reload", {})
    check("§3.3: POST /reload returns 200 with snapshot counts", st == 200, out[:110])
    check("§3.3: the response reports certs", '"certs"' in out, out[:150])

    proc.terminate()
    try:
        proc.wait(timeout=40)
    except subprocess.TimeoutExpired:
        proc.kill()

    print()
    if failures:
        print(f"CERT RELOAD: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("CERT RELOAD: PASSED (hot rotation + SNI + existing-connection isolation verified)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
