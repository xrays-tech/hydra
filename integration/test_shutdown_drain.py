#!/usr/bin/env python3
"""Graceful shutdown: does an in-flight request survive `SIGTERM`, and is the drain the bound?

`ops.md` §13.5b documents this as the Kubernetes rolling-update contract: "`HYDRA_SHUTDOWN_DRAIN_SECS`
(default **20**) is how long Pingora may spend draining in-flight requests after `SIGTERM`. It maps
to Pingora's `grace_period_seconds`", with the arithmetic for `terminationGracePeriodSeconds`, and
"Which signals flush the usage sink: `SIGTERM`, `SIGINT` **and `SIGQUIT`**" — the flush needs an
explicit hook because Pingora's shutdown path ends in `process::exit(0)`, which runs no destructors.

All of that is documented, and all of it is only guarded STATICALLY (`check_compose_grace.cjs`
compares `stop_grace_period` against the drain budget; `main.rs` has a unit test that the drain is
set explicitly). Nothing had ever sent a real `SIGTERM` to a node with a request in flight. Cases:

  N0  control: the slow upstream works (a 2 s generation)
  N1  DRAIN: a request is in flight when `SIGTERM` arrives — it must still receive a COMPLETE 200,
      and the process must stay alive until it does, then exit 0
  N2  BOUND: with `HYDRA_SHUTDOWN_DRAIN_SECS=1` and a request that needs 3 s, the client is cut at
      about the drain budget — i.e. the value is a real bound, not decoration
  N3  FLUSH: a request whose usage row is still in the sink's batch when `SIGTERM` arrives must
      have that row PERSISTED after the process is gone (the reason the hook exists)
  N4  READINESS during the drain: what `/readyz` answers while in-flight work is being drained —
      an orchestrator's readiness probe is the only application-level lever here, so the measured
      answer belongs in the docs either way

Run: python3 integration/test_shutdown_drain.py    # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import re
import shutil
import signal
import sqlite3
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "shutdown-drain-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18810, 18811, 18819
ADMIN2, DATA2 = ADMIN + 10, DATA + 10
TOKEN = "hydra-drain-admin-2026"
DOMAIN = "drain.local"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Upstream(BaseHTTPRequestHandler):
    """Answers after `slowN` seconds, where N comes from the request's model name."""

    protocol_version = "HTTP/1.1"

    def do_GET(self):
        payload = b'{"object":"list","data":[]}'
        self.send_response(200)
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        if self.path.startswith("/auth"):
            payload = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            m = re.search(rb'"model"\s*:\s*"slow(\d+)"', body)
            delay = int(m.group(1)) if m else 0
            if delay:
                time.sleep(delay)
            payload = json.dumps({"id": "chatcmpl-drain", "object": "chat.completion",
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


def call(method, url, token=None, body=None, timeout=30, host=None):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    if host:
        headers["Host"] = host
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    started = time.time()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace"), time.time() - started
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace"), time.time() - started
    except Exception as e:
        return 0, str(e), time.time() - started


def admin(method, path, body=None, port=ADMIN):
    st, out, _ = call(method, f"http://127.0.0.1:{port}/api/v1{path}", token=TOKEN, body=body)
    return st, out


def proxied(model="slow2", port=DATA, timeout=40):
    st, body, elapsed = call("POST", f"http://127.0.0.1:{port}/v1/chat/completions",
                             token="sk-tenant-1", host=DOMAIN, timeout=timeout,
                             body={"model": model, "messages": [{"role": "user", "content": "hi"}]})
    return st, body, elapsed


def start_node(label, extra, admin_port=ADMIN, data_port=DATA):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin_port}",
        "HYDRA_LISTEN": f"127.0.0.1:{data_port}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "info",
    })
    env.update(extra)
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(port=ADMIN, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health", port=port)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def seed(admin_port=ADMIN, models=("slow2", "slow3")):
    rows = [("providers", {"id": "p1", "key": "p1", "name": "P",
                           "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                           "created_at": "", "updated_at": ""}),
            ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                               "created_at": ""}),
            ("tenants", {"id": "t1", "name": "T", "domain": DOMAIN,
                         "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                         "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
            ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                                  "created_at": "", "updated_at": ""})]
    for i, model in enumerate(models):
        rows.append(("provider-models", {"id": f"pm{i}", "key": model, "name": model,
                                         "provider_id": "p1", "status": 1,
                                         "created_at": "", "updated_at": ""}))
        rows.append(("tenant-models", {"id": f"tm{i}", "tenant_id": "t1", "model_key": model,
                                       "created_at": "", "updated_at": ""}))
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body, port=admin_port)
        if st not in (200, 201):
            raise SystemExit(f"[drain] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {}, port=admin_port)
    time.sleep(0.3)


def usage_rows(db_path):
    """Rows in the SQLite usage table, or None when the file/table is absent."""
    try:
        conn = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        try:
            return conn.execute("SELECT COUNT(*) FROM usage_record").fetchone()[0]
        finally:
            conn.close()
    except Exception as e:
        return f"<{type(e).__name__}: {e}>"


def main():
    if not os.path.exists(BIN):
        print(f"[drain] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    # ---- N0/N1/N4: the drain with a default-ish budget -------------------------
    node = start_node("drain", {"HYDRA_SHUTDOWN_DRAIN_SECS": "10"})
    try:
        if not wait_healthy():
            print("[drain] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "drain.log"), errors="replace").read()[-700:],
                  file=sys.stderr)
            return 2
        seed()
        st0, _, el0 = proxied("slow2")
        check("N0: the slow upstream answers through the gateway (control)",
              st0 == 200 and el0 >= 1.5, f"HTTP {st0} in {el0:.1f}s")

        # In-flight request, then SIGTERM in the middle of it.
        result = {}

        def inflight():
            result["r"] = proxied("slow2")

        t = threading.Thread(target=inflight, daemon=True)
        t.start()
        time.sleep(0.5)                      # the upstream is now sleeping ~1.5s more
        t_term = time.time()
        node.send_signal(signal.SIGTERM)
        # N4: what the orchestrator's probes AND the data plane do DURING the drain. Polled
        # rather than sampled once, because "the admin port is gone 0.4s later" needs a shape:
        # when exactly does each listener stop answering, and does the DATA plane still serve?
        probe_trace = []
        data_trace = []
        for _ in range(20):                      # ~4s at 0.2s
            hz = call("GET", f"http://127.0.0.1:{ADMIN}/healthz", token=TOKEN)[0]
            rz = call("GET", f"http://127.0.0.1:{ADMIN}/readyz", token=TOKEN)[0]
            met = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)[0]
            dp = proxied("slow2", timeout=10)[0]
            probe_trace.append((round(time.time() - t_term, 1), hz, rz, met))
            data_trace.append((round(time.time() - t_term, 1), dp))
            if hz == 0 and rz == 0 and met == 0 and dp == 0:
                break
            time.sleep(0.2)
        alive_right_after = node.poll() is None
        hz, rz, met = probe_trace[0][1], probe_trace[0][2], probe_trace[0][3]
        # Keep probing for the REST of the drain (round 173). The loop above `break`s as soon as all
        # four probes are refused — measured at t=0.2s — and the N4 legs below then claimed "during
        # the drain there is no probe and no scrape at all" from those two samples while the process
        # went on living ~15s longer: a claim about the whole window, evidenced by one sample in its
        # first 200 ms (measured trace before this change: [(0.0, 404, 0, 0), (0.2, 0, 0, 0)]). If the
        # admin listener came back up mid-drain — the regression direction this leg exists for —
        # nothing would have looked. This phase samples until the process is GONE, and the N4
        # PREMISE below requires the sample set to actually cover the window.
        drain_deadline = time.time() + 40
        while node.poll() is None and time.time() < drain_deadline:
            hz_d = call("GET", f"http://127.0.0.1:{ADMIN}/healthz", token=TOKEN)[0]
            rz_d = call("GET", f"http://127.0.0.1:{ADMIN}/readyz", token=TOKEN)[0]
            met_d = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)[0]
            dp_d = proxied("slow2", timeout=10)[0]
            probe_trace.append((round(time.time() - t_term, 1), hz_d, rz_d, met_d))
            data_trace.append((round(time.time() - t_term, 1), dp_d))
            time.sleep(0.2)
        body_rz = ""
        t.join(timeout=30)
        rc = node.wait(timeout=30)
        t_exit = time.time()
        st1, body1, el1 = result.get("r", (0, "<no result>", 0.0))
        announce("N1 the in-flight request", f"HTTP {st1} in {el1:.1f}s; SIGTERM at +0.5s; "
                 f"alive 0.4s after SIGTERM={alive_right_after}; exit={rc} "
                 f"{t_exit - t_term:.1f}s after SIGTERM")
        check("N1: a request in flight when SIGTERM arrives still gets a COMPLETE 200 "
              "(the rolling-update promise: the drain is not a drop)",
              st1 == 200 and "chatcmpl-drain" in body1, f"HTTP {st1} {body1[:70]}")
        check("N1: the process does NOT exit before that request finished",
              alive_right_after and t_exit - t_term >= 1.0,
              f"alive={alive_right_after} exit {t_exit - t_term:.1f}s after the signal")
        check("N1: ...and it exits with code 0 once the drain is done",
              rc == 0, f"exit={rc}")
        check("N1: the exit waits out the whole drain budget (10s) plus the 5s final step — "
              "measured, and exactly what ops.md §13.5b's arithmetic tells an operator to size "
              "`terminationGracePeriodSeconds` from",
              14.0 <= t_exit - t_term <= 18.0,
              f"exit {t_exit - t_term:.1f}s after SIGTERM (drain 10s + 5s)")
        log = open(os.path.join(DIR, "drain.log"), errors="replace").read()
        check("N1: ...after flushing the usage sinks (documented hook)",
              "SIGTERM: flushing usage sinks" in log or "usage sinks flushed" in log,
              next((l for l in log.splitlines() if "flush" in l.lower()), "<no line>")[:130])
        announce("N4 the ADMIN port during the drain (t, /healthz, /readyz, /metrics)",
                 f"{probe_trace}")
        announce("N4 the DATA plane during the drain (t, status of a NEW request)",
                 f"{data_trace}")
        # Measured shape: every listener stops ACCEPTING almost immediately (at t=0 one probe
        # still got a 404 mid-teardown, then flat refusal), while the in-flight request above
        # runs to completion. So "draining" here means "finish what was already accepted",
        # NOT "keep accepting for the window" — the distinction an operator sizing
        # `terminationGracePeriodSeconds` needs.
        admin_after_1 = [t for t, a, b, c in probe_trace if t >= 0.2 and (a or b or c)]
        data_after_1 = [t for t, st in data_trace if t >= 0.2 and st != 0]
        # PREMISE, asserted (round 173): "no probe answered" is only evidence about the DRAIN if the
        # probes actually span it. Measured after the change: ~70 samples, the last within a second of
        # the exit; before it, 2 samples both inside the first 0.2s.
        n_after = len([t for t, _a, _b, _c in probe_trace if t >= 0.2])
        last_sample = probe_trace[-1][0] if probe_trace else -1.0
        drain_span = t_exit - t_term
        check("N4 PREMISE: the probe set really COVERS the drain — samples well after 0.2s and right "
              "up to the exit — so 'nothing answered' is a statement about the window and not about "
              "its first sample",
              n_after >= 5 and drain_span > 1.0 and last_sample >= drain_span - 1.0,
              f"samples at t>=0.2s: {n_after}; last sample at {last_sample}s; exit at "
              f"{drain_span:.1f}s; trace={probe_trace[:3]}…{probe_trace[-2:]}")
        check("N4: within ~0.2s of SIGTERM the ADMIN listener (health/ready/metrics) refuses "
              "every connection — during the drain there is no probe and no scrape at all, "
              "which is NOT what ops.md §9/§13.5b imply (a scrape gap of up to drain+5s)",
              not admin_after_1, f"still answering at {admin_after_1}")
        check("N4: ...and the DATA plane refuses NEW connections too: the drain finishes what "
              "was already accepted, it does not keep accepting for the window (so traffic that "
              "arrives before the orchestrator removes the endpoint is refused, not served)",
              not data_after_1, f"new requests answered at {data_after_1[:6]}")
    finally:
        if node.poll() is None:
            node.kill()

    # ---- N2: the drain budget is a real bound ---------------------------------
    node2 = start_node("short", {"HYDRA_SHUTDOWN_DRAIN_SECS": "1"}, ADMIN2, DATA2)
    try:
        if not wait_healthy(ADMIN2):
            check("N2: the second node became healthy", False, "never healthy")
        else:
            seed(ADMIN2)
            result2 = {}

            def inflight2():
                result2["r"] = proxied("slow3", port=DATA2, timeout=40)

            t2 = threading.Thread(target=inflight2, daemon=True)
            t2.start()
            time.sleep(0.5)
            t_term2 = time.time()
            node2.send_signal(signal.SIGTERM)
            t2.join(timeout=30)
            rc2 = node2.wait(timeout=30)
            took = time.time() - t_term2
            st2, body2, el2 = result2.get("r", (0, "<no result>", 0.0))
            announce("N2 the cut request", f"HTTP {st2} in {el2:.1f}s {body2[:60]!r}; "
                     f"exit={rc2} {took:.1f}s after SIGTERM (drain budget 1s)")
            check("N2: with a 1s drain a request needing 3s is CUT at about the budget — the "
                  "documented value is a real bound, not decoration",
                  el2 < 2.5 and st2 != 200, f"HTTP {st2} after {el2:.1f}s")
            # The documented arithmetic (§13.5b): drain + graceful_shutdown_timeout (5s, the
            # final runtime step, NOT the in-flight window). Measured here rather than assumed:
            # drain=1 exits at ~6s, drain=10 at ~15s.
            check("N2: ...and the process exits DRAIN + 5s, exactly the documented arithmetic "
                  "(the 5s is the final runtime-shutdown step, not the in-flight window)",
                  rc2 == 0 and 5.0 <= took <= 8.5, f"exit={rc2} after {took:.1f}s (drain 1s)")
    finally:
        if node2.poll() is None:
            node2.kill()

    # ---- N5: the hook's OWN observable — a sink that cannot flush at shutdown --
    # N3 alone does NOT discriminate the hook: measured by removing it, a row buffered at
    # SIGTERM still lands, because the periodic 5 s flush fires during the >=6 s exit. What the
    # hook owns is the SHUTDOWN-path attempt itself: with a sink that cannot flush (a dead
    # ClickHouse), the drain must run and must say so LOUDLY — the documented alternative is
    # "a shutdown signal that is not in that list discards the buffered batch silently (the
    # drop is not even counted)".
    node5 = start_node("deadch", {
        "HYDRA_SHUTDOWN_DRAIN_SECS": "2",
        "HYDRA_USAGE_SINK": "clickhouse",
        "HYDRA_CLICKHOUSE_URL": "http://127.0.0.1:18899",   # nothing listens: flush always fails
    }, ADMIN + 30, DATA + 30)
    try:
        if not wait_healthy(ADMIN + 30):
            check("N5: the ClickHouse-sink node became healthy", False, "never healthy")
        else:
            seed(ADMIN + 30, models=("slow2",))
            st5, _, _ = proxied("slow2", port=DATA + 30)
            node5.send_signal(signal.SIGTERM)
            rc5 = node5.wait(timeout=30)
            log5 = open(os.path.join(DIR, "deadch.log"), errors="replace").read()
            announce("N5 the dead-sink shutdown", f"request HTTP {st5}, exit={rc5}")
            check("N5: the shutdown drain RUNS even when the sink cannot flush — the hook is what "
                  "produces the shutdown-path evidence",
                  "SIGTERM: flushing usage sinks" in log5, f"exit={rc5}")
            check("N5: ...and the loss is LOUD, not silent (the documented contrast with a "
                  "signal that has no hook): the batch is reported as LOST",
                  "un-flushable batch" in log5 or "usage records are LOST" in log5,
                  next((l for l in log5.splitlines() if "LOST" in l or "un-flushable" in l),
                       "<no line>")[:160])
    finally:
        if node5.poll() is None:
            node5.kill()

    # ---- N3: the buffered usage row survives the shutdown ---------------------
    node3 = start_node("flush", {"HYDRA_SHUTDOWN_DRAIN_SECS": "5"}, ADMIN + 20, DATA + 20)
    db3 = os.path.join(DIR, "flush.db")
    try:
        if not wait_healthy(ADMIN + 20):
            check("N3: the third node became healthy", False, "never healthy")
        else:
            seed(ADMIN + 20, models=("slow2",))
            st3, _, _ = proxied("slow2", port=DATA + 20)
            before = usage_rows(db3)
            node3.send_signal(signal.SIGTERM)      # immediately: the batch is still buffered
            rc3 = node3.wait(timeout=30)
            time.sleep(0.3)
            after = usage_rows(db3)
            announce("N3 the persisted usage rows", f"request HTTP {st3}; rows before SIGTERM="
                     f"{before} after exit={after}; exit={rc3}")
            check("N3: the usage row that was still BUFFERED when SIGTERM arrived is persisted "
                  "after the process is gone (the reason the flush hook exists — Pingora exits "
                  "without running destructors)",
                  isinstance(after, int) and after >= 1, f"rows after exit={after}")
    finally:
        if node3.poll() is None:
            node3.kill()
        upstream.shutdown()

    print()
    if failures:
        print(f"SHUTDOWN DRAIN: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("SHUTDOWN DRAIN: PASSED (in-flight requests survive SIGTERM with a complete 200, the "
          "drain budget is a real bound, the buffered usage row is flushed, and the probes' "
          "behaviour during the drain is recorded)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
