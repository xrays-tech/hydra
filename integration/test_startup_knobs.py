#!/usr/bin/env python3
"""Startup knobs: refuse, or say out loud that the setting is being dropped.

Two operator-facing promises about `HYDRA_ROLE` / `HYDRA_REDIS_MODE` were made in prose and
checked only by hand — round 191 measured the Redis one, round 192 the role one — and a
hand measurement is not an executable contract:

  * `ops.md` §13.3 / `cluster.md` §2: an unsupported `HYDRA_REDIS_MODE` **fails fast at
    startup** ("a typo must not silently mean `single`") — **on a cluster-role node**. That
    qualifier is not decoration and it is measured by K12: the mode is read only inside
    `if role.is_cluster()` (`main.rs`), so on the documented single-node default a misspelt value is
    not validated at all (round 194: the unqualified wording here, in the CI step and in four doc
    lines claimed a fail-fast that does not exist on the default deployment);
  * `jiqun-deploy.md`: a typo'd `HYDRA_ROLE` "WARNs and falls back to single-node — it will
    not silently disable the proxy, but it DOES leave the cluster, so check it".

Every leg below observes the REAL binary (process exit code + log text), never a helper:

  K1  `HYDRA_REDIS_MODE=clustr` on a cluster node (`HYDRA_ROLE=edge`) ⇒ non-zero exit, and the
      message names the knob AND the misspelt value the operator typed. Before round 191 this node BOOTED as
      `single` — a topology switch silently ignored.
  K2  ...and so does `HYDRA_REDIS_MODE=sentinel` — through the parse catch-all, NOT through the
      `RedisMode::Sentinel` arm in `redis/mod.rs`: `parse` refuses the value before any mode is
      constructed, so no environment path can reach that arm (it stays reachable only for a direct
      caller of the public API, which is why the arm still exists). Round 194's label implied this leg
      pinned that arm; round 195 narrowed the wording.
  K3  control: the same node with `single` really boots, so K1/K2 are not "cluster mode is
      broken for some other reason".
  K4  a typo'd role WITH cluster wiring keeps proxying but reports every variable it drops
      (this is the diagnostic that already existed).
  K5  the role UNSET with cluster wiring reports the same thing. Round 192: this path was
      completely SILENT — the one path where the operator most likely just forgot the
      variable, and the wiring was configured in full.
  K6  `HYDRA_ROLE=all` + wiring is reported as "not a cluster role", never as "unknown":
      `all` is a documented value (`lib.rs`, `cluster.md`, `jiqun-deploy.md`), and calling it
      unknown was a false claim.
  K7  control: `all` (and unset) with NO wiring stays silent — the documented single-node
      default. A rule that cannot stay quiet on correct input is noise, not a guard.
  K8  `" edge "` (surrounding whitespace) is still the edge role: it boots as an edge, and
      does NOT report dropped wiring. Without whitespace folding a stray space in a manifest
      silently took the node out of the cluster.
  K9  `HYDRA_NON_ROUTE_STRATEGY` refuses an unrecognised strategy by name. That one already
      had a unit test; this pins it where an operator actually sees it.
  K10 all TEN cluster-only variables configured ⇒ the fallback ERROR names every one of them.
      Round 193 measured the defect: the list named three and dropped the other seven silently.

Run: python3 integration/test_startup_knobs.py        # needs target/debug/hydra + a Redis
     HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380 python3 integration/test_startup_knobs.py
Exit 0 pass · 1 an assertion failed · 2 could not verify (no binary / no Redis).
"""
import base64
import os
import re
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "startup-knobs-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN_TOKEN = "hydra-startup-knobs-admin-2026"
CLUSTER_TOKEN = "hydra-startup-knobs-cluster-2026"
# 32 raw bytes, base64: the inline form of the master key (see ops.md §1.2).
MASTER_KEY = base64.b64encode(b"S" * 32).decode()

