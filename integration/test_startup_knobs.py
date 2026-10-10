#!/usr/bin/env python3
"""Startup knobs: refuse, or say out loud that the setting is being dropped.

Ported to the member-list topology (ADR-0001, 2026-10-05). The drill used to be built on
`HYDRA_ROLE`: it made a "cluster node" by setting `HYDRA_ROLE=edge` and the cluster contract by
setting `HYDRA_CONTROL_URL` / `HYDRA_PUBLIC_URL`. Every one of those names is retired, so 11 of its
14 checks went red while the product was behaving correctly — the FOUNDATION had moved, not the
promises. What the port had to change, and why, leg by leg (see the per-leg comments for the rest):

  * a "cluster node" is now one with `HYDRA_CLUSTER_PEERS` set. It cannot be faked with a stub
    list: `MINIMUM_MEMBERS = 3`, `HYDRA_ARACHNE_LISTEN` must match its own entry, and
    `await_cluster_preflight` refuses to serve from an Arachne data directory that no member has
    adopted yet — measured 2026-10-05: a LONE member of a three-member list dies after 10 s with
    "no member adopted this node's Arachne data directory within 10s". So this drill really brings
    up THREE members once, waits for a leader and for adoption, stops them again, and uses ONE of
    them as the restartable VICTIM every cluster leg acts on. Its own directory is adopted by then,
    and adoption is a LOCAL read (`get_stale`), so the victim restarts alone in well under a second
    (verified by hand 2026-10-05: leader elected, trio killed, member A alone serving 200 in ~500 ms).
  * the ROLE dimension is gone (there is exactly ONE path to "wiring configured, member list
    missing"), so the four role variants K4/K5/K6/K11 collapse into K4 plus K5, which pins the
    behaviour the four of them could not: BOTH mistakes at once reach the operator in ONE line.
  * K6 and K11 are re-pointed at facts that still exist: a retired variable must be REPORTED
    (K6 — the inverse of the old "role=all stays silent"), and reported even on a HEALTHY cluster
    node (K11).

Two operator-facing promises are still the subject, and both survived the port:

  * `ops.md` §13.3 / `cluster.md` §2: an unsupported `HYDRA_REDIS_MODE` **fails fast at startup**
    ("a typo must not silently mean `single`"). Since DD-1 (2026-10-09) that promise holds on
    ANY role: the mode used to be read only inside `if role.is_cluster()` (`main.rs`), so the
    single-node default silently ignored a misspelt value (round 194 measured that the unqualified
    wording in four doc lines claimed a fail-fast that did not exist on the default deployment).
    K12 now pins the TIGHTENED boundary: `HYDRA_REDIS_MODE` is validated whenever it is SET,
    so the single-node default also rejects the typo;
  * a deployment that configures cluster settings it is not going to use must SAY SO, naming every
    variable — the diagnostic that used to be silent (round 192).

Every leg observes the REAL binary (process exit code + log text), never a helper:

  K1  `HYDRA_REDIS_MODE=clustr` on a CLUSTER node (member list set, own raft address, adopted data
      directory) ⇒ non-zero exit, and the message names the knob AND the misspelt value the
      operator typed. Before round 191 this node BOOTED as `single` — a topology switch silently
      ignored.
  K2  ...and so does `HYDRA_REDIS_MODE=sentinel` — through the parse catch-all, NOT through the
      `RedisMode::Sentinel` arm in `redis/mod.rs`: `parse` refuses the value before any mode is
      constructed, so no environment path can reach that arm (it stays reachable only for a direct
      caller of the public API, which is why the arm still exists).
  K3  control: the same member with `single` really boots AND reports itself as a cluster node, so
      K1/K2 are not "cluster mode is broken for some other reason".
  K4  cluster wiring WITHOUT the member list keeps serving, but reports EVERY variable it drops.
      ONE path reaches this diagnosis now — the role variants that used to be K4/K5/K6/K11 are gone
      with the role, and this is that path.
  K5  ...and when a RETIRED variable is set at the same time, the SAME line names both: the
      standalone wiring AND the retired names. Retirement used to return early, so a node halfway
      through this migration heard only "remove HYDRA_ROLE" and was never told it had stopped being
      a cluster node at all.
  K6  a retired variable ALONE is reported, and the node is NOT called standalone (it is a
      single-node default with one stale setting, which is a different fact with a different fix).
  K7  control: nothing cluster-shaped ⇒ completely silent. A rule that cannot stay quiet on correct
      input is noise, not a guard.
  K8  `HYDRA_CLUSTER_PEERS="   "` (whitespace only) is NOT a cluster: the node serves as a
      single node and names the wiring it drops. A stray space pasted into a manifest must not
      silently take a node out of the cluster either way — and `publish` must never be told it is
      a member.
  K9  `HYDRA_NON_ROUTE_STRATEGY` refuses an unrecognised strategy by name. That one already had a
      unit test; this pins it where an operator actually sees it.
  K10 every cluster-only variable the source declares EXCEPT the member list itself is configured
      ⇒ the fallback ERROR names every one of them. Round 193 measured the defect: the list named
      three and dropped the rest silently.
  K11 a retired variable is reported on a HEALTHY cluster node too — quorum, a leader and a served
      config, and the ERROR must still be there.
  K12 the BOUNDARY of K1/K2, tightened by DD-1 (2026-10-09): the mode used to be
      read only inside `if role.is_cluster()`, so the single-node default never
      validated a misspelt value. Now `HYDRA_REDIS_MODE` is validated whenever it
      is SET — any role — so the misspelt value is rejected on the single-node
      default too; this leg pins the opposite of the old boundary.
  K13 a data directory that has never been claimed, with NO majority up, refuses to start AFTER the
      10 s deadline — and the ERROR says what to DO about it (start a majority together /
      `podManagementPolicy: Parallel`). Measured 2026-10-05: the message named only the symptom, and
      the two actions took an afternoon of reading source to find; this leg is what keeps the
      guidance in the message. It runs BEFORE the trio starts, because the directory must still be
      unclaimed — and it is also the leg that proves the deadline is real.

Run: python3 integration/test_startup_knobs.py        # needs target/debug/hydra + a Redis
     HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_startup_knobs.py
Exit 0 pass · 1 an assertion failed · 2 could not verify (no binary / no Redis / no quorum).
"""
import base64
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "startup-knobs-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN_TOKEN = "hydra-startup-knobs-admin-2026"
# 32 raw bytes, base64: the inline form of the master key (see ops.md §1.2).
MASTER_KEY = base64.b64encode(b"S" * 32).decode()

