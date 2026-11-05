#!/usr/bin/env python3
"""Compose-cluster stability + destructive drills.

Targets the three-node stack in environment/docker-compose.local.yml, not the
in-process test fixtures. The upstream is the compose `mock-llm` service, so the
drills spend no real provider credits.

Run:
  set -a; . secure/local-test.env; set +a
  python3 integration/test_compose_stability.py

Exit 0 pass · 1 assertion failed · 2 could not verify.
"""
import json
import os
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ADMIN = os.environ.get("HYDRA_ADMIN_TOKEN", "")
if len(ADMIN) < 16:
    print("CANNOT VERIFY: HYDRA_ADMIN_TOKEN is not set (source secure/local-test.env)")
    sys.exit(2)

DATA = {"a": 8080, "b": 8083, "c": 8090}
ADMIN_PORT = {"a": 8081, "b": 8082, "c": 8084}
CONTAINER = {"a": "hydra-a", "b": "hydra-b", "c": "hydra-c"}
TENANT = "local"
DOMAIN = "localhost"
MODEL = "echo"
CLIENT_KEY = (ROOT / "secure" / "local-client-key").read_text().strip()
COMPOSE = ["docker", "compose", "-f", str(ROOT / "environment" / "docker-compose.local.yml")]
failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def call(method, url, token=None, body=None, host=DOMAIN, timeout=10):
    data = json.dumps(body).encode() if body is not None else None
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


def admin(method, path, body=None, node="a"):
    return call(method, f"http://127.0.0.1:{ADMIN_PORT[node]}/api/v1{path}", ADMIN, body,
                host="127.0.0.1")


def proxy(node="c", key=None):
    return call("POST", f"http://127.0.0.1:{DATA[node]}/v1/chat/completions",
                key or CLIENT_KEY, {"model": MODEL, "messages": [{"role": "user", "content": "hi"}]})


def docker(*args, **kw):
    return subprocess.run(["docker", *args], capture_output=True, text=True, timeout=kw.pop("timeout", 120))


def compose(*args, **kw):
    return subprocess.run([*COMPOSE, *args], capture_output=True, text=True, timeout=kw.pop("timeout", 180))


