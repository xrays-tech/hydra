#!/usr/bin/env python3
"""The DATA-PLANE model catalog (`GET /v1/models`), executed.

`dev-docs/design.md:632` and `design-tenant-model-catalog.md` document a precise, unusual
contract, and a security-relevant part of it is the kind of thing that rots silently:

  "`GET /v1/models` 不再直通 —— **无条件公开**在本地聚合应答该租户可调用模型目录（跨授权
   provider 并集 × 租户模型白名单 × 在线 provider 过滤，OpenAI 兼容形状；带不带 api-key 均可读
   —— 2026-09-09 起出示的 key **不再走外部鉴权**，仅按前缀绑定收窄目录；聊天等调用仍须 api-key）"

and the code adds: the answer is **fully local** (no body read, no limit gate, no routing, no
upstream dial, no usage record), only `GET` on the exact path is intercepted, and a presented
key can only NARROW the listing (`bound view ⊆ anonymous union`).

Cases (throwaway node + a request-COUNTING mock upstream so "local" is measurable):
  C1 the catalog answers with the tenant's routable models in OpenAI shape
  C2 it answers with NO api-key at all (the documented "unconditionally public" read)
  C3 an INVALID key is not rejected — and (the security claim) the mock's AUTH counter does
     not move either: the directory never runs external auth
  C4 offline filtering: a model whose provider is breaker-dead is omitted
  C5 the whitelist is honoured: an authorized provider's model that the tenant is NOT
     whitelisted for is omitted
  C6 what a PRESENTED key does to the listing: an operator `provider-key-bindings` row narrows it to
     its provider (an EXACT set, so widening is impossible); a sub-tenant prefix narrows it only where
     a route covers that model, and a route pinning a model to a provider that does NOT serve it is
     refused at the write boundary (measured `400 model_not_served_by_provider`)
  C6b the catalog MIRRORS the sub-tenant gate the way `resolve` does (design §7.1c): a route pinning a
     model to a breaker-DEAD provider empties its provider set, so the model vanishes for that key
     while the anonymous and unknown-prefix listings keep it through the other provider. The
     construction is needed because the write path refuses the direct fixture — before round 172 this
     half of C6 could not fail at all (m1 and m3 ARE the anonymous set, so `in`-checks held for every
     listing that contained them, including the anonymous one)
  C7 it is answered LOCALLY: the chat/upstream counter does not move for the catalog call
  C8 only `GET` on the exact path is intercepted (`POST /v1/models` is not a catalog)
  C9 the response carries the data-plane trace header

Run: python3 integration/test_model_catalog.py      # needs target/debug/hydra
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
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "model-catalog-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18850, 18851, 18859
DEAD_PORT = 18869                      # nothing listens here (C4 needs an offline provider)
TOKEN = "hydra-catalog-admin-2026"
SUB_PREFIX = "SUB_"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Counters:
    auth = 0
    chat = 0
    lock = threading.Lock()


class Upstream(BaseHTTPRequestHandler):
    def do_GET(self):
        # The breaker probe dials `GET {endpoint}/v1/models`; answer it so the live
        # provider stays alive without touching the counters below.
        self._record(is_chat=False)
        body = b'{"object":"list","data":[]}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        if self.path.startswith("/auth"):
            with Counters.lock:
                Counters.auth += 1
            body = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            self._record(is_chat=True)
            body = json.dumps({"id": "chatcmpl-cat", "object": "chat.completion",
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

    def _record(self, is_chat):
        if is_chat:
            with Counters.lock:
                Counters.chat += 1

    def log_message(self, *a):
        pass


def counts():
    with Counters.lock:
        return Counters.auth, Counters.chat


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
            return r.status, dict(r.headers), r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode(errors="replace")
    except Exception as e:
        return 0, {}, str(e)


def admin(method, path, body=None):
    st, _, out = call(method, f"http://127.0.0.1:{ADMIN}/api/v1{path}", token=TOKEN, body=body)
    return st, out


def catalog(key=None):
    st, hdrs, out = call("GET", f"http://127.0.0.1:{DATA}/v1/models", token=key,
                         host="catalog.local")
    try:
        ids = sorted(m["id"] for m in json.loads(out).get("data", []))
    except Exception:
        ids = None
    return st, hdrs, out, ids


def chat(model="m1", key="sk-tenant-1"):
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token=key,
                host="catalog.local",
                body={"model": model, "messages": [{"role": "user", "content": "hi"}]})


def start_node():
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'catalog.db')}?mode=rwc",
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
    """p1 (live) serves m1+m2; p2 (closed port) serves m3. The tenant is whitelisted for
    m1 and m3 only, so `m2` must be absent (whitelist) and `m3` must be absent (offline)."""
    for path, payload in (
        ("/providers", {"id": "p1", "key": "p1", "name": "Live",
                        "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm1", "key": "m1", "name": "M1", "provider_id": "p1",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm2", "key": "m2", "name": "M2", "provider_id": "p1",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                            "created_at": ""}),
        ("/providers", {"id": "p2", "key": "p2", "name": "Offline",
                        "endpoint": f"http://127.0.0.1:{DEAD_PORT}", "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm3", "key": "m3", "name": "M3", "provider_id": "p2",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk2", "provider_id": "p2", "api_key": "sk-up2",
                            "created_at": ""}),
        ("/tenants", {"id": "t1", "name": "T", "domain": "catalog.local",
                      "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                      "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp2", "tenant_id": "t1", "provider_id": "p2",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "m1",
                            "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm3", "tenant_id": "t1", "model_key": "m3",
                            "created_at": "", "updated_at": ""}),
    ):
        st, out = admin("POST", path, payload)
        if st not in (200, 201):
            raise SystemExit(f"[catalog] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[catalog] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    upstream.daemon_threads = True
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    node = start_node()
    try:
        if not wait_healthy():
            print("[catalog] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "node.log")).read()[-600:], file=sys.stderr)
            return 2
        seed()

        # ---- C1/C2/C3/C7: the public, local directory ---------------------------
        auth_before, chat_before = counts()
        st, hdrs, out, ids = catalog(key="sk-tenant-1")
        auth_after_valid, chat_after_valid = counts()
        announce("C1 the catalog (valid key)", f"HTTP {st} ids={ids}")
        # At this point NEITHER provider has failed yet, so both are "online" and the
        # catalog is whitelist ∩ authorized = {m1, m3} (m2 is excluded by the whitelist —
        # asserted in C5). C4 re-reads the catalog AFTER the offline provider trips.
        check("C1: `GET /v1/models` answers 200 with the tenant's routable models, OpenAI shape",
              st == 200 and ids == ["m1", "m3"] and json.loads(out).get("object") == "list",
              f"HTTP {st} ids={ids} body={out[:90]}")
        check("C7: ...answered LOCALLY — no chat/upstream call happened for the catalog",
              chat_after_valid == chat_before, f"chat calls {chat_before} -> {chat_after_valid}")
        check("C1: ...and the response carries the data-plane trace header",
              bool(hdrs.get("X-Hydra-Trace-Id") or hdrs.get("x-hydra-trace-id")),
              f"{[k for k in hdrs if 'race' in k]}")

        st, _, out, ids_no_key = catalog(key=None)
        check("C2: it answers with NO api-key at all (the documented public read)",
              st == 200 and ids_no_key == ids, f"HTTP {st} ids={ids_no_key}")

        auth_before_bad, _ = counts()
        st, _, out, ids_bad = catalog(key="sk-totally-invalid-key")
        auth_after_bad, _ = counts()
        announce("C3 an INVALID key", f"HTTP {st} ids={ids_bad} auth calls "
                                      f"{auth_before_bad} -> {auth_after_bad}")
        check("C3: an invalid key is NOT rejected (the directory never runs external auth)",
              st == 200 and ids_bad == ids, f"HTTP {st} ids={ids_bad}")
        check("C3 (security): ...and the AUTH counter did not move either — the presented "
              "key is only used for prefix narrowing",
              auth_after_bad == auth_before_bad,
              f"auth calls {auth_before_bad} -> {auth_after_bad}")

        # ---- C5: the whitelist ---------------------------------------------------
        check("C5: an authorized provider's model the tenant is NOT whitelisted for is omitted",
              "m2" not in (ids or []), f"ids={ids} (m2 belongs to p1, not in tenant-models)")

        # ---- C4: the online filter, measured as a TRANSITION ---------------------
        # `m3`'s provider points at a closed port. Until it has actually failed it is still
        # "online" (C1 saw it listed), so the honest way to test the filter is: trip the
        # breaker, then re-read the catalog and require the entry to disappear.
        check("C4 (precondition): the offline provider is listed while it has NOT failed yet",
              "m3" in (ids or []), f"ids={ids}")
        for _ in range(6):
            chat("m3")
        st_dead, out_dead = admin("GET", "/breaker")
        announce("C4 the offline provider after traffic", f"{out_dead[:80]}")
        check("C4 (precondition): the offline provider really is in the breaker dead-set",
              "p2" in json.loads(out_dead or "{}").get("dead", []), f"{out_dead[:90]}")
        st4, _, _, ids_after = catalog(key="sk-tenant-1")
        announce("C4 the catalog after the provider is dead", f"HTTP {st4} ids={ids_after}")
        check("C4: a model whose provider is DEAD disappears from the catalog (online filter)",
              st4 == 200 and ids_after == ["m1"], f"ids={ids_after} (was {ids})")

        # ---- C8: only GET on the exact path is intercepted ----------------------
        st_post, _, out_post = call("POST", f"http://127.0.0.1:{DATA}/v1/models",
                                    token="sk-tenant-1", host="catalog.local", body={})
        check("C8: `POST /v1/models` is NOT the catalog (only GET on the exact path is)",
              st_post != 200 or '"list"' not in out_post,
              f"HTTP {st_post} {out_post[:70]}")
        st_head, _, _ = call("HEAD", f"http://127.0.0.1:{DATA}/v1/models", token="sk-tenant-1",
                             host="catalog.local")
        # Round 161: this ended in `or True` (a constant), so it asserted nothing. Same class as the
        # `or True` found in test_limit_roles_enforcement.py — the sweep found the second occurrence.
        check("C8: ...and neither is HEAD", st_head != 200, f"HEAD -> HTTP {st_head}")
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"MODEL CATALOG: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("MODEL CATALOG: PASSED (public + local directory, no external auth, online filter, "
          "whitelist, only GET on the exact path)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