# Ports clear of the dev stack (8080-8084/8090) and of the other suites (188xx is taken by
# the clickhouse/usage drills). Two blocks, because the legs come in two kinds:
#   * the SINGLE-NODE legs reuse one pair (ADMIN/DATA) leg by leg — serially, and every node is
#     killed before the next leg starts;
#   * the CLUSTER legs need three members alive at once (see `bring_up_cluster`), so each member
#     gets its own admin/data/raft triple, and the victim the cluster legs act on is member C.
ADMIN, DATA = 18910, 18911
DEAD_CH = "http://127.0.0.1:18999"          # nothing listens: a cluster node needs a CH URL
REDIS_BASE = os.environ.get("HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380").rstrip("/")
REDIS_DB = int(os.environ.get("HYDRA_STARTUP_KNOBS_REDIS_DB", "52"))

# The three members of the drill's own cluster. `A` and `B` stay up as the quorum; `C` is the
# VICTIM: every cluster leg starts it with different settings and kills it again.
CLUSTER_NODES = (
    # node id, admin port, data port, raft port
    ("knobs-a", 18930, 18931, 18941),
    ("knobs-b", 18932, 18933, 18942),
    ("knobs-c", 18934, 18935, 18943),
)
VICTIM = CLUSTER_NODES[2]
PEERS = ",".join(f"{nid}=127.0.0.1:{raft}" for nid, _a, _d, raft in CLUSTER_NODES)
# How long to wait for the three members to elect a leader and adopt their data directories. Two
# election timeouts on the LAN profile plus room for a cold start; the drill gives up with CANNOT
# VERIFY rather than reporting product failures it cannot attribute.
CLUSTER_BUDGET = 25.0

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def http(url, token=None, timeout=3):
    """Status code, or 0 when nothing answers (the same convention as the other drills)."""
    headers = {} if token is None else {"Authorization": f"Bearer {token}"}
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=timeout) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return 0


def redis_url():
    """`.../52` — a DB of our own, cleared before the legs run.

    Round 194 (adversarial review): isolation used to be only a claim — the DB was never cleared, and
    a probe found FOREIGN keys in it (`hydra:{ctl:inv:applied}:node-up`, written by another suite that
    also uses DB 52). No leg reads Redis, so nothing was wrongly green; but the comment said
    "isolated" while the state was shared. This drill now FLUSHes its DB, exactly as
    `test_cluster_ha.py` learned to (whose header records the outage that taught it).
    """
    return f"{REDIS_BASE}/{REDIS_DB}"


