#!/usr/bin/env python3
"""The ClickHouse sink's WIRE format — the documented `HYDRA_CLICKHOUSE_URL` capabilities.

`ops.md` §1.1 documents three things about that URL, and the module header of
`clickhouse.rs` says it "recorded rather than ignored:" the credentials and the query string
used to be STRIPPED, so an operator pointing Hydra at an authenticated ClickHouse had a
connection that silently could not write (the failure only shows up as dropped usage):

  * `http://user:pass@host:8123` — the userinfo becomes `Authorization: Basic …`;
  * any query string (`?database=dogress`, `?user=x&password=y`) is passed through verbatim;
  * the INSERT travels as a `query=` parameter, and the ROW VALUES travel in the request
     BODY as `FORMAT JSONEachRow` (measured 2026-09-30; the `param_*` binding form in
     `clickhouse.rs` is used by the READER — `usage_query.rs` — not by this writer).

Cases (a mock stand-in for ClickHouse, so no real database is touched):
  W1 userinfo -> `Authorization: Basic <base64(user:pass)>`, and the passthrough query
     parameter survives on the wire
  W2 the `query=` parameter is an `INSERT INTO usage_record` naming the documented columns
     and ending in `FORMAT JSONEachRow`, while the request BODY carries that one row as JSON
     — with `tokens_in`/`tokens_out` as JSON numbers and `client_api_key` MASKED (the raw
     tenant key must never reach the database)
  W3 `?user=x&password=y` (no userinfo) is passed through and NO Authorization header appears
  W3b a URL that carries a PATH (`http://host:8123/clickhouse`) is trimmed, not glued to the
     port — before the 2026-09-30 fix that attempt failed at connect with
     `invalid port value`, i.e. a network-sounding error for a URL-parsing bug
  W4 a 500 from ClickHouse does NOT break the client: the proxied request still answers 200,
     the node logs `usage sink batch insert failed`, and the sink retries

Run: python3 integration/test_clickhouse_sink_wire.py     # needs target/debug/hydra
The binary must be built with the `usage-clickhouse` feature (a `server`-only binary refuses
`HYDRA_USAGE_SINK=clickhouse` at startup and this exits 2):
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import base64
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "clickhouse-wire-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18890, 18891, 18899
CH_PORT_A, CH_PORT_B = 18897, 18896
# Each leg gets its OWN mock port. `shutdown()` stops the loop but does NOT close the
# listening socket, so two legs sharing a port died with `OSError: Address already in
# use` — a harness bug that hid W3/W4 (they never ran). Distinct ports + `server_close`
# make each leg independent.
CH_PORT_C, CH_PORT_D = 18895, 18894
TOKEN = "hydra-ch-wire-admin-2026"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def close_server(server):
    """`shutdown()` alone leaves the listening socket BOUND; `server_close()` frees it."""
    server.shutdown()
    server.server_close()


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


class Received:
    """Whatever the node sent to the mock ClickHouse."""

    def __init__(self):
        self.lock = threading.Lock()
        self.requests = []          # (method, path, headers, body)
        self.status = 200

    def record(self, method, path, headers, body):
        with self.lock:
            self.requests.append((method, path, headers, body))

    def count(self):
        with self.lock:
            return len(self.requests)

    def last(self):
        with self.lock:
            return self.requests[-1] if self.requests else None


def make_ch_mock(received):
    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length).decode(errors="replace") if length else ""
            received.record("POST", self.path, {k.lower(): v for k, v in self.headers.items()}, body)
            self.send_response(received.status)
            self.send_header("Content-Length", "0")
            self.end_headers()

        def log_message(self, *a):
            pass

    return H


class Upstream(BaseHTTPRequestHandler):
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
            body = json.dumps({"id": "chatcmpl-wire", "object": "chat.completion",
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


def start_node(label, ch_url, admin=ADMIN, data=DATA):
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin}",
        "HYDRA_LISTEN": f"127.0.0.1:{data}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_USAGE_SINK": "clickhouse",
        "HYDRA_CLICKHOUSE_URL": ch_url,
        "RUST_LOG": "info",
    })
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(admin=ADMIN, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if call("GET", f"http://127.0.0.1:{admin}/api/v1/health", token=TOKEN)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed(admin=ADMIN):
    for path, payload in (
        ("/providers", {"id": "p1", "key": "p1", "name": "P",
                        "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                        "created_at": "", "updated_at": ""}),
        ("/provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                              "status": 1, "created_at": "", "updated_at": ""}),
        ("/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                            "created_at": ""}),
        ("/tenants", {"id": "t1", "name": "T", "domain": "wire.local",
                      "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth", "enabled": True,
                      "cert_key": None, "cert_file": None, "created_at": "", "updated_at": ""}),
        ("/tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                               "created_at": "", "updated_at": ""}),
        ("/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                            "created_at": "", "updated_at": ""}),
    ):
        st, out = call("POST", f"http://127.0.0.1:{admin}/api/v1{path}", token=TOKEN, body=payload)
        if st not in (200, 201):
            raise SystemExit(f"[ch-wire] CANNOT VERIFY: seeding {path} -> {st} {out[:160]}")
    call("POST", f"http://127.0.0.1:{admin}/api/v1/reload", token=TOKEN, body={})
    time.sleep(0.3)


def proxied(data=DATA):
    # The Host header selects the tenant (`resolve_tenant`); without it every request is a
    # `404 unknown_domain` and the sink never sees a row (the same harness bug this session
    # has hit before).
    return call("POST", f"http://127.0.0.1:{data}/v1/chat/completions", token="sk-tenant-1",
                host="wire.local",
                body={"model": "echo", "messages": [{"role": "user", "content": "hi"}]})


def await_flush(received, want_at_least=1, budget=20.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if received.count() >= want_at_least:
            return True
        time.sleep(0.5)
    return False


def query_params(path):
    """The `query=` value and the other passthrough params, from a raw request target."""
    qs = path.split("?", 1)[1] if "?" in path else ""
    parsed = urllib.parse.parse_qs(qs, keep_blank_values=True)
    return parsed, qs


def main():
    if not os.path.exists(BIN):
        print(f"[ch-wire] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    # ---- W1/W2/W3: credentials + passthrough --------------------------------
    received_a = Received()
    ch_a = ThreadingHTTPServer(("127.0.0.1", CH_PORT_A), make_ch_mock(received_a))
    ch_a.daemon_threads = True
    threading.Thread(target=ch_a.serve_forever, daemon=True).start()
    node_a = start_node("wire_a", f"http://chuser:chpass@127.0.0.1:{CH_PORT_A}/?database=dogress")
    try:
        if not wait_healthy():
            print("[ch-wire] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "wire_a.log")).read()[-600:], file=sys.stderr)
            return 2
        seed()
        st, body = proxied()
        check("W0: the proxied request succeeds while the clickhouse sink is configured",
              st == 200, f"HTTP {st} {body[:60]}")
        if not await_flush(received_a):
            check("W1: the sink actually sent something to ClickHouse", False,
                  f"no request after 20s; log tail: "
                  f"{open(os.path.join(DIR, 'wire_a.log'), errors='replace').read()[-200:]}")
            return 1
        method, path, headers, reqbody = received_a.last()
        parsed, qs = query_params(path)
        announce("W1/W2 the request the sink sent",
                 f"{method} {path[:90]}… auth={headers.get('authorization')}")
        expected_auth = "Basic " + base64.b64encode(b"chuser:chpass").decode()
        check("W1: userinfo in the URL becomes `Authorization: Basic <base64(user:pass)>`",
              headers.get("authorization") == expected_auth,
              f"got {headers.get('authorization')!r} want {expected_auth!r}")
        check("W1: the passthrough query string survives verbatim (`database=dogress`)",
              parsed.get("database") == ["dogress"], f"parsed={ {k: v for k, v in parsed.items() if k != 'query'} }")
        sql = (parsed.get("query") or [""])[0]
        announce("W2 the SQL", f"{sql[:150]}…")
        check("W2: the INSERT names `usage_record` and the documented columns",
              "INSERT INTO usage_record" in sql
              and all(c in sql for c in ("tenant_id", "provider_id", "model_key", "tokens_in",
                                         "tokens_out", "client_api_key")),
              f"{sql[:120]}")
        check("W2: ...and asks ClickHouse for `FORMAT JSONEachRow`",
              "FORMAT JSONEachRow" in sql, f"{sql[-60:]}")
        announce("W2 the raw request", f"path={path[:160]} | body={reqbody[:200]!r}")
        # The measured shape (2026-09-30): the values are NOT `param_*` query parameters —
        # they are one JSON object per row in the request body. Pin the measured shape so a
        # future switch to bindings is a deliberate change that turns this leg red.
        params = {k: v[0] for k, v in parsed.items() if k.startswith("param_")}
        rows = [json.loads(ln) for ln in reqbody.splitlines() if ln.strip()]
        check("W2: exactly one JSON row travels in the BODY (no `param_*` bindings)",
              len(rows) == 1 and not params, f"rows={len(rows)} params={params}")
        row = rows[0] if rows else {}
        announce("W2 the row", f"{json.dumps(row, sort_keys=True)}")
        check("W2: the row carries the seeded tenant / provider / model",
              row.get("tenant_id") == "t1" and row.get("provider_id") == "p1"
              and row.get("model_key") == "echo",
              f"tenant_id={row.get('tenant_id')!r} provider_id={row.get('provider_id')!r} "
              f"model_key={row.get('model_key')!r}")
        # JSON NUMBERS, not strings: ClickHouse's `JSONEachRow` would refuse `"5"` for a
        # UInt64 column, so a regression here is a refused INSERT, not a silent 0.
        check("W2: the token counts are JSON numbers with the upstream's values",
              row.get("tokens_in") == 5 and row.get("tokens_out") == 8,
              f"tokens_in={row.get('tokens_in')!r} tokens_out={row.get('tokens_out')!r}")
        # The tenant's own key must never be written in the clear — `mask_key` for an
        # 11-character key (`sk-tenant-1`) keeps the first 2 and last 2.
        check("W2: `client_api_key` is MASKED and the raw tenant key is absent",
              row.get("client_api_key") == "sk*******-1"
              and "sk-tenant-1" not in reqbody,
              f"client_api_key={row.get('client_api_key')!r}")
    finally:
        stop(node_a)
        close_server(ch_a)

    # ---- W3b: a URL that carries a PATH is normalised (measured 2026-09-30) --
    # `http://host:8123/clickhouse` used to keep the path, so the port parse failed and the
    # sink logged `invalid port value` — while `clickhouse.rs`'s own comment claimed paths
    # were trimmed. The parser now really trims it (unit-tested); this leg proves it on the
    # wire, because that is where an operator would see it.
    received_d = Received()
    ch_d = ThreadingHTTPServer(("127.0.0.1", CH_PORT_B), make_ch_mock(received_d))
    ch_d.daemon_threads = True
    threading.Thread(target=ch_d.serve_forever, daemon=True).start()
    node_d = start_node("wire_d", f"http://127.0.0.1:{CH_PORT_B}/clickhouse?database=dogress",
                        admin=ADMIN + 30, data=DATA + 30)
    try:
        if not wait_healthy(ADMIN + 30):
            check("W3b: the path-URL node became healthy", False, "never healthy")
        else:
            seed(ADMIN + 30)
            proxied(DATA + 30)
            if await_flush(received_d):
                method, path, headers, _ = received_d.last()
                parsed, _ = query_params(path)
                log_d = open(os.path.join(DIR, "wire_d.log"), errors="replace").read()
                announce("W3b the path-URL sink request", f"{method} {path[:70]}…")
                check("W3b: a URL with a path now WORKS (no `invalid port value`)",
                      "invalid port value" not in log_d and parsed.get("database") == ["dogress"],
                      f"path={path[:60]}")
            else:
                # Name the CAUSE, not just the symptom: with the path kept, the node logs
                # `clickhouse connect 127.0.0.1:PORT/clickhouse: invalid port value` and never
                # reaches the mock. (Verified by reverting the trim: this leg goes red.)
                tail = [l for l in open(os.path.join(DIR, "wire_d.log"), errors="replace").read()
                        .splitlines() if "clickhouse" in l.lower()]
                check("W3b: a URL with a path now WORKS (no `invalid port value`)", False,
                      f"the sink never reached the mock; clickhouse log lines: "
                      f"{(tail[-1] if tail else '<none>')[:180]}")
    finally:
        stop(node_d)
        close_server(ch_d)

    # ---- W3: the query-parameter credential form ----------------------------
    received_b = Received()
    ch_b = ThreadingHTTPServer(("127.0.0.1", CH_PORT_C), make_ch_mock(received_b))
    ch_b.daemon_threads = True
    threading.Thread(target=ch_b.serve_forever, daemon=True).start()
    node_b = start_node("wire_b", f"http://127.0.0.1:{CH_PORT_C}/?user=quser&password=qpass",
                        admin=ADMIN + 10, data=DATA + 10)
    try:
        if not wait_healthy(ADMIN + 10):
            check("W3: the second node became healthy", False, "never healthy")
        else:
            seed(ADMIN + 10)
            proxied(DATA + 10)
            if await_flush(received_b):
                method, path, headers, reqbody = received_b.last()
                parsed, _ = query_params(path)
                announce("W3 the query-credential request", f"{method} {path[:80]}… "
                                                             f"auth={headers.get('authorization')}")
                check("W3: `?user=&password=` are passed through as query parameters",
                      parsed.get("user") == ["quser"] and parsed.get("password") == ["qpass"],
                      f"{ {k: v for k, v in parsed.items() if k in ('user', 'password')} }")
                check("W3: ...and no `Authorization` header is invented",
                      headers.get("authorization") is None, f"{headers.get('authorization')!r}")
            else:
                check("W3: the sink sent a request for the query-credential URL", False,
                      "nothing arrived in 20s")
    finally:
        stop(node_b)
        close_server(ch_b)

    # ---- W4: a ClickHouse 5xx must not break the client ---------------------
    received_c = Received()
    received_c.status = 500
    ch_c = ThreadingHTTPServer(("127.0.0.1", CH_PORT_D), make_ch_mock(received_c))
    ch_c.daemon_threads = True
    threading.Thread(target=ch_c.serve_forever, daemon=True).start()
    node_c = start_node("wire_c", f"http://127.0.0.1:{CH_PORT_D}/?database=dogress",
                        admin=ADMIN + 20, data=DATA + 20)
    try:
        if not wait_healthy(ADMIN + 20):
            check("W4: the third node became healthy", False, "never healthy")
        else:
            seed(ADMIN + 20)
            st, body = proxied(DATA + 20)
            check("W4: the CLIENT still gets 200 while ClickHouse answers 500 (the sink must "
                  "never break the proxy path)", st == 200, f"HTTP {st} {body[:60]}")
            if await_flush(received_c, want_at_least=2, budget=25.0):
                check("W4: the sink RETRIES a rejected batch (≥2 attempts seen)", received_c.count() >= 2,
                      f"attempts={received_c.count()}")
            else:
                check("W4: the sink RETRIES a rejected batch (≥2 attempts seen)", False,
                      f"attempts={received_c.count()}")
            log = open(os.path.join(DIR, "wire_c.log"), errors="replace").read()
            # Show the FAILURE line, not merely "the first line mentioning the sink" — the
            # latter printed the unrelated `usage sink built kind=clickhouse` INFO line, so a
            # green check displayed evidence that did not contain the asserted string.
            check("W4: ...and says so in the log (`usage sink batch insert failed`)",
                  "usage sink batch insert failed" in log,
                  next((l for l in log.splitlines()
                        if "usage sink batch insert failed" in l), "<no matching line>")[:200])
    finally:
        stop(node_c)
        close_server(ch_c)
        upstream.shutdown()

    print()
    if failures:
        print(f"CLICKHOUSE WIRE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("CLICKHOUSE WIRE: PASSED (Basic auth from userinfo, verbatim passthrough, INSERT as "
          "query= with the row as a masked JSONEachRow body, query-param credentials, 5xx does "
          "not break the client and is retried)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
