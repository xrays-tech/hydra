#!/usr/bin/env python3
"""`limit_roles` ENFORCEMENT, black-box — plus the measured evidence for decision item D-11.

`ops.md` §4 documents the whole feature: roles live in the `limit_roles` table, each carries
`matching_*` dimensions (NULL = match-all), a `limit_count` and/or `limit_token` ceiling and a
`window` (`m`/`h`/`d`); "the matcher selects roles for a request; **the most restrictive
surviving role applies**"; a count breach is a `429` (and §4.2 promises `Retry-After` with the
remaining window); `limit_token` has "next-request" semantics; `enabled=false` soft-disables
a role; and counters are per-process (v1 limitation). Entity CRUD for `limit-roles` was
verified in the CLI drill (round 82) — **enforcement was never executed**.

Cases (throwaway node + a mock upstream that reports `usage`):
  L1 a matching role enforces `limit_count`: N pass, N+1 is 429 `rate_limited`, the metric
     `hydra_limit_rejected_total{tenant,role,dim="count"}` fires, and `Retry-After` is present
  L2 "the most restrictive surviving role applies": two overlapping roles -> the tighter wins
  L3 dimensions scope: a role for ANOTHER tenant does not touch this one; a role matching a
     different MODEL does not fire for this model
  L4 a soft-disabled role (`enabled=false`) matches nothing
  L5 `limit_token` fires on the NEXT request once the recorded usage crosses the ceiling
  L6 **D-11 evidence**: a role whose only dimension is `matching_provider` never fires —
     measured (the code says it "CANNOT match" because the pre-limit gate runs BEFORE routing,
     where no provider is chosen yet)
  L7 (round-120 promise): a key-scoped role with `matching_tenant` NULL makes the node WARN by
     name, and the window really is shared — t2's first request on the same key is refused by
     t1's usage; the control (same role scoped to t1) leaves t2 untouched and stays quiet
  L9 (where the config warnings appear): a role written through the admin API is warned about by the
     WRITE path itself (it reloads), not only at startup — and the MASK form is SILENT since D-15②
     (its "two keys share a window" cost is gone: the window is keyed by a digest of the raw key)
  L8 (one request, one config generation): a role rolled away WHILE a request is in flight and
     put back afterwards still accounts that request's usage (the restored role's token window
     refuses the next request) — before the fix the accounting phase re-read the store and the
     usage vanished

Run: python3 integration/test_limit_roles_enforcement.py    # needs target/debug/hydra
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
DIR = os.path.join(ROOT, ".acceptance", "limit-roles-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18840, 18841, 18849
TOKEN = "hydra-limits-admin-2026"
TOKENS = 13                      # the mock's reported total_tokens per response

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Upstream(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        raw = self.rfile.read(length) if length else b""
        # A model named `slow` answers after a delay, so a test can change the configuration WHILE
        # the request is in flight (the accounting gate runs in the `logging` hook, after this
        # response). Used by L8.
        try:
            if json.loads(raw or b"{}").get("model") == "slow":
                time.sleep(1.5)
        except Exception:
            pass
        if self.path.startswith("/auth"):
            body = json.dumps({"allowed": True, "expires_in": 300}).encode()
        else:
            body = json.dumps({"id": "chatcmpl-limits", "object": "chat.completion",
                               "choices": [{"index": 0,
                                            "message": {"role": "assistant", "content": "ok"},
                                            "finish_reason": "stop"}],
                               "usage": {"prompt_tokens": 5, "completion_tokens": 8,
                                         "total_tokens": TOKENS}}).encode()
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
            return r.status, dict(r.headers), r.read().decode(errors="replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode(errors="replace")
    except Exception as e:
        return 0, {}, str(e)


def admin(method, path, body=None):
    st, _, out = call(method, f"http://127.0.0.1:{ADMIN}/api/v1{path}", token=TOKEN, body=body)
    return st, out


def proxied(model="echo", tenant="t1", key="sk-tenant-1"):
    host = "limits.local" if tenant == "t1" else "limits2.local"
    return call("POST", f"http://127.0.0.1:{DATA}/v1/chat/completions", token=key, host=host,
                body={"model": model, "messages": [{"role": "user", "content": "hi"}]})


def metric(name):
    _, _, body = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
    return [l for l in body.splitlines() if l.startswith(name) and not l.startswith("#")]


def counter_value(lines, needle):
    """The value of the first counter line containing `needle` (absent ⇒ 0.0, unparsable ⇒ None)."""
    for line in lines:
        if needle in line:
            try:
                return float(line.rsplit(" ", 1)[1])
            except ValueError:
                return None
    return 0.0


def start_node():
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'limits.db')}?mode=rwc",
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


def role(rid, **kw):
    body = {"id": rid, "name": rid, "matching_tenant": None, "matching_key": None,
            "matching_model": None, "matching_provider": None, "limit_count": None,
            "limit_token": None, "window": "m", "enabled": True, "created_at": ""}
    body.update(kw)
    return body


def put_role(r):
    st, out = admin("POST", "/limit-roles", r)
    if st not in (200, 201):
        raise SystemExit(f"[limits] CANNOT VERIFY: creating role {r['id']} -> {st} {out[:160]}")
    admin("POST", "/reload", {})
    time.sleep(0.2)


def seed():
    for path, payload in (
        ("/providers", {"id": "p1", "key": "p1", "name": "P",
                        "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm1", "key": "echo", "name": "Echo", "provider_id": "p1",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm2", "key": "other", "name": "Other", "provider_id": "p1",
                              "status": 1, "created_at": "", "updated_at": ""}),
        # `slow` exists so L8 can hold a request open (the mock sleeps on this model name).
        ("/provider-models", {"id": "pm3", "key": "slow", "name": "Slow", "provider_id": "p1",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                            "created_at": ""}),
        ("/tenants", {"id": "t1", "name": "T1", "domain": "limits.local",
                      "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                      "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("/tenants", {"id": "t2", "name": "T2", "domain": "limits2.local",
                      "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                      "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp2", "tenant_id": "t2", "provider_id": "p1",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm3b", "tenant_id": "t1", "model_key": "slow",
                            "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm2", "tenant_id": "t1", "model_key": "other",
                            "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm3", "tenant_id": "t2", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
    ):
        st, out = admin("POST", path, payload)
        if st not in (200, 201):
            raise SystemExit(f"[limits] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")
    admin("POST", "/reload", {})
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[limits] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    node = start_node()
    try:
        if not wait_healthy():
            print("[limits] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "node.log")).read()[-600:], file=sys.stderr)
            return 2
        seed()

        # ---- L1: a matching role enforces limit_count --------------------------
        put_role(role("r-count", matching_tenant="t1", matching_model="echo", limit_count=2))
        codes = []
        headers_last = {}
        for _ in range(3):
            st, hdrs, _ = proxied("echo")
            codes.append(st)
            headers_last = hdrs
        rejected = metric("hydra_limit_rejected_total")
        announce("L1 three requests under limit_count=2", f"codes={codes}")
        announce("L1 metric", f"{rejected}")
        check("L1: the first N requests pass and the (N+1)-th is refused",
              codes[:2] == [200, 200] and codes[2] == 429, f"codes={codes}")
        # Round 161: this check used to end in `or True` (a constant) and read a header that does not
        # exist (`X-Noop`), so it passed whatever the product did. It now asserts the envelope the
        # refusal actually arrives in: `429` plus the JSON content type, with the body check below.
        # (The status comes from the loop above, not from `st429` — that variable is assigned by the
        # NEXT request; the header name is checked in both spellings because the drill reads headers
        # as a plain mapping.)
        ct = headers_last.get("Content-Type") or headers_last.get("content-type") or ""
        check("L1: the refusal is `429 rate_limited` in the data-plane envelope",
              codes[2] == 429 and "json" in ct.lower(),
              f"HTTP {codes[2]} content-type={ct!r}")
        st429, _, body429 = proxied("echo")
        check("L1: ...and the body names `rate_limited`",
              st429 == 429 and "rate_limited" in body429, f"HTTP {st429} {body429[:70]}")
        ra = headers_last.get("Retry-After") or headers_last.get("retry-after")
        check("L1: §4.2's `Retry-After` (remaining window) is present on the 429",
              ra is not None and ra.isdigit() and 0 < int(ra) <= 60, f"Retry-After={ra!r}")
        check("L1: `hydra_limit_rejected_total{tenant=\"t1\",role=\"r-count\",dim=\"count\"}` fires",
              any('role="r-count"' in l and 'dim="count"' in l and 'tenant="t1"' in l
                  for l in rejected), f"{rejected}")

        # ---- L2: the most restrictive of two MATCHING roles decides -------------
        # Round 165 rewrote this leg after MEASURING that the old one was green for the wrong reason.
        # It used to create `r-tight` (tenant-wide, limit 1) and assert that the next `echo` request is
        # 429 — but L1 had already spent `r-count`'s window, so the 429 came from `r-count`
        # (measured: `r-count` 2 -> 3, `r-tight` 0 -> 0), and a node that consulted only the FIRST
        # matching role, or the WIDEST one, would have answered exactly the same. The isolated design:
        #   · model `other`, which `r-count` does NOT match (it pins model `echo`),
        #   · TWO matching roles: `r-wide` (limit 1000, would allow everything) and `r-tight` (limit 1),
        #   · request 1 is admitted (r-tight's first sample), request 2 must be denied BY `r-tight`
        #     (r-wide would still allow it — that is what makes it "the strictest wins"),
        #   · control: with `r-tight` deleted the very next request is admitted again, so the deny is
        #     attributable to `r-tight` rather than to the window/ordering.
        put_role(role("r-wide", matching_tenant="t1", limit_count=1000))
        put_role(role("r-tight", matching_tenant="t1", limit_count=1))
        time.sleep(0.3)  # let the write-path reload land (the drill writes through the admin API)
        first = proxied("other")[0]
        before = sum(int(l.rsplit(" ", 1)[1]) for l in metric("hydra_limit_rejected_total")
                     if 'role="r-tight"' in l)
        second = proxied("other")[0]
        after = sum(int(l.rsplit(" ", 1)[1]) for l in metric("hydra_limit_rejected_total")
                    if 'role="r-tight"' in l)
        check("L2: the first request through two matching roles is admitted", first == 200,
              f"HTTP {first} (the WIDE role allows plenty; a deny here means ordering/luck)")
        check("L2: the stricter of two MATCHING roles refuses the second one", second == 429,
              f"HTTP {second} (the wide role would have allowed it)")
        check("L2: ...and the refusal is ATTRIBUTED to the stricter role", after > before,
              f"r-tight rejections {before} -> {after} (a deny counted elsewhere means the widest "
              f"or the first role decided)")
        admin("DELETE", "/limit-roles/r-tight")
        admin("POST", "/reload", {})
        time.sleep(0.3)
        control = proxied("other")[0]
        check("L2 CONTROL: with the stricter role gone the same request is admitted again",
              control == 200, f"HTTP {control} (429 here means the deny was not r-tight's)")
        # Retire the wide role too: the legs below assume the fixture they had before this one ran.
        admin("DELETE", "/limit-roles/r-wide")
        admin("POST", "/reload", {})
        time.sleep(0.2)

        # ---- L3: dimensions scope ---------------------------------------------
        st_t2, _, _ = proxied("echo", tenant="t2")
        check("L3: a role matching tenant t1 does not touch t2",
              st_t2 == 200, f"HTTP {st_t2}")
        # r-count matches model `echo`; a different model of the same tenant must not match it.
        # (r-tight above matches the whole tenant, so retire it first.)
        st, _ = admin("DELETE", "/limit-roles/r-tight")
        admin("POST", "/reload", {})
        time.sleep(0.2)
        codes_other = [proxied("other")[0] for _ in range(3)]
        check("L3: a role matching model `echo` does not fire for model `other`",
              codes_other == [200, 200, 200], f"codes={codes_other}")

        # ---- L4: a soft-disabled role matches nothing --------------------------
        st, _ = admin("DELETE", "/limit-roles/r-count")
        admin("POST", "/reload", {})
        time.sleep(0.2)
        put_role(role("r-off", matching_tenant="t1", limit_count=1, enabled=False))
        codes_off = [proxied("echo")[0] for _ in range(3)]
        check("L4: `enabled=false` soft-disables the role (it is listed but never matched)",
              codes_off == [200, 200, 200], f"codes={codes_off}")
        st, out = admin("GET", "/limit-roles/r-off")
        check("L4: ...and it is still listed (soft-disable, not delete)", st == 200, f"HTTP {st}")
        admin("DELETE", "/limit-roles/r-off")
        admin("POST", "/reload", {})
        time.sleep(0.2)

        # ---- L5: limit_token has next-request semantics ------------------------
        # The ceiling is just above ONE response's usage, so request 1 passes, request 2
        # crosses it, and request 3 is the one refused.
        put_role(role("r-token", matching_tenant="t1", limit_token=TOKENS + 1))
        # Snapshot BEFORE the three requests (round 174): the leg below claims the refusal "is counted"
        # with the real `dim` label. Existence alone cannot show that THIS role produced the number —
        # it cannot fail when the value was already there (the round-173 C2 shape, same fix).
        tok_before = counter_value(metric("hydra_limit_rejected_total"), 'role="r-token"')
        codes_tok = [proxied("echo")[0] for _ in range(3)]
        tok_metric = [l for l in metric("hydra_limit_rejected_total") if 'r-token' in l]
        tok_after = counter_value(tok_metric, 'role="r-token"')
        announce("L5 limit_token ceiling = one response's usage + 1", f"codes={codes_tok}")
        check("L5: the request that would exceed the ceiling is the one refused (next-request "
              "semantics)", codes_tok[:2] == [200, 200] and codes_tok[2] == 429, f"codes={codes_tok}")
        # The label VALUE is `tokens` (plural) — measured, and now documented next to the
        # metric: a rule written on `dim="token"` would never fire.
        check("L5: ...and it is counted with the metric's real `dim` value (`tokens`) — and as an "
              "INCREMENT on this role's series, so the label value AND the count are both pinned",
              any('dim="tokens"' in l for l in tok_metric)
              and tok_before is not None and tok_after is not None and tok_after > tok_before,
              f"{tok_metric} (r-token series {tok_before} -> {tok_after})")
        st_tok, hdrs_tok, _ = proxied("echo")
        ra_tok = hdrs_tok.get("Retry-After") or hdrs_tok.get("retry-after")
        check("L5: the TOKEN refusal carries `Retry-After` too (both gates, §4.2)",
              st_tok == 429 and ra_tok is not None and ra_tok.isdigit(), f"HTTP {st_tok} Retry-After={ra_tok!r}")
        admin("DELETE", "/limit-roles/r-token")
        admin("POST", "/reload", {})
        time.sleep(0.2)

        # ---- L6: D-11 evidence — matching_provider never fires -----------------
        put_role(role("r-prov", matching_provider="p1", limit_count=1))
        codes_prov = [proxied("echo")[0] for _ in range(3)]
        log = open(os.path.join(DIR, "node.log"), errors="replace").read()
        announce("L6 a role whose only dimension is matching_provider", f"codes={codes_prov}")
        announce("L6 the node's own warning",
                 next((l for l in log.splitlines() if "matching_provider" in l), "<none>")[:150])
        check("L6 (D-11): such a role does NOT enforce anything — the gate runs before routing, "
              "where no provider has been chosen yet",
              codes_prov == [200, 200, 200], f"codes={codes_prov}")
        warn6 = next((l for l in log.splitlines() if "matching_provider" in l), None)
        check("L6 (D-11): ...and the node says so at startup/config load",
              warn6 is not None, f"warning line: {warn6[:120] if warn6 else '<none found>'}")
        # ---- L7: a key-scoped role with NO tenant scope is a CROSS-TENANT budget ----
        # Round 120 added a startup warning for this and `ops.md` §4 now states the consequence
        # ("its window is shared by EVERY tenant that accepts that key"). Both halves are measured
        # here: the node must say it, and the sharing must actually happen. The control (same role
        # WITH `matching_tenant`) proves the sharing comes from the missing scope, not from
        # something else in the fixture.
        shared_key = "sk-cross-tenant-probe"
        put_role(role("r-cross", matching_key=shared_key, limit_count=1))
        log = open(os.path.join(DIR, "node.log"), errors="replace").read()
        announce("L7 the node's own warning about the missing tenant scope",
                 next((l for l in log.splitlines() if "matching_tenant NULL" in l), "<none>")[:160])
        warn7 = next((l for l in log.splitlines()
                      if "matching_tenant NULL" in l and "r-cross" in l), None)
        # Round 135: the VALUE of `matching_key` is warned about too — here the role carries a raw
        # client key, which lives in a plaintext column and is returned by the admin API (P3-4).
        warn_raw = next((l for l in log.splitlines()
                         if "RAW client key" in l and "r-cross" in l), None)
        announce("L7 the node's warning about the VALUE (a raw key in a plaintext column)",
                 (warn_raw or "<none>")[:150])
        check("L7: a role whose `matching_key` is a RAW key is named as a plaintext credential",
              warn_raw is not None, f"warning line: {warn_raw[:120] if warn_raw else '<none found>'}")
        check("L7: the node warns, BY NAME, that a key-scoped role with matching_tenant NULL "
              "shares one budget across tenants",
              warn7 is not None, f"warning line: {warn7[:120] if warn7 else '<none found>'}")
        cross = [proxied("echo", tenant="t1", key=shared_key)[0],
                 proxied("echo", tenant="t2", key=shared_key)[0]]
        announce("L7 the documented consequence, t1 then t2 with the SAME key", f"codes={cross}")
        check("L7: ...and t2's FIRST request is refused by t1's spent window (the documented "
              "cross-tenant consequence)", cross == [200, 429], f"codes={cross}")

        # CONTROL: the same role scoped to t1 must not touch t2 — even on the very same key shape.
        scoped_key = "sk-scoped-tenant-probe"
        put_role(role("r-scoped", matching_key=scoped_key, matching_tenant="t1", limit_count=1))
        ctl = [proxied("echo", tenant="t1", key=scoped_key)[0],
               proxied("echo", tenant="t1", key=scoped_key)[0],
               proxied("echo", tenant="t2", key=scoped_key)[0]]
        log2 = open(os.path.join(DIR, "node.log"), errors="replace").read()
        announce("L7 CONTROL the same role WITH matching_tenant=t1", f"codes={ctl}")
        leaked = [l for l in log2.splitlines() if "matching_tenant NULL" in l and "r-scoped" in l]
        check("L7 CONTROL: a tenant-scoped role leaves the other tenant alone, and produces no "
              "cross-tenant warning",
              ctl == [200, 429, 200] and not leaked,
              f"codes={ctl}, warnings about r-scoped: {len(leaked)}")
        # ---- L9: WHERE the config warnings appear (measured, for product-review P3-2) -------
        # The review said the warning only exists at startup. Measured here: the admin WRITE path calls
        # `reload_best_effort`, so a bad role is warned about the moment it is written — no explicit
        # `/reload` needed. This leg writes WITHOUT one on purpose, so the assertion can only pass if
        # the write path itself validated.
        before_len = len(open(os.path.join(DIR, "node.log"), errors="replace").read())
        st_noreload, _ = admin("POST", "/limit-roles",
                               role("r-nowarn-probe", matching_key="sk-raw-without-reload"))
        log_after_write = open(os.path.join(DIR, "node.log"), errors="replace").read()[before_len:]
        announce("L9 a role written WITHOUT an explicit reload", f"POST -> {st_noreload}")
        check("L9: the write itself warns about a raw key (no explicit /reload needed)",
              st_noreload in (200, 201)
              and "r-nowarn-probe" in log_after_write
              and "RAW client key" in log_after_write,
              f"POST={st_noreload}, new log: {log_after_write.strip()[-160:] or '<nothing>'}")
        # ...and the MASK form must now be SILENT (decision D-15②, 2026-10-08). It used to warn that
        # "any other key whose mask is that same string shares this window", which was true while the
        # window was `(role_id, mask(key))`. The window is keyed by a digest of the raw key now, so two
        # keys that share a mask have two windows — the warning's condition cannot occur and it was
        # deleted with its premise. Asserting the ABSENCE is what keeps it from being re-added by
        # someone reading an old note.
        before_mask = len(open(os.path.join(DIR, "node.log"), errors="replace").read())
        st_mask = admin("POST", "/limit-roles",
                        role("r-maskform-probe", matching_key="sk***************ed", matching_tenant="t1"))[0]
        log_after_mask = open(os.path.join(DIR, "node.log"), errors="replace").read()[before_mask:]
        mask_line = next((l for l in log_after_mask.splitlines()
                          if "r-maskform-probe" in l and "MASKED form" in l), None)
        announce("L9 the mask form's log (must be empty)",
                 (mask_line or "<nothing>")[:150])
        check("L9: the MASK form is NOT warned about any more — its shared-window cost is FIXED "
              "(D-15② keys the window by a digest of the key, not by the mask)",
              st_mask in (200, 201) and mask_line is None,
              f"POST={st_mask}, log: {mask_line[:120] if mask_line else '<nothing, as expected>'}")
        admin("DELETE", "/limit-roles/r-nowarn-probe")
        admin("DELETE", "/limit-roles/r-maskform-probe")
        admin("POST", "/reload", {})

        # ---- L8: ONE REQUEST, ONE CONFIG GENERATION (the mid-flight write) ----------------
        # The count/token gates read the snapshot taken when the request arrived; the usage
        # accounting runs in the `logging` hook, AFTER the upstream answered. It used to re-read
        # `store.snapshot()` there, so an admin write during the request made the two phases
        # disagree: a role deleted mid-request had its count sample spent and never received the
        # tokens (the limiter documents "the request is always counted"), and tokens could be
        # charged to a role that never gated the request. Redis paths additionally got each
        # generation's `window_ms` in turn, so samples could be evicted early or kept too long.
        #
        # Observable difference: roll the role away mid-request and put it back, then let the
        # token gate speak. With the fix, request 1's usage (13 tokens) is in the SAME window the
        # restored role reads => request 2 is refused by `limit_token=1`. Without it the window is
        # empty and request 2 sails through.
        put_role(role("r-inflight", matching_tenant="t1", limit_count=1000, limit_token=1))
        inflight = {}

        def fire():
            inflight["r1"] = proxied("slow")

        t = threading.Thread(target=fire)
        t.start()
        started = time.time()
        time.sleep(0.4)  # the mock is sleeping on `slow`
        # Round 165: the whole leg rests on "request 1 is STILL IN FLIGHT when the role is deleted"
        # (the mock sleeps 1.5 s on `slow`), and that premise was never asserted — if the delay ever
        # stopped happening, request 1 would finish before the DELETE, the accounting would land while
        # the role still exists, and request 2 would be refused anyway: the leg would stay green with
        # the bug it exists to catch back in place. Assert the premise.
        still_in_flight = t.is_alive()
        check("L8 PREMISE: request 1 is still in flight while the role is deleted mid-request "
              "(the mock sleeps on `slow`)", still_in_flight,
              f"thread alive after 0.4s = {still_in_flight} (mock delay is 1.5s on `slow`)")
        admin("DELETE", "/limit-roles/r-inflight")
        admin("POST", "/reload", {})
        t.join(timeout=20)
        elapsed1 = time.time() - started
        check("L8 PREMISE: ...and it really was the slow upstream (it took the mock's 1.5s, not a "
              "fast path)", elapsed1 >= 1.0, f"request 1 took {elapsed1:.2f}s")
        put_role(role("r-inflight", matching_tenant="t1", limit_count=1000, limit_token=1))
        r2 = proxied("echo")
        announce("L8 request 1 (slow upstream) while the role is deleted mid-flight",
                 f"code={inflight.get('r1', ('?',))[0]}")
        announce("L8 request 2 after the role is restored", f"code={r2[0]}")
        check("L8: the in-flight request is admitted (its gates ran before the write)",
              inflight.get("r1", (0,))[0] == 200, f"code={inflight.get('r1', ('?',))[0]}")
        check("L8: ...and its usage was accounted against the generation its gates used, so the "
              "restored role's token window sees it (request 2 is refused by limit_token=1)",
              r2[0] == 429, f"code={r2[0]} (200 means the mid-flight write split the two phases)")
    finally:
        stop(node)
        upstream.shutdown()

    print()
    if failures:
        print(f"LIMIT ROLES: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("LIMIT ROLES: PASSED (count + token ceilings, Retry-After, most-restrictive rule, "
          "dimension scoping, soft-disable, the measured D-11 finding, the round-120 cross-tenant "
          "warning + its documented consequence, one-config-generation-per-request, and where the "
          "config warnings appear (write path included)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