def redis_endpoint():
    parts = urlsplit(REDIS_BASE)
    return parts.hostname or "127.0.0.1", parts.port or 6379


def redis_cmd(sock, *parts):
    payload = ("*%d\r\n" % len(parts)).encode()
    for part in parts:
        b = str(part).encode()
        payload += b"$%d\r\n" % len(b) + b + b"\r\n"
    sock.sendall(payload)
    return sock.recv(256)


def flush_redis_db():
    """Clear our own DB and return a note. Never fatal: no leg reads Redis."""
    host, port = redis_endpoint()
    try:
        sock = socket.create_connection((host, port), timeout=5)
    except OSError as e:
        return f"{host}:{port} unreachable ({e})"
    try:
        redis_cmd(sock, "SELECT", REDIS_DB)
        before = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
        redis_cmd(sock, "FLUSHDB")
        after = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    finally:
        sock.close()
    return f"db {REDIS_DB}: keys {before} -> {after}"


def port_is_free(port):
    """A bind test, so a FOREIGN listener cannot make the "it booted" legs wrongly green.

    Round 194 (adversarial review): the legs judge "started" by an HTTP 200 on 127.0.0.1:18910, so
    with `python3 -m http.server 18910` running six legs pass while not one gateway process starts.
    """
    with socket.socket() as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False


def redis_reachable():
    host, port = redis_endpoint()
    try:
        with socket.create_connection((host, port), timeout=3):
            return True
    except OSError:
        return False


def base_env():
    """The environment the binary needs to get as far as its own startup checks.

    Every inherited `HYDRA_*` is dropped first: a developer shell with a retired variable such as
    `HYDRA_ROLE` exported would otherwise silently rewrite the very legs under test (K7 asserts
    SILENCE, K6 asserts that a retired name is reported, and an inherited variable would break both
    for the wrong reason).
    """
    env = {k: v for k, v in os.environ.items() if not k.startswith("HYDRA_")}
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN,
        "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'knobs.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": MASTER_KEY,
        "RUST_LOG": "info",
    })
    return env


# The env contract a cluster node passes, as the shipped compose does: the member list, its own
# raft address, a Redis backbone, the shared cluster token and a ClickHouse sink (mandatory in
# cluster mode). Retired names are deliberately ABSENT: they are what K5/K6/K11 set on purpose.
CLUSTER_ENV = {
    "HYDRA_CLUSTER_PEERS": PEERS,
    "HYDRA_REDIS_URL": redis_url(),
    "HYDRA_REDIS_MODE": "single",
    "HYDRA_USAGE_SINK": "clickhouse",
    "HYDRA_CLICKHOUSE_URL": DEAD_CH,
}
# The WIRING variables — every cluster-only name EXCEPT the member list itself, i.e. exactly the set
# a node configured "for a cluster" while forgetting the one variable that makes it one. This is what
# the diagnostic must name for K4/K5/K8.
#
# It is NOT "every key of CLUSTER_ENV minus the member list": that dict also carries the usage sink
# and the ClickHouse URL, which are not cluster-only and which the diagnostic rightly does not name
# (the first version of this port used that dict and K4/K5 went red for exactly that reason — a
# mutation-derived set is not a derived set). K4 therefore also asserts that this table still EQUALS
# `CLUSTER_ONLY_ENV` minus the member list, so a name added to the source table fails the drill
# instead of being quietly un-set here.
WIRING = {
    "HYDRA_REDIS_URL": redis_url(),
    "HYDRA_REDIS_MODE": "single",
    "HYDRA_NODE_ID": "knobs-wiring",
    "HYDRA_ARACHNE_LISTEN": "127.0.0.1:18944",
    "HYDRA_CLUSTER_ID": "knobs-drill-cluster",
}
# Variables this plan RETIRED (`RETIRED_CLUSTER_ENV` in the source, read below). Set on purpose by
# K5/K6/K11 to pin the "these do nothing now" diagnostic.
# Two retired names on purpose, and the second one is the interesting case: `HYDRA_CLUSTER_TOKEN` was
# REQUIRED in cluster mode until 2026-10-05, so a deployment that still carries it is the most likely
# reader of the retirement notice. Both must be named.
RETIRED = {"HYDRA_ROLE": "all", "HYDRA_CLUSTER_TOKEN": "still-set-by-an-old-manifest"}
# A value this drill can set each cluster-only variable to, for K10. The NAMES are not copied here —
# they are read from `CLUSTER_ONLY_ENV` in the source (see `cluster_only_names`) — so this leg grows
# with the table instead of with a hand-maintained list (round 194: the list used to be copied by
# hand, which meant the wire leg could never notice a NEW table entry).
#
# `HYDRA_CLUSTER_PEERS` is deliberately ABSENT: setting it makes the node a cluster member, which
# switches the node from "wiring I am dropping" to "wiring I am using", and K10 is about the
# former. The leg asserts that it is the ONLY name without a value, so a NEW table entry still
# fails this drill instead of being skipped.
KNOB_VALUES = {
    "HYDRA_REDIS_URL": redis_url,
    "HYDRA_REDIS_MODE": lambda: "single",
    "HYDRA_NODE_ID": lambda: "knobs-all",
    "HYDRA_ARACHNE_LISTEN": lambda: "127.0.0.1:18944",
    "HYDRA_CLUSTER_ID": lambda: "knobs-drill-cluster",
}


