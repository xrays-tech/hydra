#!/usr/bin/env python3
"""Cluster rate limits over Redis: SHARED across nodes, and FAIL-OPEN when Redis dies.

`ops.md` §12 states cluster mode gives you "multi-instance with **shared rate-limit
counters**", §13.5 says the data plane keeps serving through a Redis outage while election is
fail-closed, and §13.4/`redis/rate_limit.rs` document the deliberate opposite for limits: the
Redis rate-limit path is **hard-coded fail-open** ("there is NO env override"). `redis/mod.rs`
adds why the fail-open branch can fire at all — a 500 ms per-command timeout
(`HYDRA_REDIS_COMMAND_TIMEOUT_MS`) turns a black-holed socket into an ordinary error, because
"a hang never produces an `Err`, so the documented fail-open branch cannot fire either".

None of that had ever been observed on a real cluster: `limit_roles` enforcement was measured
single-node (in-process windows), and the Redis limiter only in-process unit tests. This drill runs
TWO members of a three-member raft cluster against the test Redis THROUGH a cuttable relay, so the
same tenant's quota can be spent on both nodes and the bus can be killed under them.

Ported to the member-list topology (ADR-0001, 2026-10-05). What changed and why:
  * `HYDRA_ROLE=leader|edge` became "two members of `HYDRA_CLUSTER_PEERS`". The roles are gone, and
    the drill never actually depended on them — it needs two nodes whose rate-limit windows share
    one Redis, which is exactly what the member topology gives;
  * the bring-up had to change ORDER, and that is a real property of the new model: a data directory
    that has never been claimed needs a MAJORITY up at the same time, so "start the leader, wait for
    it, then start the edge" CANNOT work any more (one node of three can never claim its directory —
    measured). Both members now start together, and the drill waits for both to be healthy plus
    exactly one raft writer;
  * the leg that compared the other node's applied SNAPSHOT VERSION to the version a write produced
    is GONE: it read `hydra_control_snapshot_version`, which was retired with the polling client (and
    was one of three series whose alert rows could never fire — see `ops.md` §9.1). Nothing replaced
    it as a per-node number: the control plane tracks the head's content HASH, and no metric exposes
    "which head am I serving". The claim it corroborated ("that node holds the whole version") is
    still proved by the leg above it, which uses an unconditional-deny role as the probe.

Cases (real cluster, real Redis, mock upstream; the user's dev stack is untouched):
  C0  control: both members serve the tenant before any role exists
  C1  config convergence is proven with a ZERO-side-effect probe: a `limit_count = 0` role
      (unconditional deny, decided BEFORE any Redis call) is created in the same snapshot as
      the measured role, so the OTHER member's first 429 proves it holds that whole config — and
      no Redis window exists for the deny role
  C2  the SHARED count window: 2 admits on node A + 1 on node B fill `limit_count = 3`,
      then BOTH nodes refuse; the Redis ZSET holds exactly 3 members carrying TWO different
      instance prefixes (the per-process member leak that used to collapse into one entry)
  C3  the SHARED token window: tenant t2's `limit_token` ceiling is crossed by usage recorded
      on the OTHER node (next-request semantics, measured across processes)
  C4  FAIL-OPEN: with the window full, cutting the relay admits requests again, logs
      `redis rate-limit check failed; failing open`, and moves
      `hydra_control_poll_total{result="rate_limit_error"}` — bounded in time by the command
      timeout, so the data plane never hangs
  C5  RECOVERY: restoring the relay restores enforcement (fail-open is not state damage)

Run: python3 integration/test_cluster_limits.py    # needs the 6380 test Redis + a cluster build
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import socket
import socketserver
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "cluster-limits-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
# Two members are STARTED; the third exists only in the member list, because a three-member table is
# the minimum the parser accepts and two live members are a majority (enough to elect and to claim
# their data directories). `PHANTOM_RAFT` is never bound by anything.
A_ADMIN, A_DATA, A_RAFT = 18860, 18861, 18862
B_ADMIN, B_DATA, B_RAFT = 18870, 18871, 18872
PHANTOM_RAFT = 18873
UPSTREAM = 18879
REDIS_DB = 3
ADMIN_TOKEN = "hydra-cluster-limits-admin-2026"
MEMBERS = f"cl-a=127.0.0.1:{A_RAFT},cl-b=127.0.0.1:{B_RAFT},cl-c=127.0.0.1:{PHANTOM_RAFT}"
TOKENS = 13                      # 5 in + 8 out, the mock upstream's usage
DEAD_CH = "http://127.0.0.1:18898"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


# ---------------------------------------------------------------------------
# Redis: raw RESP (there is no redis-cli on this host) + a cuttable relay
# ---------------------------------------------------------------------------

def redis_url_parts():
    raw = os.environ.get("HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380").rstrip("/")
    parsed = urllib.parse.urlparse(raw)
    if not parsed.hostname or not parsed.port:
        return None, f"cannot parse {raw!r} as redis://host:port"
    return (parsed.hostname, parsed.port), ""


def redis_cmd(sock, *args):
    """Send one RESP command on `sock`.
    HAZARD (measured round 166): the db is whatever `sock` has SELECTed — this helper does NOT select
    one. The window lives in `REDIS_DB`, so `redis_cmd(redis_sock, "DEL", <window key>)` deletes
    NOTHING (`:0`) unless that socket was selected first; the readers below therefore open their own
    selected connection. An ad-hoc probe that ignores this silently measures nothing.
    """
    out = b"*%d\r\n" % len(args)
    for a in args:
        b = str(a).encode()
        out += b"$%d\r\n%s\r\n" % (len(b), b)
    sock.sendall(out)
    time.sleep(0.05)
    sock.settimeout(2)
    try:
        return sock.recv(65536)
    except socket.timeout:
        return b""


def flush_db(host, port):
    """Start from an empty Redis DB so a previous run's windows cannot decide this one."""
    try:
        sock = socket.create_connection((host, port), timeout=5)
    except OSError as e:
        return None, f"test Redis {host}:{port} unreachable: {e}"
    before = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    sock.close()
    sock = socket.create_connection((host, port), timeout=5)
    redis_cmd(sock, "SELECT", REDIS_DB)
    redis_cmd(sock, "FLUSHDB")
    after = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    sock.close()
    return True, f"db {REDIS_DB} flushed (was {before} keys on db 0, now {after})"