def wait_healthy(nodes="abc", timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ok = 0
        for n in nodes:
            st, _ = admin("GET", "/health", node=n)
            if st == 200:
                ok += 1
        if ok == len(nodes):
            return True
        time.sleep(2)
    return False


def wait_proxy(node="c", timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        st, _ = proxy(node)
        if st == 200:
            return True
        time.sleep(2)
    return False


def restart_mode(container, mode):
    docker("update", f"--restart={mode}", container)


def stop_keep(container):
    restart_mode(container, "no")
    docker("stop", "-t", "10", container)


def start_again(container):
    docker("start", container)
    restart_mode(container, "unless-stopped")


def mint_tenant_token():
    name = f"stability-{int(time.time())}"
    st, out = admin("POST", f"/tenants/{TENANT}/access-tokens", {"name": name})
    if st not in (200, 201):
        raise SystemExit(f"CANNOT VERIFY: mint tenant token -> {st} {out[:200]}")
    return json.loads(out)["token"]


def load(duration, rps=20, nodes=("a", "b", "c")):
    """Return (total, ok, p99_ms, codes)."""
    total = []; lock = threading.Lock(); stop = threading.Event()
    def worker(node):
        while not stop.is_set():
            t0 = time.monotonic()
            st, body = proxy(node)
            dt = (time.monotonic() - t0) * 1000
            with lock:
                total.append((dt, st, body[:80]))
            time.sleep(max(0, 1.0 / rps))
    threads = [threading.Thread(target=worker, args=(n,), daemon=True) for n in nodes]
    for t in threads: t.start()
    time.sleep(duration)
    stop.set()
    for t in threads: t.join(timeout=10)
    total.sort()
    ok = sum(1 for _, st, _ in total if st == 200)
    p99 = total[int(len(total) * 0.99)][0] if total else 0
    codes = {}
    for _, st, _ in total:
        codes[st] = codes.get(st, 0) + 1
    return len(total), ok, p99, codes


def leader_node():
    for n in "abc":
        st, out = call("GET", f"http://127.0.0.1:{ADMIN_PORT[n]}/healthz/leader",
                       ADMIN, host="127.0.0.1")
        if st == 200:
            return n
    return None


def wait_token(token, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ok = 0
        for n in "abc":
            st, _ = call("GET", f"http://127.0.0.1:{DATA[n]}/tenant/{TENANT}/api/v1/whoami",
                         token)
            if st == 200:
                ok += 1
        if ok == 3:
            return True
        time.sleep(2)
    return False


def wait_docker_healthy(name, timeout=90):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        st = docker("inspect", "-f", "{{.State.Health.Status}}", name).stdout.strip()
        if st == "healthy":
            return True
        time.sleep(2)
    return False


def main():
    print("== preflight: compose services + 3 healthy nodes")
    ps = docker("ps", "--format", "{{.Names}} {{.Status}}")
    for name in ("hydra-a", "hydra-b", "hydra-c", "hydra-local-redis",
                 "hydra-local-clickhouse", "hydra-local-mock-llm"):
        check(f"{name} running", name in ps.stdout, "docker ps")
    check("all three nodes healthy", wait_healthy("abc", 120))
    for n in "abc":
        st, body = proxy(n)
        check(f"proxy {n} serves 200", st == 200, f"HTTP {st} {body[:80]}")

    tenant_token = mint_tenant_token()
    token_ok = wait_token(tenant_token, 120)
    check("minted tenant token propagates to all three nodes", token_ok,
          "whoami on A/B/C within 120s")

    print("\n== baseline load: 8s across all three nodes")
    total, ok, p99, codes = load(8, 20)
    check("baseline load 100% 2xx", total > 50 and ok == total,
          f"{ok}/{total} codes={codes}")
    check("baseline p99 < 1000ms", p99 < 1000, f"p99={p99:.0f}ms")

    print("\n== destructive 1: kill the raft leader under load")
    leader = leader_node()
    check("leader known", leader in CONTAINER, f"leader={leader}")
    victim = leader if leader in CONTAINER else "a"
    restart_mode(CONTAINER[victim], "no")
    result = {}
    def kill_leader():
        time.sleep(2)
        docker("kill", CONTAINER[victim])
        result["killed"] = time.monotonic()
    t = threading.Thread(target=kill_leader, daemon=True); t.start()
    total, ok, p99, codes = load(8, 20, nodes=tuple(n for n in "abc" if n != victim))
    t.join(timeout=10)
    check("traffic survives leader kill (>=90% 2xx)", total > 30 and ok / total >= 0.90,
          f"{ok}/{total} codes={codes}")
    start_again(CONTAINER[victim])
    check("leader restarts and cluster is healthy", wait_healthy("abc", 120))
    check("proxy works after leader restart", wait_proxy("a", 60))

    print("\n== destructive 2: kill a follower under load")
    follower = next((n for n in "abc" if n != victim), "b")
    stop_keep(CONTAINER[follower])
    total, ok, p99, codes = load(6, 20, nodes=tuple(n for n in "abc" if n != follower))
    check("traffic survives follower kill (100% 2xx)", total > 20 and ok == total,
          f"{ok}/{total} codes={codes}")
    start_again(CONTAINER[follower])
    check("follower restarts and cluster is healthy", wait_healthy("abc", 120))

    print("\n== destructive 3: restart Redis backbone")
    docker("restart", "hydra-local-redis", timeout=60)
    time.sleep(2)
    total, ok, p99, codes = load(5, 10)
    check("traffic during Redis restart is 2xx or explicit 5xx (no transport errors)",
          total > 10 and all(st in (200, 429, 502, 503) for st in codes.keys()),
          f"{ok}/{total} codes={codes}")
    check("Redis recovers", docker("inspect", "-f", "{{.State.Health.Status}}",
                                  "hydra-local-redis").stdout.strip() == "healthy")
    check("traffic after Redis recovery", wait_proxy("c", 60))

    print("\n== destructive 4: stop ClickHouse usage sink")
    docker("stop", "hydra-local-clickhouse", timeout=60)
    total, ok, p99, codes = load(4, 10)
    check("data plane keeps serving while ClickHouse is down", total > 10 and ok == total,
          f"{ok}/{total} codes={codes}")
    docker("start", "hydra-local-clickhouse", timeout=60)
    check("ClickHouse restarts healthy", wait_docker_healthy("hydra-local-clickhouse", 90))

    print("\n== destructive 5: quorum loss (2 of 3 nodes down)")
    stop_keep(CONTAINER["b"]); stop_keep(CONTAINER["c"])
    time.sleep(2)
    st, body = proxy("a")
    check("remaining node serves last-known-good data", st == 200, f"HTTP {st} {body[:100]}")
    st, body = admin("PUT", "/tenants/local", {
        "id": "local", "name": "Local Stability Tenant", "domain": "localhost",
        "enabled": True, "cert_key": None, "cert_file": None,
        "created_at": "", "updated_at": ""})
    check("management write is refused without quorum (503)", st == 503,
          f"HTTP {st} {body[:120]}")
    start_again(CONTAINER["b"]); start_again(CONTAINER["c"])
    check("cluster recovers after quorum returns", wait_healthy("abc", 150))
    check("proxy works after quorum recovery", wait_proxy("b", 60))

    print("\n== final state")
    for n in "abc":
        st, body = proxy(n)
        check(f"final proxy {n}", st == 200, f"HTTP {st} {body[:80]}")
    if failures:
        print(f"\nCOMPOSE STABILITY: FAILED ({len(failures)})")
        for f in failures:
            print(f"  - {f}")
        return 1
    print("\nCOMPOSE STABILITY: PASSED")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except SystemExit:
        raise
    except Exception as e:
        print(f"CANNOT VERIFY: {e}")
        sys.exit(2)