def cluster_only_names():
    """`CLUSTER_ONLY_ENV` as the SOURCE declares it — the table the fallback ERROR must print.

    Read from the file rather than repeated here: a name added to the table is then covered by K10
    automatically, and a name this drill has no value for fails K10 instead of being skipped.
    """
    src = open(os.path.join(ROOT, "crates/hydra-server/src/cluster/mod.rs"), encoding="utf-8").read()
    m = re.search(r"const\s+CLUSTER_ONLY_ENV\s*:\s*\[&str;\s*(\d+)\]\s*=\s*\[(.*?)\];", src, re.S)
    if not m:
        raise AssertionError("CLUSTER_ONLY_ENV declaration not found — re-verify, then update this drill")
    names = re.findall(r'"([A-Z0-9_]+)"', m.group(2))
    if len(names) != int(m.group(1)) or not names:
        raise AssertionError(f"CLUSTER_ONLY_ENV parsed as {names} but declares {m.group(1)} entries")
    # The DECLARED length travels with the names so the caller can assert it again: a falsification of
    # this very leg (returning `names[:3]`, i.e. the hand-written three-name list this leg replaced)
    # PASSED until the count was checked at the point of use — the truncation happened after the
    # internal check, so only the caller could see it (measured 2026-10-01, round 194).
    return names, int(m.group(1))



def observe(label, extra, budget=9.0, admin=ADMIN):
    """Start a node and watch it: boot, answer, or die.

    Returns `(state, rc, log)` where state is "up" (a health endpoint answered 200), "alive" (still
    running when the budget expired) or "exited" (it refused to start, `rc`). The distinction
    matters: "did not exit" is the honest evidence for a node that must keep serving, while a
    refusal leg needs the EXIT CODE — a process that is merely slow to fail would otherwise pass.

    `admin` is the port to probe. `/healthz` is token-free and `/api/v1/health` needs the admin
    token; either one answering 200 means the node is serving. The cluster legs pass their member's
    port, because the single-node block's port belongs to a different process at that point.
    """
    env = base_env()
    env.update(extra)
    log_path = os.path.join(DIR, f"{label}.log")
    with open(log_path, "w") as fh:
        proc = subprocess.Popen([BIN], env=env, stdout=fh, stderr=subprocess.STDOUT)
    state, deadline = "exited", time.time() + budget
    try:
        while time.time() < deadline:
            if proc.poll() is not None:
                break
            if http(f"http://127.0.0.1:{admin}/healthz") == 200 or \
               http(f"http://127.0.0.1:{admin}/api/v1/health", token=ADMIN_TOKEN) == 200:
                state = "up"
                break
            time.sleep(0.25)
        else:
            state = "alive"
        if proc.poll() is not None:
            state = "exited"
    finally:
        if proc.poll() is None:
            proc.send_signal(signal.SIGKILL)
            proc.wait(timeout=10)
    with open(log_path) as fh:
        log = fh.read()
    return state, proc.returncode, log


def node_env(node, extra=None):
    """The environment of one cluster MEMBER: its identity, its ports and its own data directory.

    Every member needs all three, and none of them can be shared: `HYDRA_ARACHNE_LISTEN` must equal
    the address the member list holds for it (`ClusterPeersError::ListenMismatch`), and the Arachne
    data directory is per node (it is what the identity preflight reads and adopts).
    """
    nid, admin, data, raft = node
    env = base_env()
    env.update(CLUSTER_ENV)
    env.update({
        "HYDRA_NODE_ID": nid,
        "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin}",
        "HYDRA_LISTEN": f"127.0.0.1:{data}",
        "HYDRA_ARACHNE_LISTEN": f"127.0.0.1:{raft}",
        "HYDRA_ARACHNE_DATA_DIR": os.path.join(DIR, f"arachne-{nid}"),
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, nid + '.db')}?mode=rwc",
    })
    env.update(extra or {})
    return env