# Ports clear of the dev stack (8080-8084/8090) and of the other suites (188xx is taken by
# the clickhouse/usage drills). One pair, reused leg by leg: the gate runs drills serially
# and every node is killed before the next leg starts.
ADMIN, DATA = 18910, 18911
DEAD_CH = "http://127.0.0.1:18999"          # nothing listens: cluster roles need a CH URL
REDIS_BASE = os.environ.get("HYDRA_TEST_REDIS_URL", "redis://127.0.0.1:6380").rstrip("/")
REDIS_DB = int(os.environ.get("HYDRA_STARTUP_KNOBS_REDIS_DB", "52"))

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

    Every inherited `HYDRA_*` is dropped first: a developer shell with `HYDRA_ROLE` exported
    would otherwise silently rewrite the very legs under test (K7 asserts SILENCE, and an
    inherited variable would break it for the wrong reason).
    """
    env = {k: v for k, v in os.environ.items() if not k.startswith("HYDRA_")}
    env.update({
        "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN,
        "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'knobs.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": MASTER_KEY,
        "RUST_LOG": "info",
    })
    return env


# Cluster roles pass the same env contract the shipped compose uses: a Redis backbone, the
# shared cluster token, a control endpoint, and a ClickHouse sink (mandatory in cluster mode).
CLUSTER_ENV = {
    "HYDRA_REDIS_URL": redis_url(),
    "HYDRA_REDIS_MODE": "single",
    "HYDRA_CLUSTER_TOKEN": CLUSTER_TOKEN,
    "HYDRA_CONTROL_URL": f"http://127.0.0.1:{ADMIN}",
    "HYDRA_PUBLIC_URL": f"http://127.0.0.1:{ADMIN}",
    "HYDRA_NODE_ID": "knobs-probe",
    "HYDRA_USAGE_SINK": "clickhouse",
    "HYDRA_CLICKHOUSE_URL": DEAD_CH,
}
# The three variables the "you configured cluster wiring but this node is not a cluster
# node" diagnostic must name (cluster/mod.rs::ignored_cluster_wiring). Kept small on purpose for
# K4-K6, which are about WHICH PATH reaches the diagnosis; the completeness of the list itself is
# K10 below.
WIRING = {k: CLUSTER_ENV[k] for k in ("HYDRA_REDIS_URL", "HYDRA_CLUSTER_TOKEN", "HYDRA_CONTROL_URL")}
# A value this drill can set each cluster-only variable to. The NAMES are not copied here — they are
# read from `CLUSTER_ONLY_ENV` in the source (see `cluster_only_names`), so this leg grows with the
# table instead of with a hand-maintained list (round 194: the list used to be copied by hand, which
# meant the wire leg could never notice a NEW table entry).
KNOB_VALUES = {
    "HYDRA_REDIS_URL": redis_url,
    "HYDRA_REDIS_MODE": lambda: "single",
    "HYDRA_CLUSTER_TOKEN": lambda: CLUSTER_TOKEN,
    "HYDRA_CONTROL_URL": lambda: f"http://127.0.0.1:{ADMIN}",
    "HYDRA_PUBLIC_URL": lambda: f"http://127.0.0.1:{ADMIN}",
    "HYDRA_NODE_ID": lambda: "knobs-all",
    "HYDRA_CONTROL_POLL_MS": lambda: "1000",
    "HYDRA_LEADER_LEASE_MS": lambda: "3000",
    "HYDRA_REGISTRY_STALE_GRACE_SECS": lambda: "30",
    "HYDRA_FORWARD_TIMEOUT_SECS": lambda: "5",
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



def observe(label, extra, budget=9.0):
    """Start a node and watch it: boot, answer, or die.

    Returns `(state, rc, log)` where state is "up" (an HTTP health endpoint answered 200),
    "alive" (still running when the budget expired) or "exited" (it refused to start, `rc`).
    The distinction matters: "did not exit" is the honest evidence for a node that must keep
    serving, while a refusal leg needs the EXIT CODE — a process that is merely slow to fail
    would otherwise pass.
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
            # `/healthz` is token-free but only routed for edges; `/api/v1/health` is the
            # admin-routed one. Whichever answers 200 means the node is serving.
            if http(f"http://127.0.0.1:{ADMIN}/healthz") == 200 or \
               http(f"http://127.0.0.1:{ADMIN}/api/v1/health", token=ADMIN_TOKEN) == 200:
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


def line_about(log, needle):
    """The log line carrying `needle` (tracing writes the fields AFTER the message)."""
    for raw in log.splitlines():
        if needle in raw:
            return raw
    return ""


