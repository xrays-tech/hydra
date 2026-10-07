#!/usr/bin/env python3
"""The tenant API's documented rate limits, asserted.

`dev-docs/ops.md` §5 documents three separate budgets and their exact semantics:
  * `HYDRA_TENANT_API_RATE_LIMIT_PER_MIN` (60) — per-tenant cap on AUTHORISED requests,
    and it counts "every authenticated request the node accepts for the tenant,
    INCLUDING ones it then rejects" (the drill shows this with `/usage` calls that answer
    400: three of them consume a cap of 3, the fourth is 429);
  * `HYDRA_TENANT_API_INVALIDATE_PER_MIN` (10) — per-tenant cap on
    `auth/cache/invalidate`, which fans out to the whole fleet;
  * a `0`/garbage value must fall back to the default rather than refuse everything (a
    typo must not be a self-inflicted denial of service).
Also pinned: the 429 carries `Retry-After`, and a VALID token is never refused by the
FAILURE lockout (the B1 contract: only the authentication-failure path consults it).

The caps are shortened here so the assertions are quick (the drill prints the values it
used); the default values are what the docs quote. Reaching the authorised paths needs a
tenant access token, which the admin API sets as `access_token` (stored hashed, ≥16 chars).

Run: python3 integration/test_tenant_api_limits.py     # needs target/debug/hydra
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
from _mock_clickhouse import MockClickHouse
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "tenant-api-limits-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN_TOKEN = "hydra-limits-admin-token-2026"
TENANT_TOKEN = "tenant-selfservice-token-0001"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def call(method, url, token=None, body=None, timeout=5):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, dict(r.headers), r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode(errors="replace")
    except Exception as e:
        return 0, {}, str(e)


def start(label, admin, data, rate, invalidate):
    env = dict(os.environ)
    env.update(usage_env("clickhouse", MOCK.url))
    env.update({
        "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin}",
        "HYDRA_LISTEN": f"127.0.0.1:{data}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_TENANT_API_RATE_LIMIT_PER_MIN": str(rate),
        "HYDRA_TENANT_API_INVALIDATE_PER_MIN": str(invalidate),
        "HYDRA_TENANT_API_AUTH_FAIL_LIMIT_PER_MIN": "10",
        "HYDRA_TENANT_API_LOCKOUT_SECS": "15",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(admin, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{admin}/api/v1/health", token=ADMIN_TOKEN)[0] == 200:
            return True
        time.sleep(0.25)
    return False


# `GET /usage` is how this drill spends the tenant's request budget, and the handler checks the
# READER before it validates the window — so a node with no reader answers 503 instead of the 400
# the legs expect. A ClickHouse double gives the node a reader; nothing is written to it.
MOCK = MockClickHouse()


def bring_up(label, admin, data, rate, invalidate):
    proc = start(label, admin, data, rate, invalidate)
    if not wait_healthy(admin):
        raise SystemExit(f"[tenant-limits] CANNOT VERIFY: node {label} never became healthy")
    st, _, out = call("POST", f"http://127.0.0.1:{admin}/api/v1/tenants", token=ADMIN_TOKEN, body={
        "id": "t1", "name": "T", "domain": "load.local", "auth_url": "http://127.0.0.1:18997",
        "enabled": True, "access_token": TENANT_TOKEN, "cert_key": None, "cert_file": None,
        "created_at": "", "updated_at": ""})
    if st not in (200, 201):
        raise SystemExit(f"[tenant-limits] CANNOT VERIFY: seeding the tenant -> {st} {out[:120]}")
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
        print(f"[tenant-limits] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    os.makedirs(DIR, exist_ok=True)
    for f in os.listdir(DIR):
        if f.endswith((".db", ".db-wal", ".db-shm", ".log")):
            os.remove(os.path.join(DIR, f))
    killed = kill_our_instances()
    if killed:
        print(f"   (killed leftover instance(s) of OUR build: {killed})")
    time.sleep(0.5)

    MOCK.start()
    procs = []
    try:
        # A: the authorised-request cap, and the documented "counts rejected requests" clause
        p = bring_up("a-rate", 18490, 18491, 3, 10)
        procs.append(p)
        codes = [call("GET", f"http://127.0.0.1:18491/tenant/t1/api/v1/usage", token=TENANT_TOKEN)[0]
                 for _ in range(6)]
        check("AUTHORISED cap (3): 429 past the configured rate", 429 in codes, f"codes={codes}")
        check("AUTHORISED cap counts requests the node then REJECTED (the 400s consumed it)",
              codes[:3] == [400, 400, 400] and codes[3] == 429, f"codes={codes}")
        st, hdr, _ = call("GET", "http://127.0.0.1:18491/tenant/t1/api/v1/usage", token=TENANT_TOKEN)
        retry = hdr.get("Retry-After") or hdr.get("retry-after")
        check("the throttle carries Retry-After (actionable for an operator)", st == 429 and bool(retry),
              f"HTTP {st}, Retry-After={retry}")
        _, _, metrics = call("GET", "http://127.0.0.1:18490/metrics", token=ADMIN_TOKEN)
        check("hydra_tenant_api_throttled_total{scope=\"tenant\"} is exported",
              'hydra_tenant_api_throttled_total{scope="tenant"}' in metrics)
        p.send_signal(signal.SIGKILL); p.wait(timeout=10)

        # B: the invalidation cap (invalidation fans out to the fleet, so it is capped lower)
        p = bring_up("b-invalidate", 18492, 18493, 60, 2)
        procs.append(p)
        inv = [call("POST", f"http://127.0.0.1:18493/tenant/t1/api/v1/auth/cache/invalidate",
                    token=TENANT_TOKEN, body={})[0] for _ in range(5)]
        check("INVALIDATE cap (2): the third and later invalidations are 429",
              inv[:2] == [200, 200] and set(inv[2:]) == {429}, f"codes={inv}")
        p.send_signal(signal.SIGKILL); p.wait(timeout=10)

        # D: a VALID token is never refused by the FAILURE lockout (B1)
        p = bring_up("d-independent", 18494, 18495, 60, 10)
        procs.append(p)
        for i in range(12):
            call("POST", f"http://127.0.0.1:18495/tenant/t1/api/v1/auth/cache/invalidate",
                 token=f"sk-bad-{i}", body={})
        st, _, _ = call("GET", "http://127.0.0.1:18495/tenant/t1/api/v1/whoami", token=TENANT_TOKEN)
        check("a VALID token still answers 200 after the failure budget is exhausted", st == 200, f"HTTP {st}")
        p.send_signal(signal.SIGKILL); p.wait(timeout=10)

        # E: 0 falls back to the default (a typo must not be a self-inflicted DoS)
        p = bring_up("e-zeros", 18496, 18497, 0, 0)
        procs.append(p)
        st, _, _ = call("GET", "http://127.0.0.1:18497/tenant/t1/api/v1/whoami", token=TENANT_TOKEN)
        check("a 0 cap falls back to the documented default (the request is served)", st == 200, f"HTTP {st}")
    finally:
        MOCK.stop()
        for p in procs:
            if p.poll() is None:
                p.send_signal(signal.SIGKILL)
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    pass

    print()
    if failures:
        print(f"TENANT-API LIMITS: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("TENANT-API LIMITS: PASSED (authorised cap, invalidate cap, Retry-After, failure-lockout isolation, 0-fallback)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
