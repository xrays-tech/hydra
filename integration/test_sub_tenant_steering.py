#!/usr/bin/env python3
"""Sub-tenant / key-prefix STEERING on the chat path — the documented security boundary.

`design-sub-tenant.md` §4.1 and the tenant contract §7.6 describe two mechanisms that steer a
client key to a provider, and one boundary that is easy to get wrong:

  * a **sub-tenant** matches by `key_prefix`; its routes are per-model OVERRIDES
    (`model-specific > default`), and a model it has no route for falls back to the tenant's
    own routing ("routes are overrides; otherwise the tenant default applies");
  * an **operator `provider-key-bindings`** row also matches by prefix and is the stronger
    one: it restricts the candidate set to that provider (fail-closed), and when it matches
    the sub-tenant gate is skipped entirely (ruling a');
  * §7.6.2: **"deleting or disabling a sub-tenant ≠ revoking"** — those keys keep working
    through the DEFAULT pipeline. Only the tenant's own `auth_url` can revoke a key.

Cases (two distinguishable mock upstreams so the SERVING PROVIDER is observable):
  S1 baseline: a plain key is served by one of the tenant's providers
  S2 a sub-tenant-prefixed key is steered to that sub-tenant's route (repeatedly)
  S3 a model the sub-tenant has NO route for falls back to the tenant default
  S4 DELETING the sub-tenant does not revoke the key (documented boundary)
  S5 DISABLING it does not revoke the key either
  S6 an operator prefix binding wins: the candidate set is restricted to its provider
  S7 the `model ∩ authorised providers` backstop still refuses an unauthorised model

Run: python3 integration/test_sub_tenant_steering.py     # needs target/debug/hydra
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
DIR = os.path.join(ROOT, ".acceptance", "sub-tenant-steering-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA = 18870, 18871
UP_A, UP_B = 18879, 18878
TOKEN = "hydra-steering-admin-2026"
SUB_PREFIX = "SUB_"
OPB_PREFIX = "OPB_"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def make_upstream(tag):
    class H(BaseHTTPRequestHandler):
        def do_GET(self):
            body = b'{"object":"list","data":[]}'
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            if length:
                self.rfile.read(length)
            if self.path.startswith("/auth"):
                body = json.dumps({"allowed": True, "expires_in": 300}).encode()
            else:
                body = json.dumps({"id": f"chatcmpl-{tag}", "object": "chat.completion",
                                   "choices": [{"index": 0,
                                                "message": {"role": "assistant", "content": tag},
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

    return H


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


def chat(model, key):
    """Returns (status, serving tag)."""
    st, out = call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token=key,
                   host="steering.local",
                   body={"model": model, "messages": [{"role": "user", "content": "hi"}]})
    tag = None
    try:
        tag = json.loads(out)["id"].removeprefix("chatcmpl-")
    except Exception:
        tag = None
    return st, tag, out


def start_node():
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'steering.db')}?mode=rwc",
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


def must(step, st, out):
    # 204 is the documented DELETE answer (and 200/201 the create answers).
    if st not in (200, 201, 204):
        raise SystemExit(f"[steering] CANNOT VERIFY: {step} -> {st} {out[:160]}")


def seed():
    """p-a (tag A) serves `shared` + `onlya`; p-b (tag B) serves `shared` only."""
    for path, payload in (
        ("/providers", {"id": "p-a", "key": "p-a", "name": "A",
                        "endpoint": f"http://127.0.0.1:{UP_A}", "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm-a1", "key": "shared", "name": "S", "provider_id": "p-a",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm-a2", "key": "onlya", "name": "OA", "provider_id": "p-a",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk-a", "provider_id": "p-a", "api_key": "sk-a",
                            "created_at": ""}),
        ("/providers", {"id": "p-b", "key": "p-b", "name": "B",
                        "endpoint": f"http://127.0.0.1:{UP_B}", "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm-b1", "key": "shared", "name": "S", "provider_id": "p-b",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk-b", "provider_id": "p-b", "api_key": "sk-b",
                            "created_at": ""}),
        ("/tenants", {"id": "t1", "name": "T", "domain": "steering.local",
                      "auth_url": f"http://127.0.0.1:{UP_A}/auth", "enabled": True,
                      "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp-a", "tenant_id": "t1", "provider_id": "p-a",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp-b", "tenant_id": "t1", "provider_id": "p-b",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "shared",
                            "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm2", "tenant_id": "t1", "model_key": "onlya",
                            "created_at": "", "updated_at": ""}),
    ):
        st, out = admin("POST", path, payload)
        must(f"seeding {path}", st, out)
    admin("POST", "/reload", {})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[steering] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    up_a = ThreadingHTTPServer(("127.0.0.1", UP_A), make_upstream("A"))
    up_b = ThreadingHTTPServer(("127.0.0.1", UP_B), make_upstream("B"))
    up_a.daemon_threads = True
    up_b.daemon_threads = True
    threading.Thread(target=up_a.serve_forever, daemon=True).start()
    threading.Thread(target=up_b.serve_forever, daemon=True).start()
    node = start_node()
    try:
        if not wait_healthy():
            print("[steering] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "node.log")).read()[-600:], file=sys.stderr)
            return 2
        seed()

        # ---- S1: baseline --------------------------------------------------------
        st, tag, out = chat("shared", "sk-plain-key-1234")
        check("S1: a plain key is served by one of the tenant's providers",
              st == 200 and tag in ("A", "B"), f"HTTP {st} tag={tag} {out[:60]}")

        # ---- S2: a sub-tenant steers its prefixed key ----------------------------
        st, out = admin("POST", "/sub-tenants",
                        {"id": "sub1", "tenant_id": "t1", "name": "sub1",
                         "key_prefix": SUB_PREFIX, "enabled": True, "created_at": ""})
        must("creating the sub-tenant", st, out)
        st, out = admin("POST", "/sub-tenant-routes",
                        {"id": "str-shared", "sub_tenant_id": "sub1", "model_key": "shared",
                         "provider_id": "p-b", "enabled": True, "created_at": "",
                         "updated_at": ""})
        must("routing `shared` to p-b for the sub-tenant", st, out)
        admin("POST", "/reload", {})
        time.sleep(0.3)
        tags = [chat("shared", SUB_PREFIX + "abc123")[1] for _ in range(4)]
        announce("S2 four requests with the sub-tenant's key", f"tags={tags}")
        check("S2: the sub-tenant's key is STEERED to its route's provider (always B)",
              tags == ["B"] * 4, f"tags={tags}")

        # ---- S3: a model with no route falls back to the tenant default ----------
        st, tag, out = chat("onlya", SUB_PREFIX + "abc123")
        check("S3: a model the sub-tenant has NO route for falls back to the tenant default "
              "(served by A, the only provider of `onlya`)",
              st == 200 and tag == "A", f"HTTP {st} tag={tag} {out[:60]}")

        # ---- S6: an operator prefix binding wins ---------------------------------
        st, out = admin("POST", "/provider-key-bindings",
                        {"id": "opb-a", "key_prefix": OPB_PREFIX, "provider_id": "p-a",
                         "enabled": True, "created_at": "", "updated_at": ""})
        must("creating the operator prefix binding", st, out)
        admin("POST", "/reload", {})
        time.sleep(0.3)
        tags = [chat("shared", OPB_PREFIX + "xyz789")[1] for _ in range(4)]
        announce("S6 four requests with the operator-bound key", f"tags={tags}")
        check("S6: an operator prefix binding restricts routing to ITS provider (always A), "
              "even though `shared` is also served by B",
              tags == ["A"] * 4, f"tags={tags}")

        # ---- S7: the model ∩ authorised backstop ---------------------------------
        st, tag, out = chat("not-a-model", OPB_PREFIX + "xyz789")
        check("S7: an unauthorised model is refused (`model_not_allowed`) regardless of the "
              "prefix", st == 403 and "model_not_allowed" in out, f"HTTP {st} {out[:70]}")

        # ---- S4/S5: deleting or disabling a sub-tenant is NOT revocation ---------
        st, out = admin("DELETE", "/sub-tenants/sub1")
        must("deleting the sub-tenant", st, out)
        admin("POST", "/reload", {})
        time.sleep(0.3)
        st_del, tag_del, out_del = chat("shared", SUB_PREFIX + "abc123")
        announce("S4 after deleting the sub-tenant", f"HTTP {st_del} tag={tag_del}")
        check("S4 (§7.6.2): the key still works through the DEFAULT pipeline after the "
              "sub-tenant is deleted — a deleted sub-tenant is NOT revoked",
              st_del == 200 and tag_del in ("A", "B"), f"HTTP {st_del} tag={tag_del}")

        st, out = admin("POST", "/sub-tenants",
                        {"id": "sub2", "tenant_id": "t1", "name": "sub2",
                         "key_prefix": "OFF_", "enabled": False, "created_at": ""})
        must("creating a DISABLED sub-tenant", st, out)
        st, out = admin("POST", "/sub-tenant-routes",
                        {"id": "str-off", "sub_tenant_id": "sub2", "model_key": "shared",
                         "provider_id": "p-b", "enabled": True, "created_at": "",
                         "updated_at": ""})
        must("routing for the disabled sub-tenant", st, out)
        admin("POST", "/reload", {})
        time.sleep(0.3)
        # The default pipeline is SWRR over equal weights, which ALTERNATES — so the honest
        # way to show "this key is no longer steered" is the opposite of S2: the tags must
        # NOT all be B. (The first version's assertion was a tautology; the plain key is
        # measured first as the control.)
        plain_tags = [chat("shared", "sk-plain-key-1234")[1] for _ in range(6)]
        off_tags = [chat("shared", "OFF_" + "abc123")[1] for _ in range(6)]
        announce("S5 default-pipeline tags", f"plain={plain_tags} disabled-sub={off_tags}")
        check("S5 (control): the default pipeline with equal weights alternates between both "
              "providers", set(plain_tags) == {"A", "B"}, f"plain={plain_tags}")
        check("S5: a DISABLED sub-tenant does not steer and does not revoke — the key is not "
              "pinned to the disabled sub-tenant's route",
              all(chat("shared", "OFF_" + "abc123")[0] == 200 for _ in range(1))
              and set(off_tags) == {"A", "B"}, f"off={off_tags}")
    finally:
        stop(node)
        up_a.shutdown()
        up_b.shutdown()

    print()
    if failures:
        print(f"SUB-TENANT STEERING: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("SUB-TENANT STEERING: PASSED (route steering, per-model fallback, operator binding "
          "wins, delete/disable != revoke, model backstop)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
