#!/usr/bin/env python3
"""Startup knobs: refuse, or say out loud that the setting is being dropped.

The shared-DB architecture changed the foundation this drill was built on (again). The raft
member list is gone: a node is a "cluster node" **iff `HYDRA_REDIS_URL` is set**. There is no
leader and no quorum to wait for, so every leg below is a single real process against a real
database and a real Redis; nothing is brought up as a three-member raft group and nothing probes
`/healthz/leader`.

Two operator-facing promises are the subject:

  * an unsupported `HYDRA_REDIS_MODE` fails fast at startup, naming the knob and the bad value —
    on a cluster node AND on the single-node default (DD-1);
  * a deployment that configures cluster-shaped settings it is not going to use must SAY SO,
    naming every live variable it drops AND every retired variable it still carries. Retired
    names include `HYDRA_CLUSTER_PEERS`, `HYDRA_CLUSTER_ID`, `HYDRA_ARACHNE_*` and the old
    role/lease/control names — none of them makes a node a cluster member any more.

Legs (real binary, exit code + log text):

  K1  `HYDRA_REDIS_MODE=clustr` on a CLUSTER node (Redis URL set) ⇒ non-zero exit, names knob+value
  K2  ...and so does `sentinel` (the parse catch-all, before any mode is constructed)
  K3  control: the same node with `single` boots as a cluster node (`role=cluster`)
  K4  live cluster wiring WITHOUT `HYDRA_REDIS_URL` keeps serving, but reports EVERY dropped name
  K5  ...and when a retired variable is set too, the SAME line names both groups
  K6  a retired variable ALONE is reported, and the node is not called standalone
  K7  control: nothing cluster-shaped ⇒ completely silent
  K8  `HYDRA_CLUSTER_PEERS` ALONE no longer makes a cluster: the node serves as a single node and
      reports the retirement (the member list is inert)
  K9  an unrecognised `HYDRA_NON_ROUTE_STRATEGY` refuses to start, quoting the value
  K10 every live cluster-only variable the source declares EXCEPT `HYDRA_REDIS_URL` is configured
      ⇒ the fallback ERROR names every one of them
  K11 retired variables are reported on a HEALTHY cluster node too, and it still says role=cluster
  K12 the misspelt `HYDRA_REDIS_MODE` is validated on the single-node default as well

Run:
  export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
  export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
  python3 integration/test_startup_knobs.py
Exit 0 pass · 1 an assertion failed · 2 could not verify (no binary / DB / Redis).
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

from _redis_harness import RedisSetupError, endpoint as redis_endpoint, flush_db, require_url as require_redis_url
from _shared_db import SetupError, isolated
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "startup-knobs-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN_TOKEN = "hydra-startup-knobs-admin-2026"
MASTER_KEY = base64.b64encode(b"S" * 32).decode()

ADMIN, DATA = 18910, 18911
REDIS_DB = int(os.environ.get("HYDRA_STARTUP_KNOBS_REDIS_DB", "52"))

failures = []
TEST_DB = None


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def http(url, token=None, timeout=3):
    headers = {} if token is None else {"Authorization": f"Bearer {token}"}
    try:
        with urllib.request.urlopen(urllib.request.Request(url, headers=headers), timeout=timeout) as r:
            return r.status
    except urllib.error.HTTPError as e:
        return e.code
    except Exception:
        return 0


def port_is_free(port):
    with socket.socket() as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind(("127.0.0.1", port))
            return True
        except OSError:
            return False


def base_env():
    """The environment the binary needs to get as far as its own startup checks."""
    assert TEST_DB is not None, "base_env called before the test database was provisioned"
    env = {k: v for k, v in os.environ.items() if not k.startswith("HYDRA_")}
    env.update(usage_env())
    env.update(
        {
            "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN,
            "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
            "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
            "HYDRA_DB_URL": TEST_DB.url,
            "HYDRA_ENCRYPTION_KEY": MASTER_KEY,
            "RUST_LOG": "info",
        }
    )
    return env


def cluster_env():
    # A cluster node is exactly "a non-empty HYDRA_REDIS_URL". No peers, no token, no raft port.
    return {"HYDRA_REDIS_URL": f"{require_redis_url().rstrip('/')}/{REDIS_DB}", "HYDRA_REDIS_MODE": "single"}


# Live cluster-only names (the source's CLUSTER_ONLY_ENV minus the decision itself, Redis URL).
# Read from source for K4/K10 so a table that grows fails HERE rather than being silently skipped.
WIRING = {
    "HYDRA_REDIS_MODE": "single",
    "HYDRA_NODE_ID": "knobs-wiring",
    "HYDRA_BREAKER_QUORUM": "3",
    "HYDRA_CONFIG_POLL_MS": "500",
}
# Retired by the shared-DB rewrite: setting them does nothing and must be reported.
RETIRED = {
    "HYDRA_CLUSTER_PEERS": "n1=127.0.0.1:7001,n2=127.0.0.1:7002,n3=127.0.0.1:7003",
    "HYDRA_CLUSTER_ID": "knobs-drill-cluster",
    "HYDRA_ARACHNE_LISTEN": "127.0.0.1:18944",
    "HYDRA_ARACHNE_DATA_DIR": "/tmp/knobs-arachne",
}
# Values for K10, which must cover every CLUSTER_ONLY_ENV name except HYDRA_REDIS_URL.
KNOB_VALUES = dict(WIRING)


def source_table(name):
    """Read a `const NAME: [&str; N] = [..]` table from cluster/mod.rs."""
    src = open(os.path.join(ROOT, "crates/hydra-server/src/cluster/mod.rs"), encoding="utf-8").read()
    m = re.search(rf"const\s+{name}\s*:\s*\[&str;\s*(\d+)\]\s*=\s*\[(.*?)\];", src, re.S)
    if not m:
        raise AssertionError(f"{name} declaration not found — re-verify, then update this drill")
    names = re.findall(r'"([A-Z0-9_]+)"', m.group(2))
    if len(names) != int(m.group(1)) or not names:
        raise AssertionError(f"{name} parsed as {names} but declares {m.group(1)} entries")
    return names, int(m.group(1))


def observe(label, extra, budget=9.0):
    """Start a node and watch it: boot, answer, or die. Returns (state, rc, log)."""
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
            if (
                http(f"http://127.0.0.1:{ADMIN}/healthz") == 200
                or http(f"http://127.0.0.1:{ADMIN}/api/v1/health", token=ADMIN_TOKEN) == 200
            ):
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
        return state, proc.returncode, fh.read()


def line_about(log, needle):
    for raw in log.splitlines():
        if needle in raw:
            return raw
    return ""


def main():
    global TEST_DB
    if not os.path.exists(BIN):
        print(f"[startup-knobs] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    try:
        redis_url = require_redis_url()
        host, port = redis_endpoint(redis_url)
        note = flush_db(REDIS_DB, redis_url)
    except RedisSetupError as e:
        print(f"[startup-knobs] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2
    try:
        TEST_DB = isolated("startup_knobs")
    except SetupError as e:
        print(f"[startup-knobs] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2

    busy = [p for p in (ADMIN, DATA) if not port_is_free(p)]
    if busy:
        print(
            f"[startup-knobs] CANNOT VERIFY: port(s) {busy} are already in use — every "
            f"\"state=up\" leg would be satisfied by whatever answers there",
            file=sys.stderr,
        )
        TEST_DB.cleanup()
        return 2
    print(f"   ....  isolation: {note}; db={TEST_DB}; ports {ADMIN}/{DATA} free")

    try:
        live_legs()
        single_node_legs()
    finally:
        TEST_DB.cleanup()
        shutil.rmtree(DIR, ignore_errors=True)

    print()
    if failures:
        print(f"STARTUP KNOBS: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print(
        "STARTUP KNOBS: PASSED (fail-fast mode validation, named dropped/retired wiring, "
        "silence control, and the Redis-URL cluster switch — no raft/leader checks)"
    )
    return 0


def live_legs():
    """Legs on a node whose HYDRA_REDIS_URL is set (the whole definition of a cluster node)."""
    for kid, lab, mode in (
        ("K1", "k1_redis_mode_clustr", "clustr"),
        ("K2", "k2_redis_mode_sentinel", "sentinel"),
    ):
        state, rc, log = observe(lab, {**cluster_env(), "HYDRA_REDIS_MODE": mode})
        named = f"unsupported HYDRA_REDIS_MODE '{mode}'" in log
        check(
            f"{kid}: HYDRA_REDIS_MODE={mode} on a cluster node refuses to start and names the "
            f"knob AND the value",
            state == "exited" and rc not in (0, None) and named,
            f"state={state} exit={rc} src={line_about(log, 'HYDRA_REDIS_MODE')[:150] or '<silent>'}",
        )

    state, rc, log = observe("k3_redis_mode_single_control", cluster_env())
    check(
        "K3: control — the same node with the supported mode boots as a CLUSTER node",
        state in ("up", "alive") and "role=cluster" in log and "fatal startup error" not in log,
        f"state={state} exit={rc}",
    )

    # K11: retirement applies to a healthy cluster node too (the setting does nothing either way).
    state, rc, log = observe("k11_retired_on_a_cluster_node", {**cluster_env(), **RETIRED})
    line = line_about(log, "were retired by the shared-DB architecture")
    check(
        "K11: retired variables are reported even on a healthy cluster node, and it still says "
        "role=cluster",
        state in ("up", "alive")
        and bool(line)
        and all(name in line for name in RETIRED)
        and "role=cluster" in log,
        f"state={state} exit={rc} src={line[:220] or '<SILENT: nothing named the retirement>'}",
    )


def single_node_legs():
    """Legs on the documented single-node default (no HYDRA_REDIS_URL)."""
    names, declared = source_table("CLUSTER_ONLY_ENV")
    expected_wiring = sorted(n for n in names if n != "HYDRA_REDIS_URL")
    check(
        "K4: this drill configures every live cluster-only name the source declares except the "
        "Redis URL (a table that grew must fail HERE, not be silently skipped)",
        sorted(WIRING) == expected_wiring and len(names) == declared,
        f"drill={sorted(WIRING)} source-minus-redis={expected_wiring} parsed={len(names)}/{declared}",
    )

    state, rc, log = observe("k4_wiring_without_redis", WIRING)
    error_line = line_about(log, "cluster data-plane wiring is configured but HYDRA_REDIS_URL is not set")
    check(
        "K4: with live cluster wiring configured and no HYDRA_REDIS_URL the node still serves, "
        "but reports EVERY variable it ignores",
        state in ("up", "alive")
        and bool(error_line)
        and all(n in error_line for n in WIRING),
        f"state={state} exit={rc} src={error_line[:240] or '<SILENT: no line named the wiring>'}",
    )

    state, rc, log = observe("k5_wiring_and_retired_together", {**WIRING, **RETIRED})
    both = [
        l
        for l in log.splitlines()
        if "cluster data-plane wiring is configured but" in l
        and "were retired by the shared-DB architecture" in l
    ]
    check(
        "K5: when retired variables are set at the same time, the SAME line names BOTH groups "
        "(a node mid-migration must hear both mistakes)",
        state in ("up", "alive")
        and len(both) == 1
        and all(n in both[0] for n in WIRING)
        and all(name in both[0] for name in RETIRED),
        f"state={state} exit={rc} lines={len(both)} "
        f"src={(both[0] if both else line_about(log, 'dropped'))[:260]}",
    )

    state, rc, log = observe("k6_retired_alone", RETIRED)
    line = line_about(log, "were retired by the shared-DB architecture")
    check(
        "K6: retired variables ALONE are reported, and the node is NOT called standalone",
        state in ("up", "alive")
        and bool(line)
        and all(name in line for name in RETIRED)
        and "cluster data-plane wiring is configured but" not in line,
        f"state={state} exit={rc} src={line[:220] or '<SILENT: nothing named the retirement>'}",
    )

    state, rc, log = observe("k7_default_silent", {})
    noisy = [
        l
        for l in log.splitlines()
        if "cluster data-plane wiring" in l
        or "were retired" in l
        or "HYDRA_CLUSTER_PEERS" in l
    ]
    check(
        "K7: control — with nothing cluster-shaped the node boots with no complaint at all",
        state in ("up", "alive") and not noisy,
        f"state={state} exit={rc} noise={noisy[:1]}",
    )

    state, rc, log = observe("k8_retired_member_list_alone", {"HYDRA_CLUSTER_PEERS": RETIRED["HYDRA_CLUSTER_PEERS"]})
    line = line_about(log, "were retired by the shared-DB architecture")
    check(
        "K8: HYDRA_CLUSTER_PEERS alone no longer makes a cluster — the node serves as a single "
        "node and reports the retirement",
        state in ("up", "alive")
        and bool(line)
        and "HYDRA_CLUSTER_PEERS" in line
        and "role=cluster" not in log
        and "fatal startup error" not in log,
        f"state={state} exit={rc} src={line[:220] or '<SILENT: nothing named the retirement>'}",
    )

    state, rc, log = observe("k9_non_route_strategy", {"HYDRA_NON_ROUTE_STRATEGY": "rejekt"})
    check(
        "K9: an unrecognised HYDRA_NON_ROUTE_STRATEGY refuses to start and quotes the value",
        state == "exited"
        and rc not in (0, None)
        and '"rejekt"' in log
        and "HYDRA_NON_ROUTE_STRATEGY" in log,
        f"state={state} exit={rc} src={line_about(log, 'HYDRA_NON_ROUTE_STRATEGY')[:150] or '<silent>'}",
    )

    unvalued = sorted(n for n in names if n not in KNOB_VALUES)
    if unvalued != ["HYDRA_REDIS_URL"]:
        check(
            f"K10: this drill can supply a value for every live cluster-only variable except the "
            f"Redis URL (declared {names})",
            False,
            f"no-value-for={unvalued or 'none'} — the table grew and this drill did not keep up",
        )
    else:
        configured = {n: KNOB_VALUES[n] for n in names if n in KNOB_VALUES}
        state, rc, log = observe("k10_every_cluster_only_knob", configured)
        line = line_about(log, "cluster data-plane wiring is configured but")
        missing = sorted(name for name in configured if name not in line)
        check(
            f"K10: with every live cluster-only variable except HYDRA_REDIS_URL configured "
            f"({len(configured)} of {len(names)}), the ERROR names every one of them",
            state in ("up", "alive") and not missing and len(names) == declared,
            f"state={state} exit={rc} parsed={len(names)}/{declared} missing={missing or 'none'} "
            f"src={line[:360] or '<no line>'}",
        )

    state, rc, log = observe("k12_redis_mode_typo_single_node", {"HYDRA_REDIS_MODE": "clustr"})
    rejected = "unsupported HYDRA_REDIS_MODE" in log
    check(
        "K12: on the single-node default the misspelt HYDRA_REDIS_MODE IS validated — the node "
        "refuses to start and names the knob AND the value",
        state not in ("up", "alive") and rejected and "fatal startup error" in log,
        f"state={state} exit={rc} rejected={rejected}",
    )

    declared_retired, retired_len = source_table("RETIRED_CLUSTER_ENV")
    unknown = sorted(name for name in RETIRED if name not in declared_retired)
    check(
        "K11/K6: every retired name this drill sets is actually in RETIRED_CLUSTER_ENV",
        not unknown and len(declared_retired) == retired_len,
        f"unknown={unknown or 'none'} parsed={len(declared_retired)}/{retired_len}",
    )


if __name__ == "__main__":
    sys.exit(main())
