#!/usr/bin/env python3
"""`HYDRA_TRUSTED_PROXIES`, asserted: which address the per-IP dimension actually keys on.

`dev-docs/ops.md` §5 documents the allowlist, and this file pins the behaviour that the
docs (and one log message) got wrong until 2026-09-29:

  * UNSET/empty                       -> X-Forwarded-For is ignored; the bucket is the PEER;
  * a TRUSTED peer (127.0.0.1 here)   -> the rightmost non-trusted XFF entry is the client,
                                         so each XFF value gets its OWN failure bucket
                                         (that is also the forgeability hazard when the LB
                                         neither appends nor strips: rotation then defeats
                                         the per-IP dimension);
  * an UNTRUSTED peer                 -> XFF ignored again, even though the header is present
                                         (the allowlist decides, not the header);
  * a CATCH-ALL (0.0.0.0/0)           -> every candidate counts as "trusted", so
                                         `resolve_client_ip` falls back to the PEER:
                                         rotation does NOT help, and every client behind that
                                         peer SHARES one bucket (a fresh XFF + a fresh bad
                                         token is refused too). The startup warning used to
                                         claim the opposite ("forgeable"); it now says this.

The budgets come from the documented defaults: 10 failed authentications per minute per
source IP **and** per token digest independently (`HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN`).
The lockout is shortened to 15 s (from 900) so the cases can run back-to-back, and the bad
token ROTATES so the per-token dimension cannot be what trips a case.

Run: python3 integration/test_trusted_proxies.py        # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "trusted-proxy-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
TOKEN = "hydra-xff-admin-token-2026"
LOCKOUT_SECS = 15          # documented default is 900; shortened so cases can follow each other
FAIL_LIMIT = 10            # documented default (HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN)
N = FAIL_LIMIT + 3

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def start(label, admin, data, trusted):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin}",
        "HYDRA_LISTEN": f"127.0.0.1:{data}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_TENANT_API_LOCKOUT_SECS": str(LOCKOUT_SECS),
        "RUST_LOG": "info",
    })
    if trusted:
        env["HYDRA_TRUSTED_PROXIES"] = trusted
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def get(url, token=None, timeout=3):
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)


def post(url, body, token=None, headers=None, timeout=4):
    hdr = {"Content-Type": "application/json"}
    if token:
        hdr["Authorization"] = f"Bearer {token}"
    if headers:
        hdr.update(headers)
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST", headers=hdr)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status
    except urllib.error.HTTPError as e:
        e.read()
        return e.code
    except Exception:
        return 0


def wait_healthy(admin, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if get(f"http://127.0.0.1:{admin}/api/v1/health", token=TOKEN)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def bad_auth(data, xff, token):
    headers = {"X-Forwarded-For": xff} if xff else {}
    return post(f"http://127.0.0.1:{data}/tenant/t1/api/v1/auth/cache/invalidate", {}, token=token, headers=headers)


def codes(node, data, xff_for, token_for, count=N):
    out = []
    for i in range(1, count + 1):
        out.append(bad_auth(data, xff_for(i), token_for(i)))
    return out


def bring_up(label, admin, data, trusted):
    proc = start(label, admin, data, trusted)
    if not wait_healthy(admin):
        raise SystemExit(f"[trusted-proxy] CANNOT VERIFY: node {label} never became healthy")
    st = post(f"http://127.0.0.1:{admin}/api/v1/tenants",
              {"id": "t1", "name": "T", "domain": "load.local", "auth_url": "http://127.0.0.1:18997",
               "enabled": True, "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""},
              token=TOKEN)
    if st not in (200, 201):
        raise SystemExit(f"[trusted-proxy] CANNOT VERIFY: seeding the tenant -> {st}")
    time.sleep(0.3)
    return proc


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
        print(f"[trusted-proxy] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    os.makedirs(DIR, exist_ok=True)
    for f in os.listdir(DIR):
        if f.endswith((".db", ".db-wal", ".db-shm", ".log")):
            os.remove(os.path.join(DIR, f))
    killed = kill_our_instances()
    if killed:
        print(f"   (killed leftover instance(s) of OUR build: {killed})")
    time.sleep(0.5)

    procs = []
    try:
        # A: UNSET -> the header is ignored, the PEER's budget trips (rotation must not help)
        p = bring_up("a-unset", 18480, 18481, None)
        procs.append(p)
        c = codes(p, 18481, lambda i: f"203.0.113.{i}", lambda i: f"sk-a-{i}")
        check("UNSET: 14 different XFF values still trip the peer's budget", 429 in c, f"codes={c}")

        # B: a TRUSTED peer -> each XFF is its own client, so rotation never trips ...
        p = bring_up("b-trusted", 18482, 18483, "127.0.0.1")
        procs.append(p)
        c = codes(p, 18483, lambda i: f"203.0.114.{i}", lambda i: f"sk-b-{i}")
        check("TRUSTED peer: 14 different XFF values never trip a budget (per-client buckets)",
              429 not in c, f"codes={c}")
        # ... and a FIXED XFF trips its own bucket
        c2 = codes(p, 18483, lambda i: "198.51.100.7", lambda i: f"sk-b2-{i}")
        check("TRUSTED peer: a FIXED XFF trips its own budget", 429 in c2, f"codes={c2}")

        # C: an UNTRUSTED peer -> the header is ignored again (presence is not trust)
        p = bring_up("c-untrusted", 18484, 18485, "10.0.0.1")
        procs.append(p)
        c = codes(p, 18485, lambda i: f"203.0.115.{i}", lambda i: f"sk-c-{i}")
        check("UNTRUSTED peer: the allowlist decides — rotation still trips the peer", 429 in c, f"codes={c}")

        # D: CATCH-ALL -> falls back to the PEER: rotation does not help, and every client shares it
        p = bring_up("d-catchall", 18486, 18487, "0.0.0.0/0")
        procs.append(p)
        c = codes(p, 18487, lambda i: f"203.0.116.{i}", lambda i: f"sk-d-{i}")
        check("CATCH-ALL: rotation does NOT help (the client IP falls back to the peer)", 429 in c, f"codes={c}")
        fresh = bad_auth(18487, "203.0.117.250", "sk-d-fresh")
        check("CATCH-ALL: a FRESH XFF + fresh token is refused too ⇒ everyone shares one bucket",
              fresh == 429, f"HTTP {fresh}")
        log = open(os.path.join(DIR, "d-catchall.log"), errors="replace").read()
        check("CATCH-ALL: the startup warning names the sharing hazard (not 'forgeable')",
              "share one failure bucket" in log and "forgeable" not in log.split("catch-all range")[1][:400],
              "warning text")
    finally:
        for p in procs:
            if p.poll() is None:
                p.send_signal(signal.SIGKILL)
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    pass

    print()
    if failures:
        print(f"TRUSTED PROXIES: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("TRUSTED PROXIES: PASSED (peer vs XFF keying, allowlist semantics, catch-all sharing)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
