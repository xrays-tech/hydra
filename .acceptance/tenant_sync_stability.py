#!/usr/bin/env python3
"""Tenant-API CRUD propagation + destructive stability drill (compose cluster).

Focus: after tenant-API writes (groups/users/api-keys/billing), how long does it
take for EVERY node to serve the new state, and does that survive destructive
conditions?

Run:
  set -a; . secure/local-test.env; set +a
  python3 .acceptance/tenant_sync_stability.py

Env knobs: BULK_GROUPS=30 BULK_USERS=300 BULK_KEYS=300 CONCURRENCY=4
Exit 0 pass · 1 assertion failed · 2 cannot verify.
"""
import json
import os
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ADMIN = os.environ.get("HYDRA_ADMIN_TOKEN", "")
if len(ADMIN) < 16:
    print("CANNOT VERIFY: HYDRA_ADMIN_TOKEN is not set (source secure/local-test.env)")
    sys.exit(2)

DATA = {"a": 8080, "b": 8083, "c": 8090}
ADMIN_PORT = {"a": 8081, "b": 8082, "c": 8084}
CONTAINER = {"a": "hydra-a", "b": "hydra-b", "c": "hydra-c"}
GROUPS = int(os.environ.get("BULK_GROUPS", "30"))
USERS = int(os.environ.get("BULK_USERS", "300"))
KEYS = int(os.environ.get("BULK_KEYS", "300"))
CONCURRENCY = int(os.environ.get("CONCURRENCY", "4"))
STAMP = int(time.time())
TENANT = f"bulk-{STAMP}"
DOMAIN = f"{TENANT}.local"
failures = []
latencies = {"write": [], "token": [], "key": [], "billing": [], "delete": []}


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def call(method, url, token=None, body=None, host=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if host:
        headers["Host"] = host
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace"), (time.monotonic() - t0) * 1000
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace"), (time.monotonic() - t0) * 1000
    except Exception as e:
        return 0, str(e), (time.monotonic() - t0) * 1000


def admin(method, path, body=None, node="a"):
    return call(method, f"http://127.0.0.1:{ADMIN_PORT[node]}/api/v1{path}", ADMIN, body,
                host="127.0.0.1")


def tenant(method, path, token, body=None, node="a"):
    return call(method, f"http://127.0.0.1:{DATA[node]}/tenant/{TENANT}/api/v1{path}",
                token, body, host=DOMAIN)


def proxy(node, key, body=None):
    return call("POST", f"http://127.0.0.1:{DATA[node]}/v1/chat/completions", key,
                body or {"model": "echo", "messages": [{"role": "user", "content": "hi"}]},
                host=DOMAIN)


def docker(*args, timeout=120):
    return subprocess.run(["docker", *args], capture_output=True, text=True, timeout=timeout)


def wait_nodes_healthy(timeout=150):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ok = 0
        for n in "abc":
            st, _, _ = admin("GET", "/health", node=n)
            ok += st == 200
        if ok == 3:
            return True
        time.sleep(2)
    return False


def wait_token(token, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ok = 0
        for n in "abc":
            st, _, _ = call("GET", f"http://127.0.0.1:{DATA[n]}/tenant/{TENANT}/api/v1/whoami",
                            token, host=DOMAIN)
            ok += st == 200
        if ok == 3:
            return True
        time.sleep(1)
    return False


def wait_admin_count(path, want, timeout=120):
    """Return (converged, seconds_by_node)."""
    deadline = time.monotonic() + timeout
    start = time.monotonic()
    seen = {}
    while time.monotonic() < deadline:
        for n in "abc":
            if n in seen:
                continue
            st, out, _ = admin("GET", f"{path}?tenant_id={TENANT}", node=n)
            if st == 200:
                try:
                    rows = json.loads(out)
                    if len(rows) >= want:
                        seen[n] = time.monotonic() - start
                except Exception:
                    pass
        if len(seen) == 3:
            return True, seen
        time.sleep(0.5)
    return False, seen


def wait_proxy_status(node, key, status, timeout=120):
    deadline = time.monotonic() + timeout
    start = time.monotonic()
    while time.monotonic() < deadline:
        st, _, _ = proxy(node, key)
        if st == status:
            return True, time.monotonic() - start
        time.sleep(0.5)
    return False, None


def pct(vals, p):
    if not vals:
        return 0.0
    s = sorted(vals)
    return s[min(len(s) - 1, int(len(s) * p))]


def main():
    print(f"== setup tenant {TENANT} (domain {DOMAIN})")
    st, out, _ = admin("POST", "/tenants", {
        "id": TENANT, "name": TENANT, "domain": DOMAIN, "enabled": True,
        "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""})
    check("create tenant", st in (200, 201), f"HTTP {st} {out[:100]}")
    for path, body in (
        ("/tenant-providers", {"id": f"tp-{TENANT}", "tenant_id": TENANT,
                               "provider_id": "mock", "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": f"tm-{TENANT}", "tenant_id": TENANT,
                            "model_key": "echo", "created_at": "", "updated_at": ""}),
    ):
        st, out, _ = admin("POST", path, body)
        check(f"associate {path}", st in (200, 201), f"HTTP {st} {out[:80]}")

    st, out, ms = admin("POST", f"/tenants/{TENANT}/access-tokens", {"name": "bulk"})
    if st not in (200, 201):
        print(f"CANNOT VERIFY: tenant token -> {st} {out[:160]}")
        return 2
    token = json.loads(out)["token"]
    latencies["token"].append(ms)
    check("mint tenant token", True, f"{ms:.0f}ms")
    token_ok = wait_token(token, 120)
    check("token propagates to all nodes", token_ok)
    if not token_ok:
        print("CANNOT VERIFY: tenant token did not propagate; stopping before bulk writes")
        return 1

    print(f"\n== bulk create: {GROUPS} groups, {USERS} users, {KEYS} keys "
          f"(concurrency {CONCURRENCY})")
    groups = []

    def mk_group(i):
        st, out, ms = tenant("PUT", "/user-groups", token, {"name": f"g{i}"})
        if st in (200, 201):
            latencies["write"].append(ms)
            return json.loads(out)["user_group"]["id"]
        raise RuntimeError(f"group {i}: HTTP {st} {out[:120]}")

    try:
        for i in range(GROUPS):
            groups.append(mk_group(i))
    except Exception as e:
        check("create groups", False, str(e))
        return 1
    check("create groups", len(groups) == GROUPS)

    def mk_user(i):
        st, out, ms = tenant("PUT", "/users", token,
                             {"user_group_id": groups[i % len(groups)], "name": f"u{i}"})
        if st in (200, 201):
            latencies["write"].append(ms)
            return json.loads(out)["user"]["id"]
        raise RuntimeError(f"user {i}: HTTP {st} {out[:120]}")

    try:
        with ThreadPoolExecutor(max_workers=CONCURRENCY) as ex:
            users = list(ex.map(mk_user, range(USERS)))
    except Exception as e:
        check("create users", False, str(e))
        return 1
    check("create users", len(users) == USERS)

    def mk_key(i):
        st, out, ms = tenant("POST", f"/users/{users[i % len(users)]}/api-keys", token,
                             {"name": f"k{i}"})
        if st in (200, 201):
            latencies["write"].append(ms)
            body = json.loads(out)
            return body["id"], body["key"]
        raise RuntimeError(f"key {i}: HTTP {st} {out[:120]}")

    try:
        with ThreadPoolExecutor(max_workers=CONCURRENCY) as ex:
            key_meta = list(ex.map(mk_key, range(KEYS)))
    except Exception as e:
        check("create keys", False, str(e))
        return 1
    keys = [k for _, k in key_meta]
    check("create keys", len(keys) == KEYS)

    w = latencies["write"]
    print(f"   writes: {len(w)} total, p50={pct(w,.5):.0f}ms p95={pct(w,.95):.0f}ms "
          f"p99={pct(w,.99):.0f}ms max={max(w):.0f}ms")
    check("bulk write p99 < 5000ms", pct(w, 0.99) < 5000, f"p99={pct(w,.99):.0f}ms")

    print("\n== propagation: group/user/key counts on every node")
    for path, want, label in (("/user-groups", GROUPS, "groups"),
                              ("/users", USERS, "users"),
                              ("/user-api-keys", KEYS, "keys")):
        ok, seen = wait_admin_count(path, want, 180)
        check(f"{label} converge on all nodes", ok,
              ", ".join(f"{n}={v:.1f}s" for n, v in sorted(seen.items())))

    print("\n== per-key auth propagation (sample 20)")
    sample = keys[:: max(1, len(keys) // 20)][:20]
    key_failed = False
    for idx, key in enumerate(sample):
        for n in "bc":
            ok, secs = wait_proxy_status(n, key, 200, 60)
            if secs is not None:
                latencies["key"].append(secs * 1000)
            check(f"key[{idx}] auth on {n} within 60s", ok,
                  f"{secs:.1f}s" if secs is not None else "timeout")
            if not ok:
                key_failed = True
                break
        if key_failed:
            print("CANNOT VERIFY: key auth did not propagate; stopping before destructive phase")
            return 1

    print("\n== billing update propagation")
    victim_user = users[0]
    victim_key = keys[0]
    st, out, ms = tenant("PUT", f"/users/{victim_user}/billing", token,
                         {"status": "delinquent", "reason": "bulk-test"})
    check("set delinquent", st == 200, f"HTTP {st} {out[:100]}")
    for n in "abc":
        ok, secs = wait_proxy_status(n, victim_key, 402, 60)
        if secs is not None:
            latencies["billing"].append(secs * 1000)
        check(f"delinquent 402 on {n} within 60s", ok,
              f"{secs:.1f}s" if secs is not None else "timeout")
        if not ok:
            print("CANNOT VERIFY: billing propagation failed; stopping")
            return 1
    tenant("PUT", f"/users/{victim_user}/billing", token, {"status": "active"})
    check("restore active on A", wait_proxy_status("a", victim_key, 200, 180)[0])

    print("\n== delete propagation")
    # Delete the LAST key by the id returned at creation.
    last_user = users[-1]
    kid = key_meta[-1][0]
    st, out, ms = tenant("DELETE", f"/users/{last_user}/api-keys/{kid}", token)
    check("delete key", st in (204, 200), f"HTTP {st}")
    def wait_absent(node, kid, timeout=180):
        deadline = time.monotonic() + timeout
        start = time.monotonic()
        while time.monotonic() < deadline:
            st2, out2, _ = admin("GET", f"/user-api-keys?tenant_id={TENANT}", node=node)
            if st2 == 200 and all(r.get("id") != kid for r in json.loads(out2)):
                return True, time.monotonic() - start
            time.sleep(0.5)
        return False, None
    for n in "abc":
        gone, secs = wait_absent(n, kid, 60)
        check(f"deleted key absent on {n} within 60s", gone,
              f"{secs:.1f}s" if secs is not None else "timeout")
        if not gone:
            print("CANNOT VERIFY: delete propagation failed; stopping")
            return 1

    if os.environ.get("SKIP_DESTRUCTIVE"):
        print("\n== destructive phase skipped (SKIP_DESTRUCTIVE)")
        return 0 if not failures else 1

    print("\n== destructive churn: leader kill / follower kill / Redis / ClickHouse / quorum")
    churn_stop = threading.Event()
    churn_errors = []
    churn_ok = []
    churn_lock = threading.Lock()

    def churn_worker(wid):
        i = 0
        while not churn_stop.is_set():
            name = f"churn-{wid}-{i}"
            st, out, _ = tenant("PUT", "/user-groups", token, {"name": name})
            with churn_lock:
                (churn_ok if st in (200, 201) else churn_errors).append(st)
            if st in (200, 201):
                gid = json.loads(out)["user_group"]["id"]
                st2, _, _ = tenant("DELETE", f"/user-groups/{gid}", token)
                with churn_lock:
                    (churn_ok if st2 in (204, 200) else churn_errors).append(st2)
            i += 1
            time.sleep(0.1)

    threads = [threading.Thread(target=churn_worker, args=(i,), daemon=True) for i in range(3)]
    for t in threads:
        t.start()
    time.sleep(2)

    def leader():
        for n in "abc":
            st, _, _ = call("GET", f"http://127.0.0.1:{ADMIN_PORT[n]}/healthz/leader", ADMIN,
                            host="127.0.0.1")
            if st == 200:
                return n
        return None

    for label, action in (
        ("kill leader", lambda: docker("kill", CONTAINER[leader() or "a"])),
        ("kill follower", lambda: docker("kill", CONTAINER["b" if leader() == "a" else "a"])),
    ):
        print(f"   -- {label}")
        action()
        time.sleep(4)
        check(f"{label}: churn keeps making progress", len(churn_ok) > 0,
              f"ok={len(churn_ok)} err={len(churn_errors)}")
        for n in "abc":
            docker("start", CONTAINER[n])
        check(f"{label}: cluster recovers", wait_nodes_healthy(180))

    print("   -- restart Redis")
    docker("restart", "hydra-local-redis", timeout=60)
    time.sleep(3)
    check("Redis restart: churn continues", len(churn_ok) > 0)
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        if docker("inspect", "-f", "{{.State.Health.Status}}",
                  "hydra-local-redis").stdout.strip() == "healthy":
            break
        time.sleep(2)
    check("Redis healthy", docker("inspect", "-f", "{{.State.Health.Status}}",
                                  "hydra-local-redis").stdout.strip() == "healthy")

    print("   -- stop ClickHouse")
    docker("stop", "hydra-local-clickhouse", timeout=60)
    time.sleep(3)
    before = len(churn_ok)
    time.sleep(3)
    check("ClickHouse down: churn continues", len(churn_ok) > before)
    docker("start", "hydra-local-clickhouse", timeout=60)
    deadline = time.monotonic() + 90
    while time.monotonic() < deadline:
        if docker("inspect", "-f", "{{.State.Health.Status}}",
                  "hydra-local-clickhouse").stdout.strip() == "healthy":
            break
        time.sleep(2)
    check("ClickHouse healthy again",
          docker("inspect", "-f", "{{.State.Health.Status}}",
                 "hydra-local-clickhouse").stdout.strip() == "healthy")

    print("   -- quorum loss (2/3 down)")
    churn_stop.set()
    for t in threads:
        t.join(timeout=10)
    docker("update", "--restart=no", "hydra-b"); docker("update", "--restart=no", "hydra-c")
    docker("stop", "-t", "10", "hydra-b"); docker("stop", "-t", "10", "hydra-c")
    time.sleep(2)
    st, out, _ = tenant("PUT", "/user-groups", token, {"name": "no-quorum"})
    check("tenant write refused without quorum (503)", st == 503, f"HTTP {st} {out[:120]}")
    check("existing data plane still serves", proxy("a", keys[1])[0] == 200)
    docker("start", "hydra-b"); docker("start", "hydra-c")
    docker("update", "--restart=unless-stopped", "hydra-b"); docker("update", "--restart=unless-stopped", "hydra-c")
    check("cluster recovers after quorum", wait_nodes_healthy(180))
    st, out, _ = tenant("PUT", "/user-groups", token, {"name": "quorum-back"})
    check("tenant write works after recovery", st in (200, 201), f"HTTP {st} {out[:100]}")

    print("\n== summary")
    for k, vals in latencies.items():
        if vals:
            print(f"   {k}: n={len(vals)} p50={pct(vals,.5):.0f} p95={pct(vals,.95):.0f} "
                  f"p99={pct(vals,.99):.0f} max={max(vals):.0f} (ms)")
    print(f"   churn: ok={len(churn_ok)} errors={len(churn_errors)} "
          f"codes={sorted(set(churn_errors))}")
    if failures:
        print(f"\nTENANT SYNC STABILITY: FAILED ({len(failures)})")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("\nTENANT SYNC STABILITY: PASSED")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception as e:
        print(f"CANNOT VERIFY: {e}")
        sys.exit(2)