def keys_matching(sock, pattern):
    sock2 = socket.create_connection(sock.getpeername(), timeout=5)
    redis_cmd(sock2, "SELECT", REDIS_DB)
    raw = redis_cmd(sock2, "KEYS", pattern).decode(errors="replace")
    sock2.close()
    return [l for l in raw.splitlines() if l.startswith("hydra:")]


def zinfo(sock, key):
    """ZCARD + PTTL of one window key, so "the window is still there" is measured."""
    sock2 = socket.create_connection(sock.getpeername(), timeout=5)
    redis_cmd(sock2, "SELECT", REDIS_DB)
    card = redis_cmd(sock2, "ZCARD", key).decode(errors="replace").strip().splitlines()[-1]
    ttl = redis_cmd(sock2, "PTTL", key).decode(errors="replace").strip().splitlines()[-1]
    sock2.close()
    return f"zcard={card} pttl={ttl}"


def zmembers(sock, key):
    """Members of a Redis ZSET, as a list of strings."""
    sock2 = socket.create_connection(sock.getpeername(), timeout=5)
    redis_cmd(sock2, "SELECT", REDIS_DB)
    raw = redis_cmd(sock2, "ZRANGE", key, "0", "-1").decode(errors="replace")
    sock2.close()
    return [l for l in raw.splitlines() if l and not l.startswith(("*", "$", ":"))]


