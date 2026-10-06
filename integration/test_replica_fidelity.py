#!/usr/bin/env python3
"""REPLICA FIDELITY: does a promoted edge serve EXACTLY what the leader was serving?

`ops.md` §13's list of fixed-but-was-broken items contains this one, and it is the reason to worry
about failover at all:

  > ~~Disabled `limit_role` / `provider_key_binding` rows are not carried in config snapshots —
  > after a failover they are lost from replicas.~~ **FIXED**: the snapshot contract carries the
  > full fidelity rows (including disabled ones, `provider_key` identity and tenant access-token
  > hashes), so a promoted replica is byte-faithful.

"Byte-faithful" is a strong claim about a *wire contract between nodes*, and the observable
consequence is concrete: a replica that is serving — or that becomes the active node — must still
enforce the tenant's limit roles, still authenticate its tenant-API token and still steer its
sub-tenants. If any of those rows failed to travel, it would quietly serve *more* traffic than the
leader did — the worst kind of failover.

WHAT THIS DRILL ACTUALLY OBSERVES, and what it does not (stated after the round-162 review, which
found the header claiming more than the legs check):
  · observed — ENABLED limit roles, tenant access-token HASHES and sub-tenant ROUTES, on the edge
    (F1-F3), after a RESTART of that replica (F4/F5) and after the leader is killed (F6);
  · observed since round 169 — the "disabled rows travel" half of §13, on the PUBLISHER's wire (F8):
    the leader serves `GET /api/v1/internal/control` (cluster-token gated) with the `SnapshotWire` as
    JSON, and the leg asserts that the DISABLED `limit_role` (`r-off`) and the DISABLED
    `provider_key_binding` (`b-off`) are present in `snapshot.fidelity.*` while being ABSENT from
    `snapshot.cfg.*` — the two payloads differ by exactly the rows §13 is about, so the leg cannot
    pass by reading the wrong sub-object. It also pins that the token hash is SEALED (the raw token
    appears nowhere in the body). What is still NOT observed is an edge-side BEHAVIOUR for a disabled
    row — a disabled row has none — and the `r-off` / `b-off` fixtures are now evidence rather than
    shape-keeping;
  · observed since round 164 — PROMOTION itself: a second leader-role node holds the stand-by side of
    the lease, and after the leader is killed the phase P1-P4 legs assert that the node the lease
    promotes still enforces the limit role (P2), still authenticates the token (P3) and still steers
    the sub-tenant route (P4). Measured falsification: with the standby's control URL pointed at a dead
    port (so it never materializes a snapshot) P1 still passes while P2/P3/P4 fail with 404 / 401 /
    `['?','?','?','?']` — the three legs really measure "the promoted node knows the rows".

This drill runs one leader + one edge on the test Redis and checks the SAME three things twice:
on the edge (its snapshot arrived) and again on the PROMOTED edge after the leader is killed.

  F0  control: the leader serves the tenant, and the edge serves it from its own snapshot
  F1  the limit role is enforced on the EDGE (the row travelled): 2 admits, then 429
  F2  the tenant-API access token works on the EDGE (the token HASH travelled)
  F3  the sub-tenant's per-model route steers traffic on the EDGE (sub_tenant + route rows)
  F8  (round 169) the PUBLISHER's wire: the disabled rows of §13 are in `snapshot.fidelity.*` and not
      in `snapshot.cfg.*`, the control payload is cluster-token gated, and secrets travel sealed
  F4  a RESTARTED edge re-materializes the snapshot and the LIVE limit window still denies
  F5  ...and the restarted replica still authenticates the token and still steers the sub-tenant
      (NOTE, corrected in round 163: F4/F5 restart the edge, they do NOT promote a node. The claim
      "survive its promotion" was unbacked here AND in `test_cluster_ha.py`, which promotes but never
      re-checks limit roles / token hashes / sub-tenant routes on the promoted node. Closing that gap
      is queued in the plan (§2es 队列) — this header now says what the drill really does.)

Run: python3 integration/test_replica_fidelity.py   # needs the 6380 test Redis + a cluster build
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "replica-fidelity-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
L_ADMIN, L_DATA = 18690, 18691
E_ADMIN, E_DATA = 18700, 18701
# A second leader-role node: it takes the stand-by side of the lease, so killing the leader
# PROMOTES it — that is what the P1-P4 legs measure (round 164).
S_ADMIN, S_DATA = 18740, 18741
UPSTREAM = 18709
ADMIN_TOKEN = "hydra-fidelity-admin-2026"
CLUSTER_TOKEN = "hydra-fidelity-internal-2026"
TENANT_TOKEN = "tenant-fidelity-token-2026"
LEASE_MS = 4000
POLL_MS = 300
REDIS_DB = 5
LIMIT = 2
LIMIT_KEY = "sk-fidelity-limited"          # the key the fidelity legs present
RAW_ONLY_KEY = "sk-raw-only-probe-77"      # only `r-raw` points at this key's RAW form
# `mask_key` (hydra-core/src/rewrite.rs): len >= 20 -> first 10 + stars(len-14) + last 4;
# len >= 6 -> first 2 + stars(len-4) + last 2. LIMIT_KEY is 19 chars, so: first 2, 15 stars, last 2.
MASKED_LIMIT_KEY = "sk" + "*" * (len(LIMIT_KEY) - 4) + "ed"
# The key the MASK leg presents: its MASK equals MASKED_LIMIT_KEY (first 2 + last 2) but its raw
# value is matched by NO role. Round 162: this used to be `MASKED_KEY = LIMIT_KEY`, and `r-shared`
# matches that literal raw key — so the leg ("the MASK form really fires") stayed green even if mask
# matching were broken, because the raw role did the denying. With a mask-only key the deny can only
# come from `r-masked`, which is what the leg claims to measure.
MASKED_ONLY_KEY = "sk" + "q" * (len(LIMIT_KEY) - 4) + "ed"
assert MASKED_ONLY_KEY != LIMIT_KEY and MASKED_ONLY_KEY != RAW_ONLY_KEY
# A DIFFERENT raw key that masks to the same string (the mask keeps the first 2 and last 2 of a
# 19-char key) — it matches the same role and shares the same bucket.
COLLIDE_KEY = "sk" + "z" * (len(LIMIT_KEY) - 4) + "ed"
# A key with its OWN mask (different first/last characters), so the edge legs get a fresh window.
EDGE_KEY = "sk-edge-fidelity-88"
EDGE_MASK = EDGE_KEY[:2] + "*" * (len(EDGE_KEY) - 4) + EDGE_KEY[-2:]
LATE_KEY = "sk-late-enabled-99"     # only the role enabled DURING the run points at this key
LATE_MASK = LATE_KEY[:2] + "*" * (len(LATE_KEY) - 4) + LATE_KEY[-2:]  # D-15: the MASK is what matches

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Upstream(BaseHTTPRequestHandler):
    """Two mock providers, so WHICH provider served a request is observable."""

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
            tag = self.path.split("/provider/")[-1].split("/")[0] if "/provider/" in self.path else "a"
            payload = json.dumps({"id": f"chatcmpl-{tag}", "object": "chat.completion",
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


def admin(method, path, body=None, port=L_ADMIN):
    return call(method, f"http://127.0.0.1:{port}/api/v1{path}", token=ADMIN_TOKEN, body=body)


def control_snapshot(port, token):
    """`GET /api/v1/internal/control?since=0` — what the LEADER **publishes**, as JSON.

    The wire `SnapshotWire` carries both the ordinary config payload (`cfg`, secrets stripped) and the
    `fidelity` row set. `since=0` forces a full snapshot no matter how current the caller is.
    Returns `(status, parsed_or_None, raw_body)`.
    """
    st, body = call("GET", f"http://127.0.0.1:{port}/api/v1/internal/control?since=0", token=token)
    try:
        return st, json.loads(body), body
    except ValueError:
        return st, None, body


def flush_redis_db(host, port, db):
    """Raw RESP (there is no redis-cli here): a previous run's windows must not decide this one."""
    def cmd(sock, *args):
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
    try:
        s = socket.create_connection((host, port), timeout=5)
    except OSError as e:
        return False, f"test Redis {host}:{port} unreachable: {e}"
    cmd(s, "SELECT", db)
    r = cmd(s, "FLUSHDB")
    s.close()
    return b"+OK" in r, "flushed"


