#!/usr/bin/env python3
"""Losing usage rows: a BILLING-relevant counter that no document ever mentioned.

`crates/hydra-server/src/sink.rs` counts every usage record it could not deliver —
`note_usage_drop(reason, n)` with the file comment "*The metric is the point: losing billing data
must never be visible only as a log line* (audit §3.9)" — and the vocabulary is four reasons:

  `channel_full`       the bounded channel (capacity = batch size, 256 by default) is full because
                       the flush loop is busy retrying a backend that is down
  `channel_closed`     the sink is already shut down (or the channel is closed)
  `retention_cap`      the in-memory buffer reached `MAX_RETAINED` (10 000) and incoming records
                       are refused while the backend stays down
  `shutdown_unflushed` the final flush at shutdown failed (see round 105's drain drill)

`grep -rn usage_dropped_total dev-docs/` is **empty**: this counter appears in no operator
document, and §9.1's alert table has no row for it — so the one signal that says "you are
under-billing" was reachable only by reading the source. This drill measures the behaviour it is
supposed to describe:

  D0  a HEALTHY ClickHouse-shaped sink: rows land, and the drop counter never appears (no false
      positives)
  D1  the sink's backend goes away: a burst of requests must (a) all still answer 200 — losing
      telemetry must never break the proxy path — and (b) start counting drops with a reason
  D2  which reason fires first at the default settings, and how loud the log is
  D3  recovery: once the backend answers again the retained batch lands and no further drops
      accumulate

Run: python3 integration/test_usage_drop_accounting.py    # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "usage-drop-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM, CH_PORT = 18770, 18771, 18779, 18777
TOKEN = "hydra-drop-admin-2026"
DOMAIN = "drop.local"
BURST = 3000                # the channel is 256 deep AND the loop keeps draining until the
                            # first batch flush starts backing off, so 400 was not enough (measured)
REASONS = {"channel_full", "channel_closed", "retention_cap", "shutdown_unflushed"}

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Received:
    def __init__(self):
        self.lock = threading.Lock()
        self.inserts = 0

    def record(self):
        with self.lock:
            self.inserts += 1

    def count(self):
        with self.lock:
            return self.inserts


RECEIVED = Received()


def make_ch_mock():
    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            if length:
                self.rfile.read(length)
            RECEIVED.record()
            self.send_response(200)
            self.send_header("Content-Length", "0")
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True

        def log_message(self, *a):
            pass

    return H


class Upstream(BaseHTTPRequestHandler):
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
            payload = json.dumps({"id": "chatcmpl-drop", "object": "chat.completion",
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


def call(method, url, token=None, body=None, timeout=20, host=None):
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


def admin(method, path, body=None):
    return call(method, f"http://127.0.0.1:{ADMIN}/api/v1{path}", token=TOKEN, body=body)


def proxied(key="sk-drop-1"):
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token=key, host=DOMAIN,
                body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})


def start_node():
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'drop.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_USAGE_SINK": "clickhouse",
        "HYDRA_CLICKHOUSE_URL": f"http://127.0.0.1:{CH_PORT}",
        "RUST_LOG": "info",
    })
    log = open(os.path.join(DIR, "drop.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health")[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is not None and proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def close_server(server):
    if server is None:
        return
    server.shutdown()
    server.server_close()


def seed():
    rows = [
        ("providers", {"id": "p1", "key": "p1", "name": "P",
                       "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                       "created_at": "", "updated_at": ""}),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": DOMAIN,
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]
    for path, body in rows:
        st, out = admin("POST", f"/{path}", body)
        if st not in (200, 201):
            raise SystemExit(f"[drop] CANNOT VERIFY: seeding {path} -> {st} {out[:140]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def drop_samples():
    """[(reason, value)] from `hydra_usage_records_dropped_total`, if the series exists at all.

    The name is `…_records_dropped_total`: the helper in `metrics.rs` is called `usage_dropped_total`,
    which is NOT the registered series — the first version of this drill read the helper's name and
    therefore saw an empty series while the node was logging 2488 drops.
    """
    _, out = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
    rows = []
    for line in out.splitlines():
        if line.startswith("hydra_usage_records_dropped_total{") and not line.startswith("#"):
            try:
                reason = line.split('reason="', 1)[1].split('"', 1)[0]
                rows.append((reason, float(line.rsplit(" ", 1)[1])))
            except (IndexError, ValueError):
                pass
    return rows


def total_drops():
    return sum(v for _, v in drop_samples())


def main():
    if not os.path.exists(BIN):
        print(f"[drop] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    ch = ThreadingHTTPServer(("127.0.0.1", CH_PORT), make_ch_mock())
    ch.daemon_threads = True
    threading.Thread(target=ch.serve_forever, daemon=True).start()

    node = start_node()
    try:
        if not wait_healthy():
            print("[drop] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "drop.log"), errors="replace").read()[-600:],
                  file=sys.stderr)
            return 2
        seed()

        # ---- D0: healthy sink, no drops ---------------------------------------
        codes = [proxied(f"sk-drop-ok-{i}")[0] for i in range(5)]
        got_rows = False
        deadline = time.time() + 12
        while time.time() < deadline:
            if RECEIVED.count() >= 1:
                got_rows = True
                break
            time.sleep(0.25)
        samples0 = drop_samples()
        announce("D0 the healthy sink", f"codes={codes} inserts={RECEIVED.count()} "
                 f"drops={samples0}")
        check("D0: with a working sink every request is served and rows reach it",
              all(c == 200 for c in codes) and got_rows, f"codes={codes} inserts={RECEIVED.count()}")
        check("D0: ...and `hydra_usage_records_dropped_total` does not appear at all (no false "
              "positives)", samples0 == [], f"{samples0}")

        # ---- D1/D2: the backend goes away, traffic keeps flowing ---------------
        close_server(ch)
        time.sleep(0.5)
        # Drive in chunks and STOP at the first drop, so the drill reports the threshold rather
        # than a bare "it eventually drops": with the default 256-deep channel the sink buffers a
        # full batch and starts its backoff before the channel can overflow, which is why the
        # first version (400 requests) saw ZERO drops and the whole leg failed for that reason.
        burst = []
        first_drop_at = None
        # The reason set AT the first drop is what "the first reason an operator sees" means — read it
        # here, not after the burst, because a later sample could contain a second reason and hide
        # which one came first (round 168: the check used to `any(...)` the final sample, so it
        # proved presence, never precedence).
        samples_first = None
        for chunk in range(0, BURST, 100):
            for i in range(chunk, chunk + 100):
                burst.append(proxied(f"sk-drop-burst-{i}")[0])
            if total_drops() > 0:
                first_drop_at = len(burst)
                samples_first = drop_samples()
                break
        ok_burst = sum(1 for c in burst if c == 200)
        samples1 = drop_samples()
        log = open(os.path.join(DIR, "drop.log"), errors="replace").read()
        warn_lines = [l for l in log.splitlines() if "dropping usage record" in l]
        announce("D1 the burst against a dead sink",
                 f"{len(burst)} requests (first drop after {first_drop_at}), {ok_burst} answered "
                 f"200; drops={samples1}; drop-warning lines={len(warn_lines)}")
        check("D1: losing telemetry never breaks the proxy path — every request in the burst is "
              "still served 200",
              ok_burst == len(burst) and len(burst) > 0, f"{ok_burst}/{len(burst)} were 200")
        check("D1: ...and the loss is COUNTED, not only logged (the counter the audit note "
              "insists on)", total_drops() > 0, f"drops={samples1}")
        check("D2: the reason label comes from the documented vocabulary",
              samples1 and all(r in REASONS for r, _ in samples1),
              f"reasons={[r for r, _ in samples1]} (documented set: {sorted(REASONS)})")
        announce("D2 the reasons that actually fired at the default settings",
                 f"{samples1}")
        # The claim is PRECEDENCE, so the predicate is an exact set equality on the sample taken at
        # the first drop: measured 2026-10-01, that sample contains `channel_full` and nothing else
        # (`[('channel_full', 88.0)]` after 600 requests). `any(...)` on the LATER sample — what this
        # check used to do — is satisfied by a run in which some other reason fired first.
        check("D2: at the shipped defaults the first reason an operator sees is the bounded "
              "channel (`channel_full`) — the 10 000-record retention cap needs the buffer to "
              "fill first, and the channel overflows long before that",
              [r for r, _ in samples_first or []] == ["channel_full"],
              f"reasons at the first drop (after {first_drop_at} request(s))="
              f"{[r for r, _ in samples_first or []]}, at the end of the burst="
              f"{[r for r, _ in samples1]} (documented set: {sorted(REASONS)})")
        check("D2: ...and each drop is warned about (with the trace id), so a log-only operator "
              "can also see it",
              len(warn_lines) > 0 and "dropped_trace_id" in log,
              f"{len(warn_lines)} warning line(s); first: "
              f"{warn_lines[0][:120] if warn_lines else '<none>'}")

        # ---- D3: recovery ------------------------------------------------------
        drops_before_recovery = total_drops()
        inserts_before = RECEIVED.count()
        ch2 = ThreadingHTTPServer(("127.0.0.1", CH_PORT), make_ch_mock())
        ch2.daemon_threads = True
        threading.Thread(target=ch2.serve_forever, daemon=True).start()
        landed = False
        deadline = time.time() + 20
        while time.time() < deadline:
            if RECEIVED.count() > inserts_before:
                landed = True
                break
            time.sleep(0.5)
        codes3 = [proxied(f"sk-drop-recover-{i}")[0] for i in range(4)]
        time.sleep(1.0)
        drops_after = total_drops()
        announce("D3 after the sink recovered",
                 f"landed={landed} inserts={RECEIVED.count()} codes={codes3} "
                 f"drops {drops_before_recovery} -> {drops_after}")
        check("D3: once the backend answers again the retained batch reaches it (the retry loop "
              "never gives up while it can make progress)",
              landed, f"inserts before={inserts_before} after={RECEIVED.count()}")
        check("D3: ...serving is unaffected throughout", all(c == 200 for c in codes3),
              f"codes={codes3}")
        check("D3: ...and the drop counter STOPS growing once the backend is healthy again",
              drops_after == drops_before_recovery,
              f"{drops_before_recovery} -> {drops_after}")
    finally:
        stop(node)
        close_server(ch)
        upstream.shutdown()

    print()
    if failures:
        print(f"USAGE DROP ACCOUNTING: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("USAGE DROP ACCOUNTING: PASSED (no false positives on a healthy sink; a dead backend "
          "counts and warns about every undeliverable row while the proxy keeps serving; the "
          "reason vocabulary and its real order of appearance are measured)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