class RedisRelay:
    """A TCP relay in front of the real Redis so the bus can be CUT and RESTORED under
    running nodes. A dead URL cannot produce the state at all: the node refuses to start
    ("redis pool failed to initialise: Connection refused", measured) — and this drill needs
    a working bus first, then a broken one, then a working one again."""

    def __init__(self, host, port):
        self.upstream = (host, port)
        self.server = None
        self.port = None
        self.blocked = False
        self.sockets = []
        self.lock = threading.Lock()
        # Every connection attempt the node makes, accepted or refused: the difference between
        # "the pool never reconnects" and "it reconnects and still fails" is the whole finding.
        self.attempts = []
        self.blackhole = False

    def start(self):
        relay = self

        class Handler(socketserver.BaseRequestHandler):
            def handle(self):
                with relay.lock:
                    relay.attempts.append((time.time(), relay.blocked))
                if relay.blocked:
                    return          # refuse immediately: the pool's next EVAL fails
                if relay.blackhole:
                    # Accept and stay SILENT: the node must bound this with its command timeout.
                    while relay.blackhole:
                        time.sleep(0.1)
                    return
                try:
                    up = socket.create_connection(relay.upstream, timeout=5)
                except OSError:
                    return
                with relay.lock:
                    relay.sockets.extend([self.request, up])
                threading.Thread(target=relay._pump, args=(self.request, up), daemon=True).start()
                relay._pump(up, self.request)

        self.server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self.port

    @staticmethod
    def _pump(src, dst):
        try:
            while True:
                chunk = src.recv(65536)
                if not chunk:
                    break
                dst.sendall(chunk)
        except OSError:
            pass
        finally:
            for s in (src, dst):
                try:
                    s.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def attempts_since(self, t0, blocked=None):
        with self.lock:
            return [a for a in self.attempts
                    if a[0] >= t0 and (blocked is None or a[1] is blocked)]

    def set_blocked(self, blocked):
        """Cut (or restore) the bus. Cutting severs the pooled connections too, which is what
        a network partition looks like from the node's side."""
        self.blocked = blocked
        if blocked:
            with self.lock:
                socks, self.sockets = self.sockets, []
            for s in socks:
                try:
                    s.close()
                except OSError:
                    pass

    def set_blackhole(self, on):
        """Round 166: a SECOND failure mode — accept the connection and never answer.

        `set_blocked` REFUSES the connection (`return` closes it), which the node sees as an
        immediate error; that is not what the command timeout is for. The timeout exists for a
        socket that accepts and then goes silent (a dropped route, a hung peer), and until this mode
        existed the drill's own claim ("the command timeout turns a black-holed socket into an
        ordinary error") was never exercised — `elapsed_cut < 2.5` passed because the refusal is
        instantaneous.
        """
        self.blackhole = on
        if on:
            # Sever the pooled connections so the next command really opens a fresh, silent one.
            with self.lock:
                socks, self.sockets = self.sockets, []
            for s in socks:
                try:
                    s.close()
                except OSError:
                    pass


# ---------------------------------------------------------------------------
# nodes and the mock upstream
# ---------------------------------------------------------------------------