def start(role, admin_port, data_port, node_id):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN, "HYDRA_CLUSTER_TOKEN": CLUSTER_TOKEN,
        "HYDRA_ROLE": role, "HYDRA_NODE_ID": node_id,
        "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin_port}",
        "HYDRA_LISTEN": f"127.0.0.1:{data_port}",
        "HYDRA_CONTROL_URL": f"http://127.0.0.1:{L_ADMIN}",
        "HYDRA_PUBLIC_URL": f"http://127.0.0.1:{admin_port}",
        f"HYDRA_LEADER_LEASE_MS": str(LEASE_MS), "HYDRA_CONTROL_POLL_MS": str(POLL_MS),
        "HYDRA_REDIS_URL": f"redis://{REDIS_HOST}:{REDIS_PORT}/{REDIS_DB}",
        "HYDRA_REDIS_MODE": "single",
        "HYDRA_USAGE_SINK": "clickhouse", "HYDRA_CLICKHOUSE_URL": "http://127.0.0.1:18899",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, node_id)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_UPSTREAM_FIRST_BYTE_TIMEOUT_SECS": "30",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, f"{node_id}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_for(fn, budget=25.0, step=0.25):
    deadline = time.time() + budget
    while time.time() < deadline:
        if fn():
            return True
        time.sleep(step)
    return False