def main():
    if not os.path.exists(BIN):
        print(f"[startup-knobs] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    os.makedirs(DIR, exist_ok=True)
    if not redis_reachable():
        print(f"[startup-knobs] CANNOT VERIFY: no Redis at {REDIS_BASE} "
              f"(HYDRA_TEST_REDIS_URL)", file=sys.stderr)
        return 2
    # A foreign listener on our ports would make every "it booted" leg wrongly green (round 194): the
    # legs judge boot by an HTTP 200 on ADMIN, not by the process being ours. Refuse to run instead.
    busy = [p for p in (ADMIN, DATA) if not port_is_free(p)]
    if busy:
        print(f"[startup-knobs] CANNOT VERIFY: port(s) {busy} are already in use — every "
              f"\"state=up\" leg would be satisfied by whatever answers there, not by the gateway",
              file=sys.stderr)
        return 2
    print(f"   ....  isolation: {flush_redis_db()}; ports {ADMIN}/{DATA} free")

    # ---- K1/K2/K3: the Redis topology switch ------------------------------------------
    # A misspelt mode must not boot. `clustr` is the measured round-191 case; `sentinel` is
    # the half the docs always claimed.
    for kid, lab, mode in (("K1", "k1_redis_mode_clustr", "clustr"),
                           ("K2", "k2_redis_mode_sentinel", "sentinel")):
        state, rc, log = observe(lab, {**CLUSTER_ENV, "HYDRA_ROLE": "edge",
                                      "HYDRA_REDIS_MODE": mode})
        named = f"unsupported HYDRA_REDIS_MODE '{mode}'" in log
        check(f"{kid}: HYDRA_REDIS_MODE={mode} refuses to start and names the knob AND the value",
              state == "exited" and rc not in (0, None) and named,
              f"state={state} exit={rc} src={line_about(log, 'HYDRA_REDIS_MODE')[:150] or '<silent>'}")

    state, rc, log = observe("k3_redis_mode_single_control",
                             {**CLUSTER_ENV, "HYDRA_ROLE": "edge"})
    check("K3: control — with the supported mode the same edge node boots (K1/K2 are about "
          "the knob, not about cluster mode being broken)",
          state in ("up", "alive") and "starting role=edge" in log
          and "fatal startup error" not in log,
          f"state={state} exit={rc}")

    # ---- K4/K5/K6: cluster wiring that this node is not going to use -------------------
    # The consequence is identical in all three legs (no registry, no lease, no L2 cache,
    # tenant writes to the LOCAL database), so the diagnosis must be identical too.
    wiring_cases = [
        ("K4", "k4_role_typo_with_wiring", {"HYDRA_ROLE": "ledge"}, 'HYDRA_ROLE="ledge"'),
        ("K5", "k5_role_unset_with_wiring", {}, "HYDRA_ROLE is not set"),
        ("K6", "k6_role_all_with_wiring", {"HYDRA_ROLE": "all"}, 'HYDRA_ROLE="all"'),
        # A blank value is its own documented case (`ops.md`: "unset or blank") with its own `why`
        # text, and it had no wire leg until round 194 (the unit test covered it, the binary did not).
        ("K11", "k11_role_blank_with_wiring", {"HYDRA_ROLE": ""}, "HYDRA_ROLE is blank"),
    ]
    for kid, lab, extra, why in wiring_cases:
        state, rc, log = observe(lab, {**WIRING, **extra})
        error_line = line_about(log, "cluster wiring is configured but")
        names_all = error_line and all(v in error_line for v in WIRING)
        check(f"{kid}: with cluster wiring configured a non-cluster role still serves, but "
              f"reports EVERY variable it will ignore",
              state in ("up", "alive") and bool(error_line) and names_all
              and sum(1 for l in log.splitlines() if why in l) >= 1,
              f"state={state} exit={rc} src={error_line[:150] or '<SILENT: no line named the wiring>'}")
        if kid == "K6":
            # `all` is a documented value (`lib.rs`, `cluster.md`, `jiqun-deploy.md`): the
            # wiring error must not call it unknown. Round 192, measured before the fix:
            # `HYDRA_ROLE=all` + wiring logged "unknown HYDRA_ROLE" with role="all".
            check("K6: ...and `all` is reported as a documented non-cluster role, never as an "
                  "unknown one",
                  "not a cluster role" in error_line and "unknown" not in error_line
                  and "known role" not in error_line,
                  error_line[:150] or "<no line>")

    # ---- K7: the silence control -------------------------------------------------------
    for lab, extra in (("k7a_default_silent", {}), ("k7b_role_all_silent", {"HYDRA_ROLE": "all"})):
        state, rc, log = observe(lab, extra)
        noisy = [l for l in log.splitlines() if "HYDRA_ROLE" in l or "cluster wiring" in l]
        check(f"K7: control — {lab.split('_', 1)[1].replace('_', ' ')} boots with no role/wiring "
              f"complaint (the documented single-node default is not a warning)",
              state in ("up", "alive") and not noisy,
              f"state={state} exit={rc} noise={noisy[:1]}")

    # ---- K8: whitespace in a manifest value is not a different role ---------------------
    state, rc, log = observe("k8_role_whitespace_edge",
                             {**CLUSTER_ENV, "HYDRA_ROLE": " edge "})
    check("K8: `HYDRA_ROLE=\" edge \"` boots AS AN EDGE (it does not fall back and does not "
          "claim to be dropping its wiring)",
          state in ("up", "alive") and "starting role=edge" in log
          and "cluster wiring is configured but" not in log,
          f"state={state} exit={rc}")

    # ---- K10: the list of dropped settings must be COMPLETE ------------------------------
    # `CLUSTER_ONLY_ENV` is the single owner of "what counts as cluster wiring". Round 193 measured
    # the defect this pins: the diagnostic named three variables while seven more were configured,
    # dropped, and never mentioned (`HYDRA_NODE_ID`, `HYDRA_CONTROL_POLL_MS`, `HYDRA_REDIS_MODE`,
    # `HYDRA_PUBLIC_URL`, `HYDRA_LEADER_LEASE_MS`, `HYDRA_REGISTRY_STALE_GRACE_SECS`,
    # `HYDRA_FORWARD_TIMEOUT_SECS`) — an operator reading "these three are ignored" reasonably
    # concludes the others took effect.
    names, declared = cluster_only_names()
    unvalued = sorted(n for n in names if n not in KNOB_VALUES)
    configured = {n: KNOB_VALUES[n]() for n in names if n in KNOB_VALUES}
    state, rc, log = observe("k10_every_cluster_only_knob", {"HYDRA_ROLE": "all", **configured})
    line = line_about(log, "cluster wiring is configured but")
    missing = sorted(name for name in configured if name not in line)
    check(f"K10: with every cluster-only variable the source declares configured ({len(configured)} "
          f"of {len(names)}), the fallback ERROR names every one of them (none is dropped in silence)",
          state in ("up", "alive") and not missing and not unvalued and len(names) == declared,
          f"state={state} exit={rc} parsed={len(names)}/{declared} missing={missing or 'none'} "
          f"no-value-for={unvalued or 'none (the table grew and this drill kept up)'} "
          f"src={line[:400] or '<no line>'}")

    # ---- K12: the BOUNDARY of K1/K2 — measured, not assumed ------------------------------
    # Oracle review, round 194: `HYDRA_REDIS_MODE` is read ONLY inside the cluster-role branch
    # (`main.rs`: `if role.is_cluster() { … RedisMode::from_env()? … }`), so on the documented
    # single-node default a misspelt topology switch is neither validated nor mentioned. The docs
    # (ops.md §13.3/§13.7, cluster.md §2, jiqun-deploy.md) and the K1/K2 labels used to state the
    # fail-fast WITHOUT that qualifier — a claim that is false on the default deployment and that a
    # green drill would have endorsed. This leg pins the truth: it boots, and the switch is never
    # mentioned (so an operator gets no signal at all).
    state, rc, log = observe("k12_redis_mode_typo_single_node", {"HYDRA_REDIS_MODE": "clustr"})
    rejected = "unsupported HYDRA_REDIS_MODE" in log
    # Measured, and more precise than "never mentioned": the knob IS in `CLUSTER_ONLY_ENV` (round 193),
    # so setting it alone also fires the "cluster wiring is configured but …" ERROR — which names the
    # VARIABLE (`ignored=HYDRA_REDIS_MODE`) and never the misspelt VALUE. That is the whole boundary:
    # on the single-node default the value is not validated, and the operator learns nothing about it
    # being wrong.
    wiring_line = line_about(log, "ignored=HYDRA_REDIS_MODE")
    check("K12: on the single-node default (HYDRA_ROLE unset) the misspelt HYDRA_REDIS_MODE is NOT "
          "validated — the node serves, no line rejects the value, and at most the wiring ERROR names "
          "the variable without the value",
          state in ("up", "alive") and not rejected and "fatal startup error" not in log
          and "clustr" not in wiring_line,
          f"state={state} exit={rc} rejected={rejected} wiring-names-value={'clustr' in wiring_line}")

    # ---- K9: the sibling fail-fast that never had an executable contract ----------------
    state, rc, log = observe("k9_non_route_strategy", {"HYDRA_NON_ROUTE_STRATEGY": "rejekt"})
    check("K9: an unrecognised HYDRA_NON_ROUTE_STRATEGY refuses to start and quotes the value "
          "(a safety control must not silently become `passthrough`)",
          state == "exited" and rc not in (0, None) and '"rejekt"' in log
          and "HYDRA_NON_ROUTE_STRATEGY" in log,
          f"state={state} exit={rc} src={line_about(log, 'HYDRA_NON_ROUTE_STRATEGY')[:150] or '<silent>'}")

    print()
    if failures:
        print(f"STARTUP KNOBS: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("STARTUP KNOBS: PASSED (refusal + named-value + ignored-wiring + silence control)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
