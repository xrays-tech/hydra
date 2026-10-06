#!/usr/bin/env python3
"""Cluster HA, asserted: forwarding, registry, lease failover, and edge transparency.

Rounds 72/73 verified these by hand; this file is the executable contract, so a
regression in the cluster path turns CI red. It is deliberately FAST: the leader lease
is shortened to 3 s (`HYDRA_LEADER_LEASE_MS` accepts 1000..600000), which makes a
promotion happen in a few seconds instead of ~18 s, so the whole drill fits in CI.

Claims under test (`dev-docs/cluster.md`):
  1. a `leader`-role node forwards admin WRITES to the active node (the lease holder is
     the only writer) — and the write really lands on the active node;
  2. the registry/`cluster/status` names both nodes and the lease holder;
  3. after the active dies the standby is PROMOTED and then accepts writes locally;
  4. an EDGE serves no admin API (`/api/v1/health` 404) but a token-free `/healthz`, and
     `/metrics` needs the admin token (the evidence behind decision D-8);
  5. an edge keeps serving its data plane across the failover (it serves from its own
     snapshot) and re-points its control polling by itself afterwards.

Requires a Redis (any instance — the DB it uses is FLUSHED, because a shared test Redis
is not a private one: a stale lease/registry entry makes a fresh fleet follow a dead
lease holder and fail closed, which once looked like a pile of product defects).

  python3 integration/test_cluster_ha.py            # needs target/debug/hydra
  HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6379 python3 integration/test_cluster_ha.py

Exit 0 pass · 1 an assertion failed · 2 could not verify (no Redis / binary missing).
"""
import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "cluster-ha-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN_TOKEN = "hydra-ha-admin-token-2026"
CLUSTER_TOKEN = "hydra-ha-cluster-token-2026"
LEASE_MS = 3000
POLL_MS = 250
# Ports chosen clear of the dev stack (8080-8084/8090) and of the other suites.
A_ADMIN, A_DATA = 18392, 18393
B_ADMIN, B_DATA = 18394, 18395
E_ADMIN, E_DATA = 18396, 18397
DEAD_CH = "http://127.0.0.1:18999"   # the cluster roles need a clickhouse sink configured;
                                     # a dead URL keeps this test away from any real CH.
REDIS_BASE = os.environ.get("HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380").rstrip("/")
REDIS_DB = int(os.environ.get("HYDRA_HA_REDIS_DB", "47"))

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def redis_endpoint():
    host = REDIS_BASE.split("://", 1)[-1].split("/")[0]
    if ":" in host:
        h, p = host.split(":", 1)
        return h, int(p)
    return host, 6379


def redis_cmd(sock, *parts):
    payload = ("*%d\r\n" % len(parts)).encode()
    for part in parts:
        b = str(part).encode()
        payload += b"$%d\r\n" % len(b) + b + b"\r\n"
    sock.sendall(payload)
    return sock.recv(256)


def flush_db():
    host, port = redis_endpoint()
    try:
        sock = socket.create_connection((host, port), timeout=5)
    except OSError as e:
        return None, f"{host}:{port} unreachable ({e})"
    redis_cmd(sock, "SELECT", REDIS_DB)
    before = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    redis_cmd(sock, "FLUSHDB")
    after = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    sock.close()
    return f"redis://{host}:{port}/{REDIS_DB}", f"db {REDIS_DB}: keys {before} -> {after}"


def get(url, token=None, timeout=3):
    headers = {"Authorization": f"Bearer {token}"} if token else {}
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)


def post(url, body, token=None, timeout=5):
    data = json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, method="POST", headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace")
    except Exception as e:
        return 0, str(e)


def start(redis_url, label, role, admin, data, control_url, node_id):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN, "HYDRA_CLUSTER_TOKEN": CLUSTER_TOKEN,
        "HYDRA_ROLE": role, "HYDRA_NODE_ID": node_id,
        "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin}", "HYDRA_LISTEN": f"127.0.0.1:{data}",
        "HYDRA_REDIS_URL": f"{redis_url}", "HYDRA_REDIS_MODE": "single",
        "HYDRA_CONTROL_URL": control_url, "HYDRA_PUBLIC_URL": f"http://127.0.0.1:{admin}",
        "HYDRA_LEADER_LEASE_MS": str(LEASE_MS), "HYDRA_CONTROL_POLL_MS": str(POLL_MS),
        "HYDRA_USAGE_SINK": "clickhouse", "HYDRA_CLICKHOUSE_URL": DEAD_CH,
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def leader_probe(admin):
    return get(f"http://127.0.0.1:{admin}/healthz/leader")[0]