def leader_probe(admin_port):
    return call("GET", f"http://127.0.0.1:{admin_port}/healthz/leader")[0]


def stop(proc):
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def kill_our_instances():
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


def proxied(data_port, key="sk-shared-1", model="shared"):
    return call("POST", f"http://127.0.0.1:{data_port}/v1/chat/completions", token=key,
                host="fidelity.local",
                body={"model": model, "messages": [{"role": "user", "content": "hi"}]})


def seed():
    rows = [
        ("providers", {"id": "p-a", "key": "p-a", "name": "A",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}/provider/a", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("providers", {"id": "p-b", "key": "p-b", "name": "B",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}/provider/b", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm-a", "key": "shared", "name": "S", "provider_id": "p-a",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm-b", "key": "shared", "name": "S", "provider_id": "p-b",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk-a", "provider_id": "p-a", "api_key": "sk-a", "created_at": ""}),
        ("provider-keys", {"id": "pk-b", "provider_id": "p-b", "api_key": "sk-b", "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": "fidelity.local",
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "access_token": TENANT_TOKEN, "cert_key": None, "cert_file": None,
                     "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp-a", "tenant_id": "t1", "provider_id": "p-a",
                              "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp-b", "tenant_id": "t1", "provider_id": "p-b",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "shared",
                           "created_at": "", "updated_at": ""}),
        # The row under test: a limit role the EDGE must know about, scoped to ONE EXACT client key
        # so the control/steering traffic cannot consume its window (a tenant-wide role was spent by
        # the readiness polling before F1 ran). NOTE: `matching_key` is an EXACT match, not a prefix
        # (`hydra_core::limit::dim_matches`, and `design.md` §: "NULL **or equal to** the client
        # api-key") — a `matching_key: "LIM_"` role silently matched NOTHING, which is what the
        # first version of this drill measured before the role was scoped to the literal key.
        ("limit-roles", {"id": "r-shared", "name": "r-shared", "matching_key": LIMIT_KEY,
                         "matching_model": None, "matching_tenant": "t1",
                         "matching_provider": None, "limit_count": LIMIT, "limit_token": None,
                         "window": "h", "enabled": True, "created_at": ""}),
        # The RAW form (what `design.md` documents: `matching_key` NULL "or equal to the client
        # api-key") on a key NO other role points at, so a 429 on it can only come from this row.
        # NOTE: this row was MISSING until round 116 — the F0b leg presented `RAW_ONLY_KEY` while
        # only the comment above `raw_codes` claimed an `r-raw` row existed, so the leg was
        # unfalsifiable-by-construction (`[200,200,200,200]` no matter what the matcher did). A
        # guard that cannot fail is not evidence; seed the row and let it speak.
        ("limit-roles", {"id": "r-raw", "name": "r-raw", "matching_key": RAW_ONLY_KEY,
                         "matching_model": None, "matching_tenant": "t1",
                         "matching_provider": None, "limit_count": LIMIT, "limit_token": None,
                         "window": "h", "enabled": True, "created_at": ""}),
        # The MASKED form of a DIFFERENT key: measured because the pre-limit gate historically
        # compared `matching_key` against `mask_key(&api_key)` only (proxy.rs:
        # `api_key: Some(&masked)`), NOT against the raw client key the docs describe.
        ("limit-roles", {"id": "r-masked", "name": "r-masked", "matching_key": MASKED_LIMIT_KEY,
                         "matching_model": None, "matching_tenant": "t1",
                         "matching_provider": None, "limit_count": LIMIT, "limit_token": None,
                         "window": "h", "enabled": True, "created_at": ""}),
        # A second masked-form role on a key with a DIFFERENT mask: the edge legs need a window of
        # their own (windows live in Redis and are shared across nodes, so reusing the same role
        # would start the edge leg already over the limit — measured).
        ("limit-roles", {"id": "r-masked-edge", "name": "r-masked-edge",
                         "matching_key": EDGE_MASK, "matching_model": None,
                         "matching_tenant": "t1", "matching_provider": None,
                         "limit_count": LIMIT, "limit_token": None, "window": "h",
                         "enabled": True, "created_at": ""}),
        # A DISABLED role + a DISABLED binding: the rows the §13 note says used to be lost.
        ("limit-roles", {"id": "r-off", "name": "r-off", "matching_key": "sk-not-yet",
                         "matching_model": None, "matching_tenant": None,
                         "matching_provider": None, "limit_count": 1, "limit_token": None,
                         "window": "h", "enabled": False, "created_at": ""}),
        ("provider-key-bindings", {"id": "b-off", "key_prefix": "OFFP_", "provider_id": "p-a",
                                   "enabled": False, "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[fidelity] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    # A sub-tenant whose route pins `shared` to p-b (steering is observable: chatcmpl-b).
    st, out = admin("POST", "/sub-tenants", {"id": "sub-1", "tenant_id": "t1", "name": "sub-1",
                                             "key_prefix": "SUB_", "enabled": True,
                                             "created_at": "", "updated_at": ""})
    if st not in (200, 201):
        raise SystemExit(f"[fidelity] CANNOT VERIFY: sub-tenant -> {st} {out[:140]}")
    st, out = admin("POST", "/sub-tenant-routes", {"id": "sr-1", "sub_tenant_id": "sub-1",
                                                   "model_key": "shared", "provider_id": "p-b",
                                                   "enabled": True, "created_at": "",
                                                   "updated_at": ""})
    if st not in (200, 201):
        raise SystemExit(f"[fidelity] CANNOT VERIFY: sub-tenant route -> {st} {out[:140]}")
    st, out = admin("POST", "/reload", {})
    return st, out


def tenant_api(admin_or_data_port, data=True):
    if data:
        return call("GET", f"http://127.0.0.1:{admin_or_data_port}/tenant/t1/api/v1/whoami",
                    token=TENANT_TOKEN, host="fidelity.local")
    return call("GET", f"http://127.0.0.1:{admin_or_data_port}/tenant/t1/api/v1/whoami",
                token=TENANT_TOKEN, host="fidelity.local")


REDIS_HOST, REDIS_PORT = "127.0.0.1", 6380


def main():
    global REDIS_HOST, REDIS_PORT
    if not os.path.exists(BIN):
        print(f"[fidelity] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    raw = os.environ.get("HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380").rstrip("/")
    parsed = urllib.parse.urlparse(raw)
    if not parsed.hostname or not parsed.port:
        print(f"[fidelity] CANNOT VERIFY: cannot parse {raw!r}", file=sys.stderr)
        return 2
    REDIS_HOST, REDIS_PORT = parsed.hostname, parsed.port
    ok, note = flush_redis_db(REDIS_HOST, REDIS_PORT, REDIS_DB)
    if not ok:
        print(f"[fidelity] CANNOT VERIFY: {note}", file=sys.stderr)
        return 2
    print(f"== redis db {REDIS_DB}: {note}")
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    killed = kill_our_instances()
    if killed:
        print(f"   (killed leftover instance(s) of OUR build: {killed})")
    time.sleep(0.4)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    leader = edge = None
    try:
        leader = start("leader", L_ADMIN, L_DATA, "fid-leader")
        if not wait_for(lambda: leader_probe(L_ADMIN) == 200, budget=30):
            print("[fidelity] CANNOT VERIFY: the leader never took the lease", file=sys.stderr)
            print(open(os.path.join(DIR, "fid-leader.log"), errors="replace").read()[-600:],
                  file=sys.stderr)
            return 2
        # The standby starts BEFORE the rows are seeded, so it materializes the same snapshot like a
        # real replica; after the leader dies the lease promotes it (P1-P4 assert what it must know).
        standby = start("leader", S_ADMIN, S_DATA, "fid-standby")
        # Readiness is `/api/v1/health` WITH the admin token, not the token-free `/healthz`: on a
        # leader-role node the token-free path is an unauthenticated admin request, so probing it in a
        # loop trips `HYDRA_ADMIN_AUTH_FAIL_LIMIT_PER_MIN` and the node starts answering 429
        # (measured while adding this leg — the "never became healthy" was the throttle, not the node).
        if not wait_for(
            lambda: call("GET", f"http://127.0.0.1:{S_ADMIN}/api/v1/health", token=ADMIN_TOKEN)[0] == 200,
            budget=25,
        ):
            print("[fidelity] CANNOT VERIFY: the standby never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "fid-standby.log"), errors="replace").read()[-800:],
                  file=sys.stderr)
            return 2
        st, out = seed()
        check("F0: the config seeded on the leader was accepted (single reload)",
              st == 200, f"HTTP {st} {out[:80]}")
        # READ THE ROWS BACK. The F0b legs below can only mean anything if the roles they rely on
        # are actually in the config: an earlier version presented `RAW_ONLY_KEY` while NO row had
        # that `matching_key`, so the leg printed `[200,200,200,200]` no matter what the matcher did
        # — a guard that cannot fail. Asserting the precondition here makes that mistake impossible
        # to repeat silently (this check goes red the moment the seed loses a row the legs need).
        st, roles_out = admin("GET", "/limit-roles")
        try:
            seeded = {r.get("matching_key"): r for r in json.loads(roles_out)}
        except (ValueError, AttributeError):
            seeded = {}
        needed = [(RAW_ONLY_KEY, "r-raw"), (MASKED_LIMIT_KEY, "r-masked"), (EDGE_MASK, "r-masked-edge")]
        missing = [f"{rid}({key!r})" for key, rid in needed
                   if key not in seeded or not seeded[key].get("enabled")]
        check("F0: every `matching_key` the F0b/F1 legs rely on is present AND enabled on the leader "
              "(`matching_key` is an EXACT match, so a missing row silently disarms a leg)",
              st == 200 and not missing,
              f"GET /limit-roles -> {st}, {len(seeded)} rows; missing={missing or 'none'}")
        # WHICH FORM of the key does `matching_key` compare against? `design.md` says "NULL **or
        # equal to** the client api-key" (the RAW key), while `proxy.rs` historically built the
        # context as `MatchCtx { api_key: Some(&mask_key(&api_key)), … }` — the MASKED form only.
        # The two forms are therefore given ONE KEY EACH (`r-raw` on `RAW_ONLY_KEY`, `r-masked` on
        # `MASKED_LIMIT_KEY`), so the 429s of one leg cannot come from the other leg's role. (An
        # earlier version put the masked role on the SAME key as the raw role, so the raw leg's 429s
        # came from the masked role's spent window and the leg measured nothing.)
        raw_codes = [proxied(L_DATA, key=RAW_ONLY_KEY)[0] for _ in range(LIMIT + 2)]
        announce("F0b the RAW-form role (what the docs describe)",
                 f"codes={raw_codes} (limit {LIMIT})")
        # FIXED in round 116: the matcher now accepts EITHER form (`key_dim_matches`), so the
        # documented raw-key form fires while the masked form keeps working. Before the fix (and
        # before `r-raw` was seeded at all) this leg measured `[200,200,200,200]` — a silently inert
        # quota that no test could see.
        check("F0b: a role whose `matching_key` is the RAW client key now FIRES (2 admits, then "
              "429) — the documented form is usable",
              raw_codes[:LIMIT] == [200] * LIMIT and raw_codes[-1] == 429,
              f"codes={raw_codes} for key {RAW_ONLY_KEY!r}")
        # ...and the MASKED form, on its own key.
        masked_codes = [proxied(L_DATA, key=MASKED_ONLY_KEY)[0] for _ in range(LIMIT + 2)]
        announce("F0b the MASKED-form role", f"codes={masked_codes} (limit {LIMIT})")
        check("F0b: ...while a role whose `matching_key` is that key's MASK is the form that "
              "really fires (2 admits, then 429)",
              masked_codes[:LIMIT] == [200] * LIMIT and masked_codes[-1] == 429,
              f"codes={masked_codes} (matching_key={MASKED_LIMIT_KEY!r})")
        # ...and the second half of D-15: two DIFFERENT raw keys with the same mask are one quota.
        collide = proxied(L_DATA, key=COLLIDE_KEY)
        announce("F0b a different raw key with the same mask",
                 f"{COLLIDE_KEY!r} -> HTTP {collide[0]} (its mask is {MASKED_LIMIT_KEY!r} too)")
        check("F0b: ...so two distinct client keys whose masks coincide share ONE quota — the "
              "second key is refused by the first key's spent window (D-15, second consequence)",
              collide[0] == 429, f"HTTP {collide[0]}")
        edge = start("edge", E_ADMIN, E_DATA, "fid-edge")
        if not wait_for(lambda: call("GET", f"http://127.0.0.1:{E_ADMIN}/healthz")[0] == 200,
                        budget=25):
            print("[fidelity] CANNOT VERIFY: the edge never became healthy", file=sys.stderr)
            return 2
        served = wait_for(lambda: proxied(E_DATA)[0] == 200, budget=25)
        check("F0: the edge serves the tenant from its own snapshot (control)",
              served, f"HTTP {proxied(E_DATA)[0]}")

        # ---- F1: the limit role is enforced ON THE EDGE ------------------------
        codes = [proxied(E_DATA, key=EDGE_KEY)[0] for _ in range(LIMIT + 2)]
        announce("F1 the edge's admits/denials", f"codes={codes} (limit {LIMIT})")
        check(f"F1: the LIMIT ROLE row travelled to the edge — it admits {LIMIT} and then 429s",
              codes[:LIMIT] == [200] * LIMIT and codes[-1] == 429, f"codes={codes}")

        # ---- F2: the tenant access-token HASH travelled ------------------------
        st2, body2 = tenant_api(E_DATA)
        check("F2: the tenant-API ACCESS TOKEN works on the edge (the hash travelled with the "
              "snapshot)", st2 == 200 and '"tenant_id":"t1"' in body2.replace(" ", ""),
              f"HTTP {st2} {body2[:80]}")

        # ---- F3: sub-tenant steering travelled ---------------------------------
        tags = []
        for _ in range(4):
            _, body = proxied(E_DATA, key="SUB_abc")
            tags.append(body.split("chatcmpl-")[-1].split('"')[0] if "chatcmpl-" in body else "?")
        announce("F3 the edge's sub-tenant steering", f"tags={tags}")
        check("F3: the SUB-TENANT + ROUTE rows travelled — the edge pins `shared` to p-b for this "
              "key prefix", tags and all(t == "b" for t in tags), f"tags={tags}")

        # ---- F4/F5: a FRESH replica must materialize the same rows ---------------
        # The edge has no admin API, so it cannot report a promotion (`/healthz/leader` is 404 on an
        # edge — measured), and promotion with leader-role nodes is already covered by
        # `test_cluster_ha.py`. What matters here is the SNAPSHOT CONTRACT, so the strongest
        # available probe is restarting the edge: a brand-new replica must materialize the snapshot
        # from the leader and behave identically.
        stop(edge)
        edge = start("edge", E_ADMIN, E_DATA, "fid-edge")
        if not wait_for(lambda: call("GET", f"http://127.0.0.1:{E_ADMIN}/healthz")[0] == 200,
                        budget=25):
            check("F4: the edge restarted", False, "never healthy")
        else:
            served2 = wait_for(lambda: proxied(E_DATA)[0] == 200, budget=25)
            st4, body4 = proxied(E_DATA, key=EDGE_KEY)
            announce("F4 the fresh replica", f"served={served2} limit-key HTTP {st4} "
                     f"{body4[:60]!r}")
            check("F4: a RESTARTED edge re-materializes the snapshot and still enforces the "
                  "tenant's limit role", st4 == 429, f"HTTP {st4} {body4[:80]}")
            st5, body5 = tenant_api(E_DATA)
            check("F5: ...and still authenticates the tenant-API token",
                  st5 == 200 and '"tenant_id":"t1"' in body5.replace(" ", ""),
                  f"HTTP {st5} {body5[:70]}")
            _, body6 = proxied(E_DATA, key="SUB_other")
            check("F5: ...and still steers the sub-tenant",
                  "chatcmpl-b" in body6, f"{body6[:70]}")

        # ---- F8: the WIRE the §13 claim is actually about (round 169) -----------------------
        # Until now this drill's header said the "disabled rows travel" half of §13 had NO live
        # observable, because an EDGE carries no admin API — true, but the claim is about the
        # PUBLISHER's wire contract, and that has an HTTP observable: the leader serves
        # `GET /api/v1/internal/control` (cluster-token gated) with the `SnapshotWire` as JSON.
        # MEASURED 2026-10-01 on this drill's own fixtures: `fidelity.limit_roles` carries 5 rows
        # INCLUDING the disabled `r-off`, while `cfg.limit_roles` carries 4 — the disabled row is
        # exactly the difference between the two payloads, which is why the fidelity set exists.
        # This leg must run BEFORE F7 (F7 ENABLES `r-off`, so afterwards the disabled-row claim has
        # nothing left to point at).
        # PREMISE, asserted instead of assumed: every check below is about a DISABLED row, and F7
        # (which runs immediately after) ENABLES `r-off`. If this leg is ever moved after F7 it must
        # fail HERE and loudly, rather than quietly measuring an enabled row and staying green — the
        # order dependency is stated in a comment above, and comments do not fail.
        pre_roles, pre_bindings, st_pre, st_preb = [], [], 0, 0
        st_pre, raw_pre = admin("GET", "/limit-roles")
        st_preb, raw_preb = admin("GET", "/provider-key-bindings")
        try:
            pre_roles = [r.get("enabled") for r in json.loads(raw_pre) if r.get("id") == "r-off"]
            pre_bindings = [b.get("enabled") for b in json.loads(raw_preb) if b.get("id") == "b-off"]
        except ValueError:
            pass
        check("F8 PREMISE: `r-off` and `b-off` are DISABLED on the leader at this moment (F7 enables "
              "`r-off`, so this leg must run BEFORE it)",
              st_pre == 200 and st_preb == 200 and pre_roles == [False] and pre_bindings == [False],
              f"GET /limit-roles -> {st_pre} r-off enabled={pre_roles}; "
              f"GET /provider-key-bindings -> {st_preb} b-off enabled={pre_bindings}")
        st_admin, _, _ = control_snapshot(L_ADMIN, ADMIN_TOKEN)
        st_anon, _, _ = control_snapshot(L_ADMIN, None)
        check("F8: the control payload is CLUSTER-token gated (the admin token and no token are "
              "both refused)", st_admin == 401 and st_anon == 401,
              f"admin token -> {st_admin}, no token -> {st_anon}")
        st8w, snap8, raw8 = control_snapshot(L_ADMIN, CLUSTER_TOKEN)
        wire = (snap8 or {}).get("snapshot") or {}
        fid8 = wire.get("fidelity") or {}
        cfg8 = wire.get("cfg") or {}
        roles_fid = {r.get("id"): r for r in (fid8.get("limit_roles") or [])}
        roles_cfg = {r.get("id"): r for r in (cfg8.get("limit_roles") or [])}
        bind_fid = {b.get("id"): b for b in (fid8.get("key_prefix_bindings") or [])}
        bind_cfg = {b.get("id"): b for b in (cfg8.get("key_prefix_bindings") or [])}
        hashes8 = fid8.get("tenant_token_hashes") or []
        raw_hash = ((hashes8[0].get("hash") or {}) if hashes8 else {})
        announce("F8 the leader's published snapshot",
                 f"HTTP {st8w} wire_version={wire.get('wire_version')} version={wire.get('version')} "
                 f"fidelity.roles={sorted(k for k in roles_fid if k)} "
                 f"cfg.roles={sorted(k for k in roles_cfg if k)} "
                 f"fidelity.bindings={sorted(k for k in bind_fid if k)} "
                 f"cfg.bindings={sorted(k for k in bind_cfg if k)} "
                 f"token_hash_rows={len(hashes8)}")
        check("F8: the DISABLED limit role (`r-off`) IS in the published fidelity rows",
              st8w == 200 and roles_fid.get("r-off") is not None
              and roles_fid["r-off"].get("enabled") is False,
              f"HTTP {st8w} roles={sorted(k for k in roles_fid if k)}")
        check("F8: ...and it is NOT in the ordinary config payload — the two payloads differ by "
              "exactly the disabled row, which is the mechanism §13 describes",
              roles_fid.get("r-off") is not None and "r-off" not in roles_cfg,
              f"fidelity has r-off={'r-off' in roles_fid}, cfg has r-off={'r-off' in roles_cfg}")
        check("F8: the DISABLED `provider_key_binding` (`b-off`) travels too (the second half of "
              "the §13 sentence)",
              bind_fid.get("b-off") is not None and bind_fid["b-off"].get("enabled") is False
              and "b-off" not in bind_cfg,
              f"fidelity.bindings={sorted(k for k in bind_fid if k)} "
              f"cfg.bindings={sorted(k for k in bind_cfg if k)}")
        # The payload carries SECRET material by design (sealed), so the leg that matters is that it
        # is SEALED: the tenant's raw access token must not appear anywhere in the body, and the
        # sealed container must be a sealed container (three-part ciphertext record), not a hash.
        check("F8: the tenant's access token travels SEALED (no plaintext token anywhere in the "
              "published body) and the sealed record has the sealing shape",
              len(hashes8) >= 1 and TENANT_TOKEN not in raw8
              and set(raw_hash) >= {"ciphertext", "nonce", "key_version"},
              f"rows={len(hashes8)} sealed_fields={sorted(raw_hash)} "
              f"plaintext_present={TENANT_TOKEN in raw8}")
        # The remaining fidelity fields are counted, not judged row-by-row: their content is what the
        # F1-F5 legs already observe behaviourally on the edge.
        check("F8: ...and the rest of the fidelity set is populated (models / tenant joins / "
              "sub-tenant route), so the edge legs are not reading an empty payload",
              len(fid8.get("provider_models") or []) >= 2
              and len(fid8.get("tenant_providers") or []) >= 1
              and len(fid8.get("sub_tenant_routes") or []) >= 1,
              f"provider_models={len(fid8.get('provider_models') or [])} "
              f"tenant_providers={len(fid8.get('tenant_providers') or [])} "
              f"sub_tenant_routes={len(fid8.get('sub_tenant_routes') or [])}")

        # ---- F7: a role ENABLED on the leader must reach the edge without a restart --------
        # The mechanism this pins (measured while trying to falsify the fidelity claim): ENABLED
        # roles travel in the ordinary config payload (`store.rs` loads `limit_roles` filtered to
        # `enabled`), while the separate "fidelity" payload carries the full set INCLUDING disabled
        # rows — so dropping the latter changes nothing observable for an ENABLED role.
        # Round 169: the disabled half is now observed directly on the publisher's wire (F8), which is
        # what §13 is about; what remains unobservable is an EDGE-side BEHAVIOUR for a disabled row,
        # because a disabled row has no behaviour. What is observable here is the other direction: a
        # role which starts DISABLED and is then enabled reaches the running edge on the next snapshot.
        st7, out7 = admin("PUT", "/limit-roles/r-off", {
            "id": "r-off", "name": "r-off", "matching_key": LATE_MASK, "matching_model": None,
            "matching_tenant": "t1", "matching_provider": None, "limit_count": LIMIT,
            "limit_token": None, "window": "h", "enabled": True, "created_at": ""})
        admin("POST", "/reload", {})
        # The LEADER first (the control that a role of this shape works at all — the point D-15
        # makes: a raw-key role is inert on every node, which would otherwise look like a
        # replication failure), then the EDGE.
        late_leader = wait_for(lambda: proxied(L_DATA, key=LATE_KEY)[0] == 429, budget=20)
        announce("F7 the late-enabled role on the LEADER",
                 f"PUT HTTP {st7} {out7[:40]!r}; leader enforced={late_leader}")
        check("F7: the role enabled during the run is enforced by the LEADER (control)",
              st7 == 200 and late_leader, f"PUT={st7} leader enforced={late_leader}")
        late_edge = wait_for(lambda: proxied(E_DATA, key=LATE_KEY)[0] == 429, budget=20)
        check("F7: ...and that ENABLED role reaches the running EDGE on the next snapshot "
              "(no restart)", late_edge, f"edge enforced={late_edge}")

        # ---- F6: the leader dies — the edge keeps serving from its snapshot ----
        leader.kill()
        leader.wait(timeout=10)
        time.sleep(1.0)
        st7, body7 = proxied(E_DATA)
        st8, _ = proxied(E_DATA, key=EDGE_KEY)
        check("F6: with the leader gone the edge keeps serving (last-known-good snapshot)",
              st7 == 200, f"HTTP {st7} {body7[:60]}")
        check("F6: ...and the limit is still enforced there (the window lives in Redis and the "
              "role row is in its snapshot)", st8 == 429, f"HTTP {st8}")

        # ---- P1-P4: the PROMOTED node must still know what a replica knows ---------------------
        # The gap round 163 documented: `test_cluster_ha.py` promotes a standby but never re-checks
        # limit roles / token hashes / sub-tenant routes on it, and this drill used to restart an edge
        # and call that "promotion". The leader is dead here, so the standby wins the lease and the
        # assertions run AGAINST IT — each one discriminates: without the row the request is routed
        # to the live upstream (200) or the token is refused (401).
        promoted = wait_for(lambda: leader_probe(S_ADMIN) == 200, budget=25)
        announce("P1 the standby after the leader died", f"/healthz/leader={leader_probe(S_ADMIN)}")
        check("P1: the standby is PROMOTED once the leader dies (the legs below need it)",
              promoted, f"leader_probe={leader_probe(S_ADMIN)} (503 = still standby)")
        if promoted:
            p2 = proxied(S_DATA, key=EDGE_KEY)[0]
            # The three outcomes are distinguishable, which is what makes this leg evidence:
            # 429 = the row travelled and the quota fired; 200 = the tenant is known but the ROLE row
            # is missing (the request was routed); 404 = the promoted node has no config for the
            # tenant at all (nothing travelled).
            check("P2: the PROMOTED node still enforces the tenant's limit role", p2 == 429,
                  f"HTTP {p2} (429 = enforced; 200 = role row missing; 404 = no config at all)")
            st3, body3 = tenant_api(S_DATA)
            check("P3: ...and still authenticates the tenant-API token (its hash travelled)",
                  st3 == 200 and '"tenant_id":"t1"' in body3.replace(" ", ""),
                  f"HTTP {st3} {body3[:70]}")
            tags_p = []
            for _ in range(4):
                _, body_p = proxied(S_DATA, key="SUB_abc")
                tags_p.append(body_p.split("chatcmpl-")[-1].split('"')[0]
                              if "chatcmpl-" in body_p else "?")
            check("P4: ...and still steers the sub-tenant's per-model route",
                  tags_p and all(t == "b" for t in tags_p), f"tags={tags_p}")
    finally:
        stop(leader)
        stop(standby)
        stop(edge)
        upstream.shutdown()

    print()
    if failures:
        print(f"REPLICA FIDELITY: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("REPLICA FIDELITY: PASSED (limit roles, tenant access-token hashes and sub-tenant routes "
          "are present on the edge, survive a RESTART of that replica, and — P1-P4 — are still "
          "enforced by the node the lease PROMOTES after the leader dies; F8 additionally reads the "
          "publisher's OWN wire and finds the disabled rows §13 is about in `fidelity`, absent from "
          "`cfg`, with the tenant token sealed)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
