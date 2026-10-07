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

        # ---- C6: what a presented key does to the listing ------------------------
        # Two DIFFERENT binding mechanisms narrow the catalog, and they are not the same:
        #   (3.5) an operator `provider-key-bindings` row  -> keep only that provider
        #   (3.6) a sub-tenant whose route covers THIS model -> keep that route's provider,
        #         while a model the sub-tenant has no route for is NOT restricted (the
        #         documented "routes are overrides; otherwise the tenant default applies").
        st_opb, out_opb = admin("POST", "/provider-key-bindings",
                                {"id": "opb1", "key_prefix": "OPB_", "provider_id": "p1",
                                 "enabled": True, "created_at": "", "updated_at": ""})
        st_list, out_list = admin("GET", "/provider-key-bindings")
        announce("C6 the operator binding", f"create HTTP {st_opb} {out_opb[:90]} | list {out_list[:120]}")
        check("C6 (precondition): the operator key-prefix binding exists in the snapshot",
              st_opb in (200, 201) and "OPB_" in out_list, f"HTTP {st_opb}")
        admin("POST", "/reload", {})
        time.sleep(0.3)
        st_op, _, _, ids_op = catalog(key="OPB_xyz789")
        announce("C6 an operator prefix binding (OPB_ -> p1)", f"HTTP {st_op} ids={ids_op}")
        check("C6: an operator key-prefix binding narrows the whole listing to its provider",
              st_op == 200 and ids_op == ["m1"],
              f"HTTP {st_op} ids={ids_op} (anonymous {ids}; m3 is served only by p2)")

        st, out = admin("POST", "/sub-tenants",
                        {"id": "steer", "tenant_id": "t1", "name": "steer",
                         "key_prefix": SUB_PREFIX, "enabled": True, "created_at": ""})
        st_route, out_route = admin("POST", "/sub-tenant-routes",
                                    {"id": "str1", "sub_tenant_id": "steer", "model_key": "m1",
                                     "provider_id": "p1", "enabled": True, "created_at": "",
                                     "updated_at": ""})
        # PREMISE, asserted (round 172). The sub-tenant half of C6 used to reduce to
        # "`m1` is in the listing and `m3` is in the listing" — and m1/m3 ARE the anonymous set, so
        # both assertions held for every listing that contains them, including the anonymous one: if
        # the sub-tenant or its route had never been created (or never reached the snapshot), the legs
        # passed while proving nothing. The operator half above carries exactly this kind of
        # precondition check; the sub-tenant half did not.
        st_read, out_read = admin("GET", "/sub-tenant-routes")
        read_back = "str1" in (out_read or "")
        check("C6 (precondition): the sub-tenant AND its route really exist (the route is read back "
              "from the admin API, not assumed from a 2xx)",
              st in (200, 201) and st_route in (200, 201) and st_read == 200 and read_back,
              f"sub-tenant -> {st} {out[:60]!r}; route -> {st_route} {out_route[:60]!r}; "
              f"GET /sub-tenant-routes -> {st_read} contains str1={read_back}")
        admin("POST", "/reload", {})
        time.sleep(0.3)
        st_sub, _, _, ids_sub = catalog(key=SUB_PREFIX + "abc123")
        # CONTROL: a key whose prefix matches NO sub-tenant. `design.md` §632 documents `/v1/models`
        # as readable with OR without a key, narrowed only by prefixes it knows — so this key must see
        # the full anonymous listing. Without this control, "the route narrowed the listing" and
        # "every key sees everything" are the same observation.
        st_unk, _, out_unk, ids_unk = catalog(key="NOPE_abc123")
        announce("C6 a sub-tenant key and an unknown-prefix key",
                 f"sub={ids_sub} unknown={ids_unk} anon={ids}")
        check("C6 (control): a key whose prefix matches NO sub-tenant sees the FULL anonymous "
              "catalog (the endpoint identifies no tenant from the key, so an unknown prefix narrows "
              "nothing)",
              st_unk == 200 and ids_unk == ids, f"HTTP {st_unk} ids={ids_unk} anon={ids}")
        # The route above covers `m1` only, and p1 genuinely serves m1 ⇒ m1 stays; m3 has NO route ⇒
        # the documented per-model fallback applies and the listing equals the anonymous one. This
        # state alone cannot distinguish "the gate is active" from "no gate", which is exactly why the
        # next step adds the discriminating route.
        check("C6: with a route covering only `m1`, the sub-tenant key sees the tenant's catalog "
              "UNCHANGED — the documented per-model fallback (`无命中 ⇒ 行为与今天完全一致`), not a "
              "whitelist",
              st_sub == 200 and ids_sub == ids, f"HTTP {st_sub} ids={ids_sub} anon={ids}")
        # The WRITE boundary is fail-closed too, and that is measured here rather than assumed: a
        # route pinning a model to a provider that does NOT serve it is REFUSED, so the "empty
        # intersection" the catalog mirror drops on cannot be created directly — it can only arise
        # from a LATER change (C6b below builds it with a breaker-dead provider).
        st_bad, out_bad = admin("POST", "/sub-tenant-routes",
                                {"id": "str-bad", "sub_tenant_id": "steer", "model_key": "m3",
                                 "provider_id": "p1", "enabled": True, "created_at": "",
                                 "updated_at": ""})
        check("C6: the write path REFUSES a route pinning a model to a provider that does not serve "
              "it (`fail-closed` at the boundary, so the broken fixture cannot be written)",
              st_bad == 400, f"HTTP {st_bad} {out_bad[:90]}")
        check("C6 (security): neither binding can WIDEN the anonymous union",
              set(ids_op or []) <= set(ids or []) and set(ids_sub or []) <= set(ids or []),
              f"op={ids_op} sub={ids_sub} anonymous={ids}")

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

        # ---- C6b: the sub-tenant CATALOG MIRROR, made observable (round 172) --------------------
        # `design.md` §7.1c promises the catalog mirrors the sub-tenant gate ("fail-closed 求交：命中
        # 一条启用路由 ⇒ 候选集 ∩= {route.provider_id}，空 ⇒ 503"), and `router::accessible_models`
        # implements the empty intersection as "drop the model". Observed only by constructing the
        # empty set the way production does, because the write path refuses the direct fixture (C6
        # measured that 400): give `m3` a SECOND provider (p1), pin the route to the other one (p2),
        # and let p2 be breaker-DEAD — which the C4 legs just produced. The anonymous view then keeps
        # `m3` through p1 while the sub-tenant key must lose it; without this construction the mirror
        # was untestable and the older `in`-assertions could not fail.
        st_pm, out_pm = admin("POST", "/provider-models",
                              {"id": "pm3b", "key": "m3", "name": "M3 via p1", "provider_id": "p1",
                               "status": 1, "created_at": "", "updated_at": ""})
        st_route2, out_route2 = admin("POST", "/sub-tenant-routes",
                                      {"id": "str2", "sub_tenant_id": "steer", "model_key": "m3",
                                       "provider_id": "p2", "enabled": True, "created_at": "",
                                       "updated_at": ""})
        admin("POST", "/reload", {})
        time.sleep(0.3)
        st_sub2, _, _, ids_sub2 = catalog(key=SUB_PREFIX + "abc123")
        st_unk2, _, _, ids_unk2 = catalog(key="NOPE_abc123")
        st_anon2, _, _, ids_anon2 = catalog(key="sk-tenant-1")   # re-read: `ids_after` is stale here
        st_m3, _, out_m3 = chat("m3", SUB_PREFIX + "abc123")
        announce("C6b after giving `m3` a second provider and pinning the sub-tenant to the dead one",
                 f"provider-model={st_pm} route={st_route2} sub={ids_sub2} unknown={ids_unk2} "
                 f"tenant={ids_anon2}; call m3 with the sub-tenant key -> HTTP {st_m3} {out_m3[:60]!r}")
        check("C6b (premise): both writes were accepted — a route may name the DEAD provider because "
              "breaker state is runtime, not config",
              st_pm in (200, 201) and st_route2 in (200, 201),
              f"provider-model -> {st_pm} {out_pm[:60]!r}; route -> {st_route2} {out_route2[:60]!r}")
        check("C6b: the mirror is REAL — pinning `m3` to the breaker-dead p2 empties its provider set, "
              "so the sub-tenant key DROPS m3 while the anonymous and unknown-prefix keys keep it "
              "through p1",
              ids_sub2 == ["m1"] and "m3" in (ids_unk2 or []) and "m3" in (ids_anon2 or []),
              f"sub={ids_sub2} unknown={ids_unk2} tenant={ids_anon2}")
        check("C6b: ...and the CALL path empties the same intersection (`503`, the documented "
              "fail-closed answer, not a fallback to the other provider)",
              st_m3 == 503, f"HTTP {st_m3} {out_m3[:90]!r}")

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
          "whitelist, prefix narrowing never widens, only GET on the exact path)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