class Member:
    """A running member of the drill's cluster, killed on teardown."""

    def __init__(self, node, log_name):
        self.node = node
        self.nid, self.admin, self.data, self.raft = node
        self.log_path = os.path.join(DIR, f"{log_name}.log")

    def start(self, extra=None):
        with open(self.log_path, "w") as fh:
            self.proc = subprocess.Popen([BIN], env=node_env(self.node, extra),
                                         stdout=fh, stderr=subprocess.STDOUT)
        return self

    def alive(self):
        return self.proc.poll() is None

    def serving(self):
        return http(f"http://127.0.0.1:{self.admin}/api/v1/health", token=ADMIN_TOKEN) == 200

    def leads(self):
        # `/healthz/leader` is the ONE token-free route (an LB must route to the writer without a
        # secret); it used to be probed with the cluster token here, which no longer exists.
        return http(f"http://127.0.0.1:{self.admin}/healthz/leader") == 200

    def kill(self):
        if self.proc.poll() is None:
            self.proc.send_signal(signal.SIGKILL)
            self.proc.wait(timeout=10)

    def log(self):
        with open(self.log_path) as fh:
            return fh.read()


def line_about(log, needle):
    """The log line carrying `needle` (tracing writes the fields AFTER the message)."""
    for raw in log.splitlines():
        if needle in raw:
            return raw
    return ""


def adoption_leg(node):
    """K13: an unclaimed data directory + no majority ⇒ refuse, and say what to do.

    Run BEFORE the trio starts: the victim's directory must still carry no cluster identity, which is
    exactly the state a first install is in. The budget is 16 s because the deadline itself is 10 s —
    the leg measures the refusal AND the wait, so an implementation that failed instantly for some
    other reason would not pass it.
    """
    fresh = os.path.join(DIR, "k13-fresh-raft")
    shutil.rmtree(fresh, ignore_errors=True)
    state, rc, log = observe(
        "k13_unclaimed_directory_no_majority",
        node_env(node, {"HYDRA_ARACHNE_DATA_DIR": fresh}),
        budget=16.0,
        admin=node[1],
    )
    line = line_about(log, "no member adopted this node's Arachne data directory")
    check("K13: an unclaimed data directory with no majority up refuses to start (after the 10 s "
          "deadline, not instantly)",
          state == "exited" and rc not in (0, None) and bool(line),
          f"state={state} exit={rc} src={line[:150] or '<SILENT: no adoption error>'}")
    check("K13: ...and the ERROR says what to DO (a majority together / podManagementPolicy: "
          "Parallel), not only what happened",
          "MAJORITY" in line and "start the members together" in line
          and "podManagementPolicy: Parallel" in line,
          f"src={line[-240:] or '<no line>'}")


def bring_up_cluster():
    """Three real members, one leader, adopted data directories — or an honest refusal.

    The Arachne identity preflight (`await_cluster_preflight`, 10 s) refuses to serve from a data
    directory no member has adopted, and only a LEADER can adopt it (the write is
    `without_redirect()`). A lone member therefore never starts: measured 2026-10-05, one member of a
    three-member list with a fresh directory exits after 10 s with "no member adopted this node's
    Arachne data directory within 10s". So the drill brings the trio up TOGETHER, exactly as the
    shipped compose does.

    Adoption is then a LOCAL read on every member that was up when it happened — which is the lever
    the cluster legs use: once adopted, a member restarts alone in well under a second, so the
    victim can be restarted with different settings without a quorum dance per leg.

    Returns the VICTIM member on success (the trio is stopped again first), or prints CANNOT VERIFY
    and returns None. Nothing else needs to stay up: adoption is a local read, so the victim
    restarts alone — verified by hand on 2026-10-05 (leader elected, trio killed, member A alone
    serving 200 within ~500 ms).
    """
    members = [Member(node, f"cluster-{node[0]}") for node in CLUSTER_NODES]
    for m in members:
        m.start()
    deadline = time.time() + CLUSTER_BUDGET
    leader_seen_at = None
    ok = False
    try:
        while time.time() < deadline:
            if not all(m.alive() for m in members):
                break
            if all(m.serving() for m in members) and any(m.leads() for m in members):
                leader_seen_at = time.time()
                break
            time.sleep(0.25)
        if leader_seen_at is None:
            died = [m.nid for m in members if not m.alive()]
            print(
                "[startup-knobs] CANNOT VERIFY: the drill's own three-member cluster did not reach "
                f"a leader within {CLUSTER_BUDGET:.0f}s (dead members: {died or 'none'}) — every "
                "cluster leg would otherwise fail for a reason that is not the knob under test",
                file=sys.stderr,
            )
            for m in members:
                print(f"--- {m.nid} log ---", file=sys.stderr)
                print("\n".join(m.log().splitlines()[-8:]), file=sys.stderr)
            return None
        # Give the adoption write a moment to reach every member's local replica: it is each
        # member's OWN directory that must carry the identity for the restart-without-quorum legs.
        time.sleep(1.5)
        victim = members[2]
        print(f"   ....  three members up, a leader elected; adoption settled. "
              f"victim = {victim.nid} on 127.0.0.1:{victim.admin} (raft {victim.raft})")
        ok = True
        return victim
    finally:
        # Always stop the trio: the legs restart the victim themselves, and leaving a live leader
        # around would change what they observe (and would hold the ports they need).
        for m in members:
            m.kill()
        if ok:
            print("   ....  trio stopped; the victim's own directory is adopted, so it restarts "
                  "alone without a quorum")


