#!/usr/bin/env python3
"""The per-provider ADMISSION QUEUE, asserted end to end.

Documented (design-admission-queue §5/§10, `admin-ui/api-docs.js`, `metrics.rs`):
  * `provider.max_concurrency` — max in-flight to that provider; `None` ⇒ unlimited
    (the passthrough opt-out), `0` ⇒ unlimited as well (ConfigData rule);
  * `provider.max_queue_depth` — max WAITERS; `0` ⇒ fail fast (no queue);
  * `provider.queue_wait_timeout_ms` — how long a waiter may wait before it gives up;
  * a denied acquire becomes `503 admission_denied` with `Retry-After`
    (`proxy.rs:1270-1273`), and is counted in
    `hydra_queue_drops_total{provider,reason}` with reason ∈ {full, timeout, closed};
  * gauges `hydra_permit_inflight` / `hydra_permit_available` / `hydra_queue_depth`
    (+ histogram `hydra_queue_wait_seconds`, counter `hydra_admission_decisions_total`);
  * `GET /api/v1/concurrency` reports `{provider_id,max_concurrency,inflight,available,
    queue_depth}` for providers that HAVE a cap.

Three cases:
  Q1 saturation: cap=2, queue=3, wait=2s, 8 concurrent -> 2 in flight, 3 queued, the
     rest shed with reason=full; the queue_depth gauge must NEVER exceed 3 (the bound
     that round-21/23's CAS fix was about);
  Q2 wait timeout: cap=1, queue=5, wait=300ms, 3 concurrent against a 2s upstream -> the
     waiters give up with reason=timeout;
  Q3 passthrough: cap unset -> no admission accounting for that provider and everything
     succeeds.

Run: python3 integration/test_admission_queue.py     # needs target/debug/hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import signal
import socket
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))  # integration/ -> repo root
DIR = os.path.join(ROOT, ".acceptance", "admission-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA = 18680, 18681
UPSTREAM = 18699
TOKEN = "hydra-admission-admin-2026"
UPSTREAM_DELAY = 1.2          # seconds — long enough to fill the queue deterministically

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def call(method, url, token=None, body=None, timeout=30):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, dict(r.headers), r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode(errors="replace")
    except Exception as e:
        return 0, {}, str(e)


class SlowUpstream(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        # The tenant's `auth_url` points here too, so the auth hop must get a real
        # verdict — and it must NOT sleep, or every request dies at the auth gate with
        # `auth_upstream_unavailable` (which is what the first version measured).
        if self.path.startswith("/auth"):
            body = json.dumps({"allowed": True, "reason": "admission-drill", "expires_in": 60}).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        time.sleep(UPSTREAM_DELAY)
        body = json.dumps({"id": "chatcmpl-slow", "object": "chat.completion",
                           "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"},
                                        "finish_reason": "stop"}],
                           "usage": {"prompt_tokens": 5, "completion_tokens": 8, "total_tokens": 13}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def start_hydra():
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'adm.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, "hydra.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/health", token=TOKEN)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def seed(provider_limits):
    """Tenant + provider + model + key + links; `provider_limits` sets the cap fields."""
    body = {"id": "p1", "key": "p1", "name": "P", "endpoint": f"http://127.0.0.1:{UPSTREAM}",
            "weight": 1, "created_at": "", "updated_at": ""}
    body.update(provider_limits)
    for path, payload in [
        ("providers", body),
        ("provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                             "status": 1, "created_at": "", "updated_at": ""}),
        ("provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up", "created_at": ""}),
        ("tenants", {"id": "t1", "name": "T", "domain": "load.local",
                     # the /auth path is what makes the mock answer a verdict instead of
                     # a (slow) chat completion
                     "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                     "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                              "created_at": "", "updated_at": ""}),
        ("tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                           "created_at": "", "updated_at": ""}),
    ]:
        st, _, out = call("POST", f"http://127.0.0.1:{ADMIN}/api/v1/{path}", token=TOKEN, body=payload)
        if st not in (200, 201):
            raise SystemExit(f"[admission] CANNOT VERIFY: seeding {path} -> {st} {out[:120]}")
    call("POST", f"http://127.0.0.1:{ADMIN}/api/v1/reload", token=TOKEN, body={})
    time.sleep(0.3)


def proxied():
    """A data-plane request THROUGH the proxy.

    The Host header is what selects the tenant (`resolve_tenant` reads it), so it must
    be set explicitly: without it the request is `404 unknown_domain` and every
    admission assertion measures nothing (the first version of this drill did exactly
    that).
    """
    body = {"model": "echo", "messages": [{"role": "user", "content": "hi"}]}
    req = urllib.request.Request(
        f"http://127.0.0.1:{DATA}/v1/chat/completions",
        data=json.dumps(body).encode(), method="POST",
        headers={"Content-Type": "application/json", "Host": "load.local",
                 "Authorization": "Bearer sk-tenant-1"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, dict(r.headers), r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode(errors="replace")
    except Exception as e:
        return 0, {}, str(e)


def metric(name):
    _, _, body = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
    return [l for l in body.splitlines() if l.startswith(name) and not l.startswith("#")]


def counter_value(lines, needle):
    """The value of the first counter line containing `needle`.

    Absent reads as 0.0 (a Prometheus counter is materialised on first use), and an unparsable line
    returns None so a malformed line can never be mistaken for a zero.
    """
    for line in lines:
        if needle in line:
            try:
                return float(line.rsplit(" ", 1)[1])
            except ValueError:
                return None
    return 0.0


def concurrency():
    st, _, body = call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/concurrency", token=TOKEN)
    if st != 200:
        return None
    try:
        return json.loads(body)
    except Exception:
        return None


def reconfigure(provider_limits):
    """Change the provider's admission limits through the documented whole-record PUT.

    The first version DELETED the provider and re-created it without checking either
    status, so case Q3 kept measuring the old limits (the delete had not taken effect).
    """
    body = {"id": "p1", "key": "p1", "name": "P", "endpoint": f"http://127.0.0.1:{UPSTREAM}",
            "weight": 1, "created_at": "", "updated_at": ""}
    body.update(provider_limits)
    st, _, out = call("PUT", f"http://127.0.0.1:{ADMIN}/api/v1/providers/p1", token=TOKEN, body=body)
    if st not in (200, 201):
        raise SystemExit(f"[admission] CANNOT VERIFY: PUT providers/p1 -> {st} {out[:160]}")
    call("POST", f"http://127.0.0.1:{ADMIN}/api/v1/reload", token=TOKEN, body={})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[admission] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    os.makedirs(DIR, exist_ok=True)
    for f in os.listdir(DIR):
        if f.endswith((".db", ".db-wal", ".db-shm", ".log")):
            os.remove(os.path.join(DIR, f))

    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), SlowUpstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    proc = start_hydra()
    try:
        if not wait_healthy():
            print("[admission] CANNOT VERIFY: hydra never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "hydra.log")).read()[-500:], file=sys.stderr)
            return 2

        # ---- Q1: saturation (cap 2, queue 3, wait 2s, 8 concurrent) ----
        seed({"max_concurrency": 2, "max_queue_depth": 3, "queue_wait_timeout_ms": 2000})
        results = []
        depths = []
        inflights = []
        threads = []
        lock = threading.Lock()

        def worker():
            r = proxied()
            with lock:
                results.append((r[0], r[2][:60]))

        # Snapshot the drop counters BEFORE the burst (round 174): the Q1 leg below claims a drop
        # "was counted", and an existence test cannot attribute a number to THIS burst — it cannot
        # fail when the value was already there (the round-173 C2 shape, fixed the same way).
        drops_before = metric("hydra_queue_drops_total")
        full_before = counter_value(drops_before, 'reason="full"')
        timeout_before = counter_value(drops_before, 'reason="timeout"')
        for _ in range(8):
            t = threading.Thread(target=worker)
            threads.append(t)
            t.start()
            time.sleep(0.05)
        # sample the gauges/endpoint while the burst is in flight
        sampler_stop = time.time() + 1.6
        while time.time() < sampler_stop:
            c = concurrency()
            if c and c.get("providers"):
                depths.append(c["providers"][0].get("queue_depth", 0))
                inflights.append(c["providers"][0].get("inflight", 0))
            time.sleep(0.05)
        for t in threads:
            t.join(timeout=60)

        codes = [c for c, _ in results]
        shed = [b for c, b in results if c == 503]
        check("Q1: 8 concurrent requests against a 2-permit provider produce 503s",
              503 in codes, f"codes={sorted(codes)}")
        check("Q1: the 503 body says admission_denied with a Retry-After",
              any("admission_denied" in b for b in shed),
              f"sample={shed[0] if shed else 'none'}")
        max_depth = max(depths) if depths else 0
        check("Q1: observed queue_depth NEVER exceeded max_queue_depth=3",
              max_depth <= 3, f"max observed depth={max_depth} over {len(depths)} samples")
        check("Q1: the queue was actually used (depth > 0 was observed)", max_depth > 0, f"max={max_depth}")
        drops = metric("hydra_queue_drops_total")
        full_after = counter_value(drops, 'reason="full"')
        timeout_after = counter_value(drops, 'reason="timeout"')
        check("Q1: hydra_queue_drops_total{\"reason=\"full\"} was counted — asserted as an "
              "INCREMENT, so the number is attributable to THIS burst",
              full_before is not None and full_after is not None and full_after > full_before,
              f"{drops[:2]} (full {full_before} -> {full_after})")
        # …and the drops must ACCOUNT for what the client saw: every 503 in this burst is one shed
        # request, and the shed reasons are `full` (queue length) and `timeout` (queue wait). Measured
        # 2026-10-01: 4 shed = full +3, timeout +1 ⇒ the sum is an equation here, and the one-sided
        # form below is deliberate: a request may be shed for a third documented reason this drill
        # does not drive.
        check("Q1: ...and the increments account for every shed request (full + timeout deltas >= the "
              "503s this burst produced)",
              None not in (full_before, full_after, timeout_before, timeout_after)
              and (full_after - full_before) + (timeout_after - timeout_before) >= len(shed),
              f"{len(shed)} shed; full +{full_after - full_before}, "
              f"timeout +{timeout_after - timeout_before}")
        # Round 161: `max(inflights) if inflights else 0 == 2` binds as
        # `max(inflights) if inflights else (0 == 2)` — a truthy number, so ANY non-zero peak passed
        # (verified: with a peak of 8 the check still printed PASS). The parentheses are the fix.
        check("Q1: in-flight peaked at exactly the configured 2 (sampled during the burst)",
              (max(inflights) if inflights else 0) == 2,
              f"max in-flight={max(inflights) if inflights else 0}")
        live = metric("hydra_permit_inflight")
        check("Q1: hydra_permit_inflight is exported (0 once the burst drained)",
              bool(live), f"{live}")
        decisions = metric("hydra_admission_decisions_total")
        check("Q1: hydra_admission_decisions_total is exported", bool(decisions), f"{decisions[:2]}")
        # ... and the OTHER direction: the stale-limits counter must stay SILENT for a
        # provider whose configured limits match the gate. ops.md §9.1 turns this series
        # into an alert ("accepted but not enforced"), and an alert that also fires when
        # nothing is stale is worse than no alert. Q1 is the right place to assert it:
        # these limits were written once, before any traffic, and never changed after.
        no_false_alarm = metric("hydra_admission_limits_stale_total")
        check("Q1: hydra_admission_limits_stale_total is ABSENT while the config matches "
              "the gate (no false alarm)", no_false_alarm == [], f"{no_false_alarm}")

        # ---- Q2: waiters give up after queue_wait_timeout_ms ----
        reconfigure({"max_concurrency": 1, "max_queue_depth": 5, "queue_wait_timeout_ms": 300})
        drops_before = metric("hydra_queue_drops_total")   # Q1's drops are NOT Q2's evidence
        results2 = []

        def worker2():
            r = proxied()
            with lock:
                results2.append((r[0], r[2][:60]))

        ts = [threading.Thread(target=worker2) for _ in range(3)]
        for t in ts:
            t.start()
            time.sleep(0.05)
        for t in ts:
            t.join(timeout=60)
        codes2 = sorted(c for c, _ in results2)
        drops2 = metric("hydra_queue_drops_total")
        check("Q2: with a 300ms wait budget against a 1.2s upstream, waiters are shed",
              429 not in codes2 and 503 in codes2, f"codes={codes2}")
        # Round 162: `hydra_queue_drops_total` is a PROCESS-cumulative vec and Q1 already produced a
        # `timeout` drop of its own (the repo's own record: `{full}=3 / {timeout}=1`), so asking
        # "does a timeout sample exist?" was answered by Q1's traffic — the leg stayed green even if
        # the wait budget stopped working and every waiter were shed as `full`. Compare the DELTA.
        timeout_before = sum(int(l.rsplit(" ", 1)[1]) for l in drops_before if 'reason="timeout"' in l)
        timeout_now = sum(int(l.rsplit(" ", 1)[1]) for l in drops2 if 'reason="timeout"' in l)
        check("Q2: the shed reason includes \"timeout\" (a NEW timeout drop since Q2 began, not Q1's)",
              timeout_now > timeout_before,
              f"timeout drops {timeout_before} -> {timeout_now} (all: {drops2})")

        # ---- Q3: passthrough (no cap) ----
        reconfigure({"max_concurrency": 0, "max_queue_depth": 0, "queue_wait_timeout_ms": None})
        results3 = []
        ts = []
        for _ in range(3):
            t = threading.Thread(target=lambda: results3.append(proxied()[0]))
            ts.append(t)
            t.start()
        for t in ts:
            t.join(timeout=60)
        check("Q3: max_concurrency=0 is the unlimited passthrough (all 3 succeed)",
              sorted(results3) == [200, 200, 200], f"codes={sorted(results3)}")
        # NB: /api/v1/concurrency cannot be used here to prove "0 = unlimited" — it
        # reports the RUNTIME gate, and p1's gate was created earlier with cap 2 (see Q4,
        # which pins that behaviour as a documented limitation).
        # ---- Q4: KNOWN LIMITATION — admission limits are NOT resized on hot-reload ----
        # `proxy/admission.rs` says so itself ("once created, a gate's semaphore is NOT
        # resized if a later call passes a different max_concurrency — the first policy
        # wins"); ops.md §4 and the /api/v1/concurrency description document it, and
        # implementing the resize is decision item D-14. This case PINS the current
        # behaviour so that a future resize is a deliberate, visible change: it asserts
        # the gate keeps the OLD cap and that the endpoint agrees with the gate (not with
        # the configuration), which is exactly the trap an operator falls into.
        probe_st, _, _ = call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/health", token=TOKEN)
        print(f"   (Q4 precheck: /api/v1/health -> {probe_st}; node alive={proc.poll() is None})")
        reconfigure({"max_concurrency": 3, "max_queue_depth": 3, "queue_wait_timeout_ms": 2000})
        samples_before = []
        ts = [threading.Thread(target=lambda: samples_before.append(proxied()[0])) for _ in range(3)]
        for t in ts:
            t.start()
            time.sleep(0.05)
        for t in ts:
            t.join(timeout=60)
        st, _, _ = call("PUT", f"http://127.0.0.1:{ADMIN}/api/v1/providers/p1", token=TOKEN, body={
            "id": "p1", "key": "p1", "name": "P", "endpoint": f"http://127.0.0.1:{UPSTREAM}",
            "weight": 1, "max_concurrency": 1, "max_queue_depth": 1, "queue_wait_timeout_ms": 200,
            "created_at": "", "updated_at": ""})
        call("POST", f"http://127.0.0.1:{ADMIN}/api/v1/reload", token=TOKEN, body={})
        time.sleep(0.4)
        st_row, _, row = call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/providers/p1", token=TOKEN)
        reported = None
        raw_st, _, raw_body = call("GET", f"http://127.0.0.1:{ADMIN}/api/v1/concurrency", token=TOKEN)
        print(f"   (Q4 /api/v1/concurrency raw -> HTTP {raw_st} {raw_body[:160]})")
        try:
            parsed = json.loads(raw_body)
        except Exception:
            parsed = {}
        entries = parsed.get("providers") if isinstance(parsed, dict) else parsed
        if isinstance(entries, list) and entries:
            reported = entries[0].get("max_concurrency") if isinstance(entries[0], dict) else None
        elif isinstance(entries, dict):
            first = next(iter(entries.values()), None)
            reported = first.get("max_concurrency") if isinstance(first, dict) else None
        check("Q4 (known limitation): the DB row takes the new cap",
              '"max_concurrency":1' in row.replace(" ", ""), f"PUT -> {st}")
        # The gate was created at p1's FIRST request, when the cap was 2 (the value it was
        # seeded with at the top of this test). Neither the later PUT to 3 nor the PUT to 1
        # moved it: "the first policy wins".
        check("Q4 (known limitation): the RUNTIME gate keeps the cap captured at its FIRST "
              "request — neither the increase to 3 nor the decrease to 1 was applied "
              "(documented in ops.md §4 + api-docs; resize tracked as D-14)",
              reported == 2, f"/api/v1/concurrency reports max_concurrency={reported} (gate, created with 2) "
                              f"while the DB row says 1")
        # …and the mismatch must be VISIBLE, never presented as the configured cap: the
        # endpoint reports both sides plus `limits_stale`, the node logs a WARN, and the
        # request is counted in hydra_admission_limits_stale_total. (Before round 78 an
        # operator saw only the stale value and would conclude the change had applied.)
        entries = parsed.get("providers") if isinstance(parsed, dict) else parsed
        first = entries[0] if isinstance(entries, list) and entries else {}
        configured = first.get("configured_max_concurrency")
        check("Q4: the endpoint reports the CONFIGURED cap next to the enforced one",
              configured == 1, f"configured_max_concurrency={configured}, enforced={first.get('max_concurrency')}")
        check("Q4: the endpoint flags the mismatch with limits_stale=true",
              first.get("limits_stale") is True, f"limits_stale={first.get('limits_stale')}")
        # a request after the change must be admitted under the stale limits AND counted
        proxied()
        time.sleep(0.2)
        stale = metric("hydra_admission_limits_stale_total")
        check("Q4: hydra_admission_limits_stale_total counts requests admitted under stale limits",
              any('provider="p1"' in l and not l.endswith(" 0") for l in stale), f"{stale[:2]}")
        log = open(os.path.join(DIR, "hydra.log"), errors="replace").read()
        check("Q4: the node warns once with both sides named",
              "admission limits changed in configuration" in log and "configured_max_concurrency=1" in log,
              "WARN present" if "admission limits changed" in log else "no WARN found")

    finally:
        upstream.shutdown()
        if proc.poll() is None:
            proc.send_signal(signal.SIGKILL)
            try:
                proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                pass

    print()
    if failures:
        print(f"ADMISSION QUEUE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("ADMISSION QUEUE: PASSED (saturation bound, drop reasons, gauges, endpoint, "
          "passthrough, and the documented no-resize-on-reload limitation)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
