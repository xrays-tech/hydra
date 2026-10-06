#!/usr/bin/env python3
"""The CIRCUIT BREAKER's documented lifecycle, end to end.

Three documents promise this workflow and none of them had ever been executed black-box
(`load_breaker_swrr.rs` is an in-process unit suite):

  * `admin-ui/api-docs.js`: `GET /api/v1/breaker` = "the circuit-breaker dead-set: providers
    currently excluded from routing (consecutive failures)"; `DELETE /api/v1/breaker/{id}` =
    "force-clears the circuit breaker for one provider (marks it healthy again)";
  * `ops.md` §9.1: `hydra_candidate_skipped_total{reason="breaker_dead"}` — the alert row for
    "a provider that WOULD have served the request was dropped while the candidate set was
    built";
  * `design.md` §8.4: `on_failure` on a failed attempt, `on_success` on a 2xx first byte.

Cases (a throwaway node, one provider pointing at a CLOSED port, one healthy mock):
  B1 the dead-set is empty to start with (the "no false alarm" direction)
  B2 consecutive failures TRIP it: `GET /api/v1/breaker` lists the provider, `/api/v1/health`
     reports `breaker_dead: 1`, and the §9.1 counter fires
  B3 while dead, traffic stops being routed to it — a client request never reaches the dead
     provider's endpoint again (proved by the endpoint never coming back up between two
     requests) and the healthy provider still serves
  B4 `DELETE /api/v1/breaker/{id}` clears it: reported `was_dead: true`, the dead-set empties,
     and the next request TRIES the provider again (its endpoint is now up, so it succeeds)
  B5 success keeps it healthy: the provider stays out of the dead-set

Run: python3 integration/test_breaker_lifecycle.py       # needs target/debug/hydra
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
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "breaker-lifecycle-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA = 18830, 18831
GOOD_UPSTREAM = 18839
DEAD_PORT = 18849          # nothing listens here until B4
TOKEN = "hydra-breaker-admin-2026"
THRESHOLD = 2              # HYDRA_BREAKER_THRESHOLD if wired; measured below either way

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class GoodUpstream(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        if self.path.startswith("/auth"):
            body = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            body = json.dumps({"id": "chatcmpl-good", "object": "chat.completion",
                               "choices": [{"index": 0,
                                            "message": {"role": "assistant", "content": "ok"},
                                            "finish_reason": "stop"}],
                               "usage": {"prompt_tokens": 5, "completion_tokens": 8,
                                         "total_tokens": 13}}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *a):
        pass


def call(method, url, token=None, body=None, host=None, timeout=20):
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


def dead_set():
    st, out = admin("GET", "/breaker")
    try:
        return st, json.loads(out)["dead"]
    except Exception:
        return st, []


def health():
    st, out = admin("GET", "/health")
    try:
        return st, json.loads(out)
    except Exception:
        return st, {}


def metric(name):
    _, body = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
    return [l for l in body.splitlines() if l.startswith(name) and not l.startswith("#")]


def proxied(model="flaky", key="sk-tenant-1"):
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token=key,
                host="breaker.local",
                body={"model": model, "messages": [{"role": "user", "content": "hi"}]})


def start_node():
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'breaker.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, "node.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health")[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed():
    """`flaky` is served ONLY by the provider whose endpoint is closed; `echo` by the mock."""
    for path, payload in (
        ("/providers", {"id": "p-flaky", "key": "p-flaky", "name": "Flaky",
                        "endpoint": f"http://127.0.0.1:{DEAD_PORT}", "weight": 5,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm-flaky", "key": "flaky", "name": "Flaky",
                              "provider_id": "p-flaky", "status": 1,
                              "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk-flaky", "provider_id": "p-flaky", "api_key": "sk-f",
                            "created_at": ""}),
        ("/providers", {"id": "p-good", "key": "p-good", "name": "Good",
                        "endpoint": f"http://127.0.0.1:{GOOD_UPSTREAM}", "weight": 5,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm-good", "key": "echo", "name": "Echo",
                              "provider_id": "p-good", "status": 1,
                              "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk-good", "provider_id": "p-good", "api_key": "sk-g",
                            "created_at": ""}),
        ("/tenants", {"id": "t1", "name": "T", "domain": "breaker.local",
                      "auth_url": f"http://127.0.0.1:{GOOD_UPSTREAM}/auth", "enabled": True,
                      "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p-flaky",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp2", "tenant_id": "t1", "provider_id": "p-good",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "flaky",
                            "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm2", "tenant_id": "t1", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
    ):
        st, out = admin("POST", path, payload)
        if st not in (200, 201):
            raise SystemExit(f"[breaker] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def start_dead_upstream():
    """Brings up the mock on the previously-closed port (B4 needs it)."""
    server = ThreadingHTTPServer(("127.0.0.1", DEAD_PORT), GoodUpstream)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server


def main():
    if not os.path.exists(BIN):
        print(f"[breaker] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    good = ThreadingHTTPServer(("127.0.0.1", GOOD_UPSTREAM), GoodUpstream)
    threading.Thread(target=good.serve_forever, daemon=True).start()
    node = start_node()
    revived = None
    try:
        if not wait_healthy():
            print("[breaker] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "node.log")).read()[-600:], file=sys.stderr)
            return 2
        seed()

        # ---- B1: the dead-set starts empty -------------------------------------
        st, dead = dead_set()
        st_h, h = health()
        check("B1: `GET /api/v1/breaker` answers with an empty dead-set on a healthy node",
              st == 200 and dead == [], f"HTTP {st} dead={dead}")
        check("B1: ...and `/api/v1/health` reports `breaker_dead: 0`",
              st_h == 200 and h.get("breaker_dead") == 0, f"breaker_dead={h.get('breaker_dead')}")

        # ---- B2: consecutive failures trip it ----------------------------------
        codes = []
        for _ in range(6):
            st_c, _ = proxied("flaky")
            codes.append(st_c)
            st_now, dead_now = dead_set()
            if "p-flaky" in dead_now:
                break
        st, dead = dead_set()
        st_h, h = health()
        skipped = metric("hydra_candidate_skipped_total")
        announce("B2 after repeated failures", f"codes={codes} dead={dead} "
                                               f"breaker_dead={h.get('breaker_dead')}")
        announce("§9.1 counter", f"{[l for l in skipped if 'breaker_dead' in l]}")
        check("B2: requests to the always-failing provider fail (no upstream at that port)",
              codes and codes[0] != 200, f"first={codes[0] if codes else None}")
        check("B2: consecutive failures put the provider in the DEAD-SET (`GET /api/v1/breaker`)",
              "p-flaky" in dead, f"dead={dead} codes={codes}")
        check("B2: ...and `/api/v1/health`'s `breaker_dead` agrees (it is the same set)",
              h.get("breaker_dead") == 1, f"breaker_dead={h.get('breaker_dead')}")
        # The §9.1 counter belongs to the SKIP event, not to the failure that tripped the
        # breaker: at this point every request still REACHED the provider (it was a
        # candidate, it failed). So the honest assertion here is the "no false alarm"
        # direction — and B3 asserts the counter firing once a request is routed while dead.
        check("B2: no skip is counted yet (nothing has been SKIPPED — the failures were attempts)",
              not [l for l in skipped if 'reason="breaker_dead"' in l],
              f"{[l for l in skipped if 'breaker_dead' in l]}")

        # ---- B3: traffic stops going to the dead provider ----------------------
        before_skip = [l for l in skipped if 'reason="breaker_dead"' in l]
        st_skip, _ = proxied("flaky")
        after_skip = [l for l in metric("hydra_candidate_skipped_total") if 'reason="breaker_dead"' in l]
        announce("B3 a request for the dead provider's model", f"HTTP {st_skip}")
        check("B3: the dead provider is SKIPPED and §9.1's "
              "`hydra_candidate_skipped_total{provider,reason=\"breaker_dead\"}` fires",
              after_skip != before_skip and any('reason="breaker_dead"' in l and 'p-flaky' in l
                                                for l in after_skip),
              f"{before_skip} -> {after_skip}")
        check("B3: the request fails over / is refused — it never silently succeeds through it",
              st_skip != 200, f"HTTP {st_skip}")
        st_good, body_good = proxied("echo")
        check("B3: ...while the HEALTHY provider keeps serving its own model",
              st_good == 200 and "chatcmpl-good" in body_good, f"HTTP {st_good} {body_good[:60]}")

        # ---- B4: the documented manual reset ----------------------------------
        st, out = admin("DELETE", "/breaker/p-flaky")
        try:
            payload = json.loads(out)
        except Exception:
            payload = {}
        announce("B4 DELETE /api/v1/breaker/p-flaky", f"HTTP {st} {payload}")
        check("B4: the reset reports `was_dead: true` and clears the set",
              st == 200 and payload.get("was_dead") is True and payload.get("dead") == [],
              f"HTTP {st} {out[:110]}")
        st, dead_at_start = dead_set()
        check("B4: `GET /api/v1/breaker` is empty again", dead_at_start == [], f"dead={dead_at_start}")
        # ...and the next request really TRIES it again: the upstream is up now, so it wins.
        revived = start_dead_upstream()
        st_revived, body_revived = proxied("flaky")
        announce("B4 after reviving the upstream and resetting the breaker",
                 f"HTTP {st_revived} {body_revived[:60]}")
        check("B4: the provider is used again (the reset is not cosmetic)",
              st_revived == 200 and st_revived != st_skip, f"HTTP {st_revived}")

        # ---- B5: success keeps it healthy --------------------------------------
        for _ in range(3):
            proxied("flaky")
        st, dead_end = dead_set()
        st_h, h_end = health()
        check("B5: a provider that succeeds stays OUT of the dead-set (on_success resets)",
              dead_end == [] and h_end.get("breaker_dead") == 0, f"dead={dead_end}")
    finally:
        stop(node)
        good.shutdown()
        if revived is not None:
            revived.shutdown()

    print()
    if failures:
        print(f"BREAKER LIFECYCLE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("BREAKER LIFECYCLE: PASSED (dead-set empties/refills, §9.1 skip counter, traffic "
          "stops being routed, DELETE reset really revives the provider, success resets)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