def wait_for(fn, budget=25.0, step=0.25):
    deadline = time.time() + budget
    while time.time() < deadline:
        if fn():
            return True
        time.sleep(step)
    return False


def proxied(data_port, host="load.local"):
    """A data-plane request through the given listener.

    The point of these probes is ROUTING, not the upstream's health: this drill seeds a
    tenant whose `auth_url` and provider endpoint are deliberately dead ports, so a
    request that the node can route answers 503/502 — while 404 means "no such tenant in
    my snapshot" and 0 means "the listener is gone". So the assertions below are
    "not 404 and not 0", which is exactly what proves snapshot-based serving.
    """
    data = json.dumps({"model": "echo", "messages": [{"role": "user", "content": "hi"}]}).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{data_port}/v1/chat/completions", data=data,
                                 method="POST",
                                 headers={"Host": host, "Content-Type": "application/json",
                                          "Authorization": "Bearer sk-tenant-1"})
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return 0


def metric(admin, name):
    st, body = get(f"http://127.0.0.1:{admin}/metrics", token=ADMIN_TOKEN)
    if st != 200:
        return 0
    total = 0
    for line in body.splitlines():
        if line.startswith(name):
            try:
                total += int(float(line.split()[-1]))
            except (ValueError, IndexError):
                pass
    return total


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
        print(f"[cluster-ha] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    os.makedirs(DIR, exist_ok=True)
    for f in os.listdir(DIR):
        if f.endswith((".db", ".db-wal", ".db-shm", ".log")):
            os.remove(os.path.join(DIR, f))
    killed = kill_our_instances()
    if killed:
        print(f"   (killed leftover instance(s) of OUR build: {killed})")
    time.sleep(0.5)

    redis_url, flush_note = flush_db()
    if redis_url is None:
        print(f"[cluster-ha] CANNOT VERIFY: {flush_note}", file=sys.stderr)
        return 2
    print(f"== redis {redis_url} ({flush_note})")

    proc_a = proc_b = proc_e = None
    try:
        proc_a = start(redis_url, "a-active", "leader", A_ADMIN, A_DATA, f"http://127.0.0.1:{A_ADMIN}", "ha-a")
        if not wait_for(lambda: leader_probe(A_ADMIN) == 200, budget=25):
            log = open(os.path.join(DIR, "a-active.log"), errors="replace").read()
            # A binary built without the cluster features cannot start these roles at all:
            # say that plainly instead of reporting a mysterious cluster failure (the CI
            # step builds `--features server,cluster-redis,usage-clickhouse` first, and so
            # must anyone running this locally).
            if "cluster-redis' cargo feature" in log:
                print("[cluster-ha] CANNOT VERIFY: the binary lacks the cluster features; rebuild with\n"
                      "             cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra",
                      file=sys.stderr)
                return 2
            print("[cluster-ha] CANNOT VERIFY: the first node never became the active leader", file=sys.stderr)
            print(log[-600:], file=sys.stderr)
            return 2
        check("node A holds the leader lease", True, f"(lease {LEASE_MS} ms)")
    except Exception as e:  # pragma: no cover - defensive
        print(f"[cluster-ha] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2

    try:
        # seed a tenant + provider (endpoint is deliberately dead: 502 still proves routing)
        for path, body in [
            ("providers", {"id": "p1", "key": "p1", "name": "P", "endpoint": "http://127.0.0.1:18998",
                           "weight": 1, "created_at": "", "updated_at": ""}),
            ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1", "status": 1,
                                 "created_at": "", "updated_at": ""}),
            ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
            ("tenants", {"id": "t1", "name": "T", "domain": "load.local",
                         "auth_url": "http://127.0.0.1:18997", "enabled": True,
                         "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
            ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                                  "created_at": "", "updated_at": ""}),
            ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                               "created_at": "", "updated_at": ""}),
        ]:
            st, out = post(f"http://127.0.0.1:{A_ADMIN}/api/v1/{path}", body, token=ADMIN_TOKEN)
            if st not in (200, 201):
                print(f"[cluster-ha] CANNOT VERIFY: seeding {path} -> {st} {out[:120]}", file=sys.stderr)
                return 2
        post(f"http://127.0.0.1:{A_ADMIN}/api/v1/reload", {}, token=ADMIN_TOKEN)

        proc_b = start(redis_url, "b-standby", "leader", B_ADMIN, B_DATA,
                       f"http://127.0.0.1:{A_ADMIN}", "ha-b")
        wait_for(lambda: get(f"http://127.0.0.1:{B_ADMIN}/api/v1/health", token=ADMIN_TOKEN)[0] == 200, budget=25)
        wait_for(lambda: leader_probe(B_ADMIN) == 503, budget=10)
        check("node B is up and reports standby", leader_probe(B_ADMIN) == 503, f"/healthz/leader={leader_probe(B_ADMIN)}")

        # 1) forwarding
        st, out = post(f"http://127.0.0.1:{B_ADMIN}/api/v1/providers",
                       {"id": "via-standby", "key": "vs", "name": "VS",
                        "endpoint": "http://127.0.0.1:18998", "weight": 1,
                        "created_at": "", "updated_at": ""}, token=ADMIN_TOKEN)
        check("a write to the STANDBY is accepted (forwarded)", st in (200, 201), f"HTTP {st}")
        st, body = get(f"http://127.0.0.1:{A_ADMIN}/api/v1/providers", token=ADMIN_TOKEN)
        check("the forwarded write is visible on the ACTIVE node", "via-standby" in body, f"HTTP {st}")

        # 2) registry
        st, body = get(f"http://127.0.0.1:{A_ADMIN}/api/v1/cluster/status", token=ADMIN_TOKEN)
        check("cluster/status names the lease holder", "ha-a" in body and "lease_holder" in body, body[:90])

        # 4) edge surface
        proc_e = start(redis_url, "e-edge", "edge", E_ADMIN, E_DATA, f"http://127.0.0.1:{A_ADMIN}", "ha-edge")
        wait_for(lambda: get(f"http://127.0.0.1:{E_ADMIN}/healthz")[0] == 200, budget=20)
        check("edge /healthz answers token-free", get(f"http://127.0.0.1:{E_ADMIN}/healthz")[0] == 200)
        check("edge /api/v1/health is 404 (no admin API ⇒ healthchecks must use /healthz)",
              get(f"http://127.0.0.1:{E_ADMIN}/api/v1/health")[0] == 404)
        check("edge /metrics is refused without a token and served with one (D-8 evidence)",
              get(f"http://127.0.0.1:{E_ADMIN}/metrics")[0] == 401
              and get(f"http://127.0.0.1:{E_ADMIN}/metrics", token=ADMIN_TOKEN)[0] == 200)

        # 5) the edge serves from its own snapshot
        unknown = proxied(E_DATA, host="nope.local")
        check("control: an unknown domain on the edge is 404 (that is what 'no config' looks like)",
              unknown == 404, f"HTTP {unknown} for Host: nope.local")
        served = wait_for(lambda: proxied(E_DATA) not in (0, 404), budget=20)
        check("the edge serves the data plane (routed the request from its snapshot)", served,
              f"HTTP {proxied(E_DATA)} (503/502 = routed, upstream/auth is dead on purpose)")

        # 3)+5) failover, with the edge probed across it
        t0 = time.time()
        proc_a.kill()
        proc_a.wait(timeout=10)
        promoted = wait_for(lambda: leader_probe(B_ADMIN) == 200, budget=20)
        took = time.time() - t0
        check("the standby is promoted after the active dies", promoted, f"{took:.1f}s (lease {LEASE_MS} ms)")
        probes = [proxied(E_DATA) for _ in range(10)]
        check("the edge kept serving across the failover (never 404/refused)",
              all(c not in (0, 404) for c in probes), f"statuses={sorted(set(probes))}")
        st, _ = post(f"http://127.0.0.1:{B_ADMIN}/api/v1/providers",
                     {"id": "post-failover", "key": "pf", "name": "PF",
                      "endpoint": "http://127.0.0.1:18998", "weight": 1,
                      "created_at": "", "updated_at": ""}, token=ADMIN_TOKEN)
        check("the promoted node accepts writes locally", st in (200, 201), f"HTTP {st}")

        # 5b) the edge re-points its polling at the new active on its own
        before = metric(E_ADMIN, "hydra_control_snapshot_version")
        post(f"http://127.0.0.1:{B_ADMIN}/api/v1/reload", {}, token=ADMIN_TOKEN)
        advanced = wait_for(lambda: metric(E_ADMIN, "hydra_control_snapshot_version") > before, budget=20)
        check("the edge follows the NEW active by itself (snapshot version advances)", advanced,
              f"{before} -> {metric(E_ADMIN, 'hydra_control_snapshot_version')}")
    finally:
        for p in (proc_e, proc_b, proc_a):
            if p and p.poll() is None:
                p.send_signal(signal.SIGKILL)
                try:
                    p.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    pass

    print()
    if failures:
        print(f"CLUSTER HA: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("CLUSTER HA: PASSED (forwarding + registry + failover + edge transparency)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