class Upstream(BaseHTTPRequestHandler):
    """The tenant `auth_url` AND the provider endpoint: any key is allowed, the chat answer
    reports 5 prompt + 8 completion tokens (so one response is 13 tokens)."""

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
            payload = json.dumps({"id": "chatcmpl-cl", "object": "chat.completion",
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
            return r.status, dict(r.headers), r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode(errors="replace")
    except Exception as e:
        return 0, {}, str(e)


def admin(method, path, body=None, port=A_ADMIN):
    st, _, out = call(method, f"http://127.0.0.1:{port}/api/v1{path}", token=ADMIN_TOKEN, body=body)
    return st, out


def proxied(data_port, host, key):
    started = time.time()
    st, headers, body = call("POST", f"http://127.0.0.1:{data_port}/v1/chat/completions",
                             token=key, host=host,
                             body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})
    return st, headers, body, time.time() - started


def start_node(redis_url, label, admin_port, data_port, raft_port):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN,
        "HYDRA_CLUSTER_PEERS": MEMBERS, "HYDRA_CLUSTER_ID": "cluster-limits-drill",
        "HYDRA_NODE_ID": label, "HYDRA_ARACHNE_LISTEN": f"127.0.0.1:{raft_port}",
        "HYDRA_ARACHNE_DATA_DIR": os.path.join(DIR, f"raft-{label}"),
        "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin_port}", "HYDRA_LISTEN": f"127.0.0.1:{data_port}",
        "HYDRA_REDIS_URL": redis_url, "HYDRA_REDIS_MODE": "single",
        "HYDRA_USAGE_SINK": "clickhouse", "HYDRA_CLICKHOUSE_URL": DEAD_CH,
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_for(fn, budget=25.0, step=0.25):
    deadline = time.time() + budget
    while time.time() < deadline:
        if fn():
            return True
        time.sleep(step)
    return False


def healthy(admin_port):
    """A member's own admin health. `/api/v1/health` needs the admin token; `/healthz` and
    `/readyz` were the retired edge role's token-free probes and answer 404 now."""
    return call("GET", f"http://127.0.0.1:{admin_port}/api/v1/health", token=ADMIN_TOKEN)[0] == 200


def leader_probe(admin_port):
    return call("GET", f"http://127.0.0.1:{admin_port}/healthz/leader")[0]


def metric(port, name):
    _, _, body = call("GET", f"http://127.0.0.1:{port}/metrics", token=ADMIN_TOKEN)
    return [l for l in body.splitlines() if l.startswith(name) and not l.startswith("#")]


def metric_sum(port, name, needle=None):
    total = 0.0
    for line in metric(port, name):
        if needle and needle not in line:
            continue
        try:
            total += float(line.rsplit(" ", 1)[1])
        except (ValueError, IndexError):
            pass
    return total


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def kill_our_instances():
    """Kill leftovers of THIS tree's binary — matched on /proc/<pid>/exe, never by name (a
    `pkill -x hydra` would kill the user's dockerized dev stack, whose process is also named
    `hydra`; that mistake was measured and fixed in round 78)."""
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


def role(rid, **kw):
    body = {"id": rid, "name": rid, "matching_key": None, "matching_model": None,
            "matching_tenant": None, "matching_provider": None, "limit_count": None,
            "limit_token": None, "window": "m", "enabled": True, "created_at": ""}
    body.update(kw)
    return body


def seed():
    rows = [
        ("providers", {"id": "p1", "key": "p1", "name": "P",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T1", "domain": "count.local",
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenants", {"id": "t2", "name": "T2", "domain": "token.local",
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenants", {"id": "t3", "name": "T3", "domain": "probe.local",
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
    ]
    for tid in ("t1", "t2", "t3"):
        rows += [
            ("tenant-providers", {"id": f"tp-{tid}", "tenant_id": tid, "provider_id": "p1",
                                  "created_at": "", "updated_at": ""}),
            ("tenant-models", {"id": f"tm-{tid}", "tenant_id": tid, "model_key": "echo",
                               "created_at": "", "updated_at": ""}),
        ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[cluster-limits] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")


def main():
    if not os.path.exists(BIN):
        print(f"[cluster-limits] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    (host, port), err = redis_url_parts()
    if err:
        print(f"[cluster-limits] CANNOT VERIFY: {err}", file=sys.stderr)
        return 2
    ok, note = flush_db(host, port)
    if not ok:
        print(f"[cluster-limits] CANNOT VERIFY: {note}", file=sys.stderr)
        return 2
    print(f"== redis: {note}")
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    killed = kill_our_instances()
    if killed:
        print(f"   (killed leftover instance(s) of OUR build: {killed})")
    time.sleep(0.5)

    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    relay = RedisRelay(host, port)
    relay_port = relay.start()
    redis_url = f"redis://127.0.0.1:{relay_port}/{REDIS_DB}"

    redis_sock = socket.create_connection((host, port), timeout=5)
    proc_a = proc_b = None
    try:
        # BOTH members at once: one node of a three-member table can never claim its data directory
        # (the claim is a raft write and needs a majority), so a sequential bring-up would time out
        # with "no member adopted this node's Arachne data directory" — measured 2026-10-05.
        proc_a = start_node(redis_url, "cl-a", A_ADMIN, A_DATA, A_RAFT)
        proc_b = start_node(redis_url, "cl-b", B_ADMIN, B_DATA, B_RAFT)
        if not wait_for(lambda: healthy(A_ADMIN) and healthy(B_ADMIN), budget=40):
            for label in ("cl-a", "cl-b"):
                log = open(os.path.join(DIR, f"{label}.log"), errors="replace").read()
                if "cluster-redis' cargo feature" in log:
                    print("[cluster-limits] CANNOT VERIFY: the binary lacks the cluster features; "
                          "rebuild with --features server,cluster-redis,arachne,usage-clickhouse",
                          file=sys.stderr)
                    return 2
                print(f"--- {label} ---\n{log[-700:]}", file=sys.stderr)
            print("[cluster-limits] CANNOT VERIFY: the two members never became healthy",
                  file=sys.stderr)
            return 2
        # Exactly one of them must be the raft writer, or the cluster never committed anything.
        if not wait_for(lambda: sum(1 for p in (A_ADMIN, B_ADMIN) if leader_probe(p) == 200) == 1,
                        budget=25):
            print("[cluster-limits] CANNOT VERIFY: no single raft writer", file=sys.stderr)
            return 2
        seed()
        # Both members must serve the seeded config before anything is measured.
        if not wait_for(lambda: all(proxied(p, "count.local", "sk-t1")[0] == 200
                                    for p in (A_DATA, B_DATA)), budget=30):
            print("[cluster-limits] CANNOT VERIFY: a member never routed the tenant",
                  file=sys.stderr)
            return 2

        # ---- C0: control, before any role exists ------------------------------
        st_l = proxied(A_DATA, "count.local", "sk-t1")[0]
        st_e = proxied(B_DATA, "count.local", "sk-t1")[0]
        check("C0: with no limit role, BOTH members serve the tenant (200)",
              st_l == 200 and st_e == 200, f"a={st_l} b={st_e}")

        # ---- C1: one snapshot, two roles; convergence proven with zero side effects
        # `r-deny` is an unconditional deny decided BEFORE any Redis call, so an edge 429 on
        # t3 proves the edge holds the WHOLE version — including `r-count` — without touching
        # a window (polling with t1 would pollute the very counter being measured).
        # Round 166: ORDER MATTERS, and the old order made the probe unsound. Each POST goes through
        # the write path, which reloads (`admin/handlers.rs` -> `reload_best_effort`) — so three POSTs
        # produce THREE versions; with `r-deny` written FIRST, an edge holding only v1 (just `r-deny`)
        # answers 429 for t3 exactly like an edge holding the whole version, and "the edge holds the
        # WHOLE version" was not proven by this leg. Writing the probe role LAST makes "the edge sees
        # `r-deny`" imply "the edge is at a version that also contains r-count and r-token".
        admin("POST", "/limit-roles", role("r-count", matching_tenant="t1", limit_count=3,
                                          window="h"))
        admin("POST", "/limit-roles", role("r-token", matching_tenant="t2", limit_token=10))
        admin("POST", "/limit-roles", role("r-deny", matching_tenant="t3", limit_count=0))
        st, out = admin("POST", "/reload", {})
        check("C1: the three roles were accepted (any member applies a management write now; the probe role written LAST)",
              st == 200, f"HTTP {st} {out[:80]}")
        converged = wait_for(lambda: proxied(B_DATA, "probe.local", "sk-t3")[0] == 429, budget=20)
        st_l3 = proxied(A_DATA, "probe.local", "sk-t3")[0]
        check("C1: `limit_count = 0` denies UNCONDITIONALLY on both members (an unconditional "
              "deny needs no window — it is the convergence probe)",
              converged and st_l3 == 429, f"a={st_l3} b-429={converged}")
        # THE VERSION-GAUGE LEG WAS HERE, AND IT IS DELETED (2026-10-05). It compared this node's
        # `hydra_control_snapshot_version` — recorded when a node APPLIED a pushed snapshot — with the
        # version the write produced. Both halves are gone: the snapshot channel was retired in T4.1,
        # and that series was one of three whose alert rows could never fire (it lost its recorder and
        # kept exporting 0 — see `ops.md` §9.1). Nothing replaced it as a per-node number: the control
        # plane tracks the head's content HASH, and no metric exposes "which head am I serving".
        # The property it corroborated — "that member holds the whole config" — is still proved by the
        # leg ABOVE, whose probe is an unconditional deny written LAST, so a 429 there can only come
        # from a config that also carries the measured roles.
        deny_keys = keys_matching(redis_sock, "hydra:{rl:r-deny*}")
        check("C1: ...and it created NO Redis window (the deny is decided client-side)",
              deny_keys == [], f"keys={deny_keys}")

        # ---- C2: the SHARED count window --------------------------------------
        codes_l = [proxied(A_DATA, "count.local", "sk-t1")[0] for _ in range(2)]
        st_e1, _, body_e1, _ = proxied(B_DATA, "count.local", "sk-t1")
        announce("C2 the three admits", f"a={codes_l} b={st_e1}")
        check("C2: the two members' admits land in ONE window: 2 on A + 1 on B fill `limit_count = 3`",
              codes_l == [200, 200] and st_e1 == 200,
              f"a={codes_l} b={st_e1} {body_e1[:60]}")
        count_keys = keys_matching(redis_sock, "hydra:{rl:r-count:*}:count")
        members = zmembers(redis_sock, count_keys[0]) if count_keys else []
        prefixes = {m.split("-")[1] if len(m.split("-")) > 1 else m for m in members}
        announce("C2 the shared window", f"key={count_keys} members={members}")
        check("C2: Redis holds exactly the 3 samples, from TWO different instances "
              "(a shared member would collapse and under-count — the fail-open direction)",
              len(count_keys) == 1 and len(members) == 3 and len(prefixes) == 2,
              f"members={len(members)} distinct instance prefixes={len(prefixes)}")
        announce("C2 the window in Redis right after the fill",
                 zinfo(redis_sock, count_keys[0]) if count_keys else "key absent")
        st_l4, headers_l4, _, _ = proxied(A_DATA, "count.local", "sk-t1")
        st_e2, _, _, _ = proxied(B_DATA, "count.local", "sk-t1")
        ra = headers_l4.get("Retry-After") or headers_l4.get("retry-after")
        check("C2: ...and once the SHARED window is full, BOTH members refuse (429)",
              st_l4 == 429 and st_e2 == 429, f"a={st_l4} b={st_e2}")
        check("C2: the cluster refusal carries `Retry-After` as the conservative whole-window "
              "upper bound (the Redis limiter cannot read the oldest sample's age)",
              ra is not None and ra.isdigit() and int(ra) >= 1, f"Retry-After={ra!r}")
        check("C2: ...and the refusal is attributed to the role on the node that denied",
              metric_sum(A_ADMIN, "hydra_limit_rejected_total", 'role="r-count"') >= 1.0,
              f"{metric(A_ADMIN, 'hydra_limit_rejected_total')}")

        # ---- C3: the SHARED token window --------------------------------------
        # `limit_token = 10` is below ONE response's usage (13), so the first t2 request passes and
        # records 13 tokens on member A; member B must then refuse the next one — the ceiling is
        # crossed by usage ONE member recorded for the other.
        st_t1 = proxied(A_DATA, "token.local", "sk-t2")[0]
        st_t2, _, _, _ = proxied(B_DATA, "token.local", "sk-t2")
        token_keys = keys_matching(redis_sock, "hydra:{rl:r-token:*}:tokens")
        check("C3: the token ceiling is crossed by usage recorded on the OTHER node "
              "(next-request semantics, measured across processes)",
              st_t1 == 200 and st_t2 == 429, f"first(a)={st_t1} second(b)={st_t2}")
        check("C3: ...and the token window lives in Redis under its own key",
              len(token_keys) == 1, f"keys={token_keys}")
        check("C3: ...counted with the metric's real `dim` value (`tokens`) on the node that denied "
              "(B — its 429 came from the usage member A recorded)",
              metric_sum(B_ADMIN, "hydra_limit_rejected_total", 'dim="tokens"') >= 1.0,
              f"b={metric(B_ADMIN, 'hydra_limit_rejected_total')} "
              f"a={metric(A_ADMIN, 'hydra_limit_rejected_total')}")

        # ---- C4: FAIL-OPEN while the bus is cut -------------------------------
        errs_before = metric_sum(A_ADMIN, "hydra_control_poll_total", 'result="rate_limit_error"')
        relay.set_blocked(True)
        time.sleep(0.5)
        st_cut, _, _, elapsed_cut = proxied(A_DATA, "count.local", "sk-t1")
        announce("C4 request 1 (a, cut)",
                 f"HTTP {st_cut} in {elapsed_cut:.2f}s window={zinfo(redis_sock, count_keys[0])}")
        cut_codes = [st_cut]
        for i in (2, 3):
            st_i, _, _, el_i = proxied(B_DATA, "count.local", "sk-t1")
            cut_codes.append(st_i)
            announce(f"C4 request {i} (b, cut)",
                     f"HTTP {st_i} in {el_i:.2f}s window={zinfo(redis_sock, count_keys[0])}")
        log_l = open(os.path.join(DIR, "cl-a.log"), errors="replace").read()
        log_e = open(os.path.join(DIR, "cl-b.log"), errors="replace").read()
        announce("C4 under a cut bus", f"codes={cut_codes} first-latency={elapsed_cut:.2f}s")
        check("C4: with Redis unreachable the SHARED window stops being enforced — the "
              "documented fail-open direction (traffic keeps flowing)",
              all(c == 200 for c in cut_codes), f"codes={cut_codes}")
        check("C4: ...and the node says so in the log (`failing open`)",
              "redis rate-limit check failed; failing open" in log_l + log_e,
              next((l for l in (log_l + log_e).splitlines() if "failing open" in l),
                   "<no line>")[:150])
        errs_after = metric_sum(A_ADMIN, "hydra_control_poll_total", 'result="rate_limit_error"')
        check("C4: ...and every fail-open is visible to the operator in "
              "`hydra_control_poll_total{result=\"rate_limit_error\"}`",
              errs_after > errs_before, f"{errs_before} -> {errs_after}")
        check("C4: the failure is BOUNDED in time — a refused connection is an ordinary error, "
              "not a data-plane hang",
              elapsed_cut < 2.5, f"first request under the cut took {elapsed_cut:.2f}s")

        # ---- C4b: the COMMAND TIMEOUT, against a socket that accepts and stays silent -----------
        # Round 166: C4 above cuts the bus by REFUSING connections, so it says nothing about the
        # command timeout. This leg black-holes the relay instead (accept, never answer) and requires
        # the request to come back (fail-open) in a time window that only the timeout can produce:
        #   · too fast  => the socket was not really silent (the leg measures nothing),
        #   · too slow  => nothing bounded it (the "hang" the timeout exists to prevent).
        relay.set_blocked(False)
        relay.set_blackhole(True)
        time.sleep(0.5)
        st_bh, _, _, elapsed_bh = proxied(A_DATA, "count.local", "sk-t1")
        relay.set_blackhole(False)
        announce("C4b request against a black-holed bus", f"HTTP {st_bh} in {elapsed_bh:.2f}s")
        check("C4b: a black-holed Redis still yields a response (fail-open, not a hang)",
              st_bh == 200, f"HTTP {st_bh}")
        check("C4b: ...and the COMMAND TIMEOUT is what bounded it (the socket really was silent)",
              elapsed_bh >= 0.3, f"took {elapsed_bh:.2f}s (a refusal would be ~0s)")
        check("C4b: ...and it stayed bounded (well under the data-plane timeouts)",
              elapsed_bh < 3.0, f"took {elapsed_bh:.2f}s")

        # ---- C5: recovery -----------------------------------------------------
        announce("C4 the window in Redis while the bus is cut",
                 zinfo(redis_sock, count_keys[0]) if count_keys else "key absent")
        announce("C4 the fail-open metric on both nodes",
                 f"a={metric(A_ADMIN, 'hydra_control_poll_total')} "
                 f"b={metric(B_ADMIN, 'hydra_control_poll_total')}")
        relay.set_blocked(False)
        t_restored = time.time()
        time.sleep(0.5)
        # The bus must really be back before the node's recovery is timed — otherwise this leg
        # would measure the HARNESS. An independent client through the same relay proves it.
        probe = socket.create_connection(("127.0.0.1", relay_port), timeout=3)
        bus_back = redis_cmd(probe, "PING").strip()
        probe.close()
        announce("C5 the relay serves again (independent client through it)", f"PING -> {bus_back!r}")
        announce("C5 the window in Redis after the bus is restored",
                 zinfo(redis_sock, count_keys[0]) if count_keys else "key absent")
        announce("C5 the roles still in the config member A serves",
                 f"HTTP {admin('GET', '/limit-roles')[0]} {admin('GET', '/limit-roles')[1][:130]}")
        check("C5: the relay itself is serving again (the harness is not the thing being "
              "measured)", bus_back == b"+PONG", f"{bus_back!r}")
        recovered, took, st_r = False, 0.0, 0
        first_after = None          # round 166: the FIRST reply after the bus is back
        t0 = time.time()
        for attempt in range(1, 46):
            st_r, _, _, el_r = proxied(A_DATA, "count.local", "sk-t1")
            if first_after is None:
                first_after = st_r
            took = time.time() - t0
            if attempt <= 3 or attempt % 5 == 0 or st_r == 429:
                announce(f"C5 attempt {attempt} (a, restored)",
                         f"HTTP {st_r} in {el_r:.2f}s after {took:.1f}s "
                         f"failopen={metric_sum(A_ADMIN, 'hydra_control_poll_total', 'rate_limit_error')}")
            if st_r == 429:
                recovered = True
                break
            time.sleep(1.5)
        announce("C5 TCP connections the node opened after the bus returned",
                 f"{len(relay.attempts_since(t_restored, blocked=False))} accepted, "
                 f"{len(relay.attempts_since(t_restored, blocked=True))} refused")
        check("C5: restoring the bus restores enforcement (fail-open left the window intact)",
              recovered, f"last status={st_r} after {took:.1f}s of probing")
        # Round 166: "some probe within 45 attempts is a 429" does NOT distinguish "the window was
        # preserved" from "the window was WIPED and the probes refilled it" (the 4th probe would then
        # be denied and the leg would still pass). The window held `limit_count` samples before the
        # cut, so the FIRST request after the bus is healthy must already be refused.
        check("C5: ...and the FIRST request after recovery is already refused — the window was "
              "preserved, not rebuilt by the probes",
              first_after == 429,
              f"first status after the bus returned = {first_after} "
              f"(200 means the window was empty and the probing refilled it)")
        if recovered:
            announce("C5 the node needed this long after the bus returned", f"{took:.1f}s")
    finally:
        stop(proc_a)
        stop(proc_b)
        relay.set_blocked(True)
        try:
            relay.server.shutdown()
            relay.server.server_close()
        except Exception:
            pass
        redis_sock.close()
        upstream.shutdown()

    print()
    if failures:
        print(f"CLUSTER LIMITS: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("CLUSTER LIMITS: PASSED (shared count window across nodes, shared token window, "
          "zero-side-effect convergence probe, fail-open under a cut bus with its metric and "
          "bounded latency, and recovery)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