def main():
    if not os.path.exists(BIN):
        print(f"[startup-knobs] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    # WIPE the working directory first. It holds each member's Arachne data directory, and a
    # directory that was ADOPTED by a previous run turns K13 (unclaimed + no majority ⇒ refuse) into
    # a node that starts in ~500 ms on its local read — measured: leaving it stale made K13 fail with
    # `state=up`, which is the RIGHT behaviour for an adopted directory and the wrong fixture.
    # (The other multi-process drills wipe theirs for the same reason.)
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    if not redis_reachable():
        print(f"[startup-knobs] CANNOT VERIFY: no Redis at {REDIS_BASE} "
              f"(HYDRA_TEST_REDIS_URL)", file=sys.stderr)
        return 2
    # A foreign listener on our ports would make every "it booted" leg wrongly green (round 194): the
    # legs judge boot by an HTTP 200, not by the process being ours. The raft ports are checked too,
    # because a member that cannot bind its transport is a different failure than the knob under test.
    ports = [ADMIN, DATA] + [p for _n, a, d, r in CLUSTER_NODES for p in (a, d, r)]
    busy = [p for p in ports if not port_is_free(p)]
    if busy:
        print(f"[startup-knobs] CANNOT VERIFY: port(s) {busy} are already in use — every "
              f"\"state=up\" leg would be satisfied by whatever answers there, not by the gateway",
              file=sys.stderr)
        return 2
    print(f"   ....  isolation: {flush_redis_db()}; ports {ports[0]}-{ports[-1]} free")

    # K13 FIRST: it needs the victim's data directory to be UNCLAIMED, which is only true before the
    # trio has ever been up (the bring-up below claims it).
    adoption_leg(CLUSTER_NODES[2])
    victim = bring_up_cluster()
    if victim is None:
        return 2
    try:
        cluster_legs(victim)
        single_node_legs()
    finally:
        victim.kill()

    print()
    if failures:
        print(f"STARTUP KNOBS: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("STARTUP KNOBS: PASSED (refusal + named-value + ignored-wiring + retirement + "
          "silence control, over a real three-member cluster and the single-node default)")
    return 0


def cluster_legs(victim):
    """The legs that need a real cluster node: they act on `victim` (member C).

    `victim` is stopped and restarted leg by leg. Its data directory was adopted while the trio was
    up, and adoption is read from the LOCAL replica, so none of these legs needs a live quorum.
    """
    # ---- K1/K2/K3: the Redis topology switch, ON A CLUSTER NODE ------------------------------
    # `clustr` is the measured round-191 case; `sentinel` is the half the docs always claimed. The
    # node must be a REAL member: the mode is only read inside `if role.is_cluster()` (K12 pins the
    # other side of that boundary), so on the single-node default there is nothing to refuse.
    for kid, lab, mode in (("K1", "k1_redis_mode_clustr", "clustr"),
                           ("K2", "k2_redis_mode_sentinel", "sentinel")):
        state, rc, log = observe(lab, node_env(victim.node, {"HYDRA_REDIS_MODE": mode}),
                                 admin=victim.admin)
        named = f"unsupported HYDRA_REDIS_MODE '{mode}'" in log
        check(f"{kid}: HYDRA_REDIS_MODE={mode} on a cluster node refuses to start and names the "
              f"knob AND the value",
              state == "exited" and rc not in (0, None) and named,
              f"state={state} exit={rc} src={line_about(log, 'HYDRA_REDIS_MODE')[:150] or '<silent>'}")

    state, rc, log = observe("k3_redis_mode_single_control", node_env(victim.node), admin=victim.admin)
    check("K3: control — the same member with the supported mode boots as a CLUSTER node (K1/K2 are "
          "about the knob, not about cluster mode being broken)",
          state in ("up", "alive") and "starting role=cluster" in log
          and "fatal startup error" not in log,
          f"state={state} exit={rc}")

    # ---- K11: a retired variable is reported on a HEALTHY cluster node too -------------------
    # Retirement is not a standalone-node problem: the setting does nothing whether or not the node
    # is a member, so the diagnostic must reach a member as well. There is no quorum in this leg
    # (only the victim is running), which is exactly why it is worth pinning: the retirement notice
    # is emitted by `NodeRole::from_env`, BEFORE anything that needs a leader.
    state, rc, log = observe("k11_retired_on_a_cluster_node", node_env(victim.node, RETIRED),
                             admin=victim.admin)
    line = line_about(log, "were retired by the Arachne control plane")
    check("K11: retired variables are reported even on a cluster node (they do nothing either way), "
          "and the member is NOT called standalone",
          state in ("up", "alive") and bool(line)
          and all(name in line for name in RETIRED)
          and "cluster wiring is configured but" not in line
          and "starting role=cluster" in log,
          f"state={state} exit={rc} src={line[:200] or '<SILENT: nothing named the retirement>'}")


def single_node_legs():
    """The legs that run on the documented single-node default (one pair of ports, reused)."""
    # ---- K4/K5: cluster wiring the node is not going to use ---------------------------------
    # ONE path reaches this diagnosis now. The old drill had four legs (K4/K5/K6/K11) because the
    # diagnosis was reachable through four different role values; with the role retired there is
    # exactly one way to be a non-member with cluster settings, so the four collapse into two that
    # pin different things: that EVERY dropped variable is named (K4), and that a retired variable
    # set at the same time is named in the SAME line (K5).
    # ...and the wiring set this leg configures must still be the whole table minus the member list,
    # or the leg would be asserting on a subset it chose itself.
    table, _declared_len = cluster_only_names()
    expected_wiring = sorted(n for n in table if n != "HYDRA_CLUSTER_PEERS")
    check("K4: ...and this drill configures every cluster-only name the source declares except the "
          "member list (a table that grew must fail HERE, not be silently skipped)",
          sorted(WIRING) == expected_wiring,
          f"drill={sorted(WIRING)} source-minus-peers={expected_wiring}")

    state, rc, log = observe("k4_wiring_without_members", WIRING)
    error_line = line_about(log, "cluster wiring is configured but")
    names_all = bool(error_line) and all(n in error_line for n in WIRING)
    check("K4: with cluster wiring configured and no member list the node still serves, but reports "
          "EVERY variable it will ignore",
          state in ("up", "alive") and names_all,
          f"state={state} exit={rc} src={error_line[:200] or '<SILENT: no line named the wiring>'}")

    state, rc, log = observe("k5_wiring_and_retired_together", {**WIRING, **RETIRED})
    line = line_about(log, "cluster wiring is configured but")
    parts = [l for l in log.splitlines()
             if "cluster wiring is configured but" in l and "were retired by the Arachne control plane" in l]
    check("K5: when a retired variable is set at the same time, the SAME line names BOTH (a node "
          "mid-migration must not hear only the least consequential of its two mistakes)",
          state in ("up", "alive") and len(parts) == 1
          and all(n in parts[0] for n in WIRING)
          and all(name in parts[0] for name in RETIRED),
          f"state={state} exit={rc} lines={len(parts)} src={(line or '<no line>')[:200]}")

    # ---- K6: a retired variable ALONE, on a single-node default -------------------------------
    state, rc, log = observe("k6_retired_alone", RETIRED)
    line = line_about(log, "were retired by the Arachne control plane")
    check("K6: retired variables ALONE are reported, and the node is NOT called standalone (one "
          "stale setting and a dropped cluster are different mistakes)",
          state in ("up", "alive") and bool(line)
          and all(name in line for name in RETIRED)
          and "cluster wiring is configured but" not in line,
          f"state={state} exit={rc} src={line[:200] or '<SILENT: nothing named the retirement>'}")

    # ---- K7: the silence control ---------------------------------------------------------------
    state, rc, log = observe("k7_default_silent", {})
    noisy = [l for l in log.splitlines()
             if "cluster wiring" in l or "were retired" in l or "HYDRA_CLUSTER_PEERS" in l]
    check("K7: control — with nothing cluster-shaped the node boots with no complaint at all "
          "(the documented single-node default is not a warning)",
          state in ("up", "alive") and not noisy,
          f"state={state} exit={rc} noise={noisy[:1]}")

    # ---- K8: a whitespace member list is NOT a cluster ----------------------------------------
    # The old K8 pinned that `" edge "` was still the edge role. The role is gone; the equivalent (and
    # more consequential) mistake is a member list whose value is whitespace — a stray character
    # pasted into a manifest. The node must treat it as "no member list": serve as a single node and
    # NAME the wiring it is dropping, rather than start raft on a list of nothing.
    state, rc, log = observe("k8_whitespace_member_list",
                             {**WIRING, "HYDRA_CLUSTER_PEERS": "   "})
    error_line = line_about(log, "cluster wiring is configured but")
    check("K8: HYDRA_CLUSTER_PEERS=\"   \" is NOT a cluster — the node serves as a single node and "
          "names the wiring it drops",
          state in ("up", "alive") and bool(error_line) and "fatal startup error" not in log,
          f"state={state} exit={rc} src={error_line[:200] or '<SILENT: no line named the wiring>'}")

    # ---- K9: the sibling fail-fast that never had an executable contract -----------------------
    state, rc, log = observe("k9_non_route_strategy", {"HYDRA_NON_ROUTE_STRATEGY": "rejekt"})
    check("K9: an unrecognised HYDRA_NON_ROUTE_STRATEGY refuses to start and quotes the value "
          "(a safety control must not silently become `passthrough`)",
          state == "exited" and rc not in (0, None) and '"rejekt"' in log
          and "HYDRA_NON_ROUTE_STRATEGY" in log,
          f"state={state} exit={rc} src={line_about(log, 'HYDRA_NON_ROUTE_STRATEGY')[:150] or '<silent>'}")

    # ---- K10: the list of dropped settings must be COMPLETE ------------------------------------
    # `CLUSTER_ONLY_ENV` is the single owner of "what counts as cluster wiring". Round 193 measured the
    # defect this pins: the diagnostic named three variables while the rest were configured, dropped,
    # and never mentioned — an operator reading "these three are ignored" reasonably concludes the
    # others took effect. The member list is the ONE name this leg cannot set (setting it makes the
    # node a member, and then nothing is being dropped), so the assertion is: every name EXCEPT the
    # member list has a value here, and every one of them is named.
    names, declared = cluster_only_names()
    unvalued = sorted(n for n in names if n not in KNOB_VALUES)
    if unvalued != ["HYDRA_CLUSTER_PEERS"]:
        check(f"K10: this drill can supply a value for every cluster-only variable except the member "
              f"list (declared {names})", False,
              f"no-value-for={unvalued or 'none'} — the table grew and this drill did not keep up")
    else:
        configured = {n: KNOB_VALUES[n]() for n in names if n in KNOB_VALUES}
        state, rc, log = observe("k10_every_cluster_only_knob", configured)
        line = line_about(log, "cluster wiring is configured but")
        missing = sorted(name for name in configured if name not in line)
        check(f"K10: with every cluster-only variable the source declares except the member list "
              f"configured ({len(configured)} of {len(names)}), the fallback ERROR names every one of "
              f"them (none is dropped in silence)",
              state in ("up", "alive") and not missing and len(names) == declared,
              f"state={state} exit={rc} parsed={len(names)}/{declared} missing={missing or 'none'} "
              f"src={line[:400] or '<no line>'}")

    # ---- K12: the BOUNDARY of K1/K2 — tightened by DD-1 (2026-10-09) --------
    # The mode used to be read only inside `if role.is_cluster()`, so on the
    # single-node default a misspelt value was silently ignored (this leg pinned
    # that gap). DD-1 (2026-10-09) validates `HYDRA_REDIS_MODE` whenever it is
    # SET — any role — so the single-node default now REJECTS the typo too,
    # naming the knob AND the value, and the node does not serve.
    state, rc, log = observe("k12_redis_mode_typo_single_node", {"HYDRA_REDIS_MODE": "clustr"})
    rejected = "unsupported HYDRA_REDIS_MODE" in log
    check("K12 (DD-1): on the single-node default the misspelt HYDRA_REDIS_MODE IS validated — the "
          "node refuses to start and names the knob AND the value",
          state not in ("up", "alive") and rejected and "fatal startup error" in log,
          f"state={state} exit={rc} rejected={rejected} log-names-value={'clustr' in line_about(log, 'HYDRA_REDIS_MODE')}")


if __name__ == "__main__":
    sys.exit(main())
