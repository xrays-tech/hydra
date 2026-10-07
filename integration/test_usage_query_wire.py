#!/usr/bin/env python3
"""The tenant API's USAGE READ path over ClickHouse — `tenant-api-integration.md` §5.3, executed.

Every fact §5.3 documents about the cluster reader was measured once by hand against the
bundled ClickHouse 24.3 (`design-tenant-api.md` §4.3.3) and then pinned only by `#[ignore]`d
tests that need a live database. The reader's own WIRE behaviour — what it sends, what it
does with each shape ClickHouse can answer, and what the tenant sees when it cannot read —
has never been observed at the HTTP level. That gap matters because the failure mode this
whole capability exists to prevent is a **200 that is syntactically fine and semantically
wrong**: a parser that only accepts JSON numbers reads ClickHouse's string-encoded 64-bit
integers as *zero usage* and reports an honest-looking empty window.

Cases (a mock ClickHouse stands in for the database, so no real instance is touched):
  U1/U2 the totals row, answered in ClickHouse's MEASURED shape (`{"requests":"3",…}` — the
        counters are JSON STRINGS), must come back as real numbers, with `as_of` the window's
        `MAX(created_at)` and `source: clickhouse`
  U3    the SQL and its bindings: `WHERE tenant_id = {t:String}` with `param_t`/`param_s`/
        `param_e` — the tenant id is bound, never interpolated into the query text
  U4    `group_by=model` is a SECOND query (`GROUP BY key ORDER BY key`), not a UNION ALL
  U5    `group_by=sub_tenant` coalesces the NULL sub-tenant key (one unattributed row would
        otherwise fail the whole window) and `group_by=day` slices `created_at`
  U6    the JSON-NUMBER shape is accepted too, so a format change cannot zero usage
  U7    an EMPTY window: ClickHouse answers `{"last_seen":""}` (not null) -> `as_of: null`
  U8/U9 a store that cannot be read -> 503 `usage_store_unavailable`, NEVER a zeroed 200, with
        the failure attributed in `hydra_tenant_api_usage_query_total{result="decode_error"}`
  U10   a 5xx from ClickHouse -> the same 503 and the same MESSAGE as a decode error (both are
        "transient, retry or call the operator")
  U11   an answer past the response cap -> 503 with a DIFFERENT message: retrying is pointless,
        the caller must narrow the window
  U12   the good path is attributed too (`result="ok"` + the latency histogram)
  U13   querying usage does NOT write usage (the read path sends no INSERT)
  U14   `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` is the READER's own deadline, independent of the
        writer's: on a hung ClickHouse the tenant gets its 503 in seconds, not in 15s

Run: python3 integration/test_usage_query_wire.py      # needs target/debug/hydra
The binary must be built with `usage-clickhouse`:
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse --bin hydra
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
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from _usage_env import usage_env

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "usage-query-wire-test")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
ADMIN, DATA, UPSTREAM = 18880, 18881, 18889
CH_A, CH_B = 18887, 18886          # one mock per node: each leg owns its own port
ADMIN_B, DATA_B = ADMIN + 10, DATA + 10
TOKEN = "hydra-usage-read-admin-2026"
TENANT_TOKEN = "tenant-usage-read-token-2026"
DOMAIN = "usage.local"
WINDOW = "since=2026-09-29T00:00:00Z&until=2026-09-30T00:00:00Z"
LAST_SEEN = "2026-09-29T19:50:07Z"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def announce(label, detail):
    print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def close_server(server):
    """`shutdown()` alone leaves the listening socket BOUND; `server_close()` frees it."""
    server.shutdown()
    server.server_close()


# ---------------------------------------------------------------------------
# The mock ClickHouse
# ---------------------------------------------------------------------------

def totals_body(mode):
    """The totals row in the requested shape. `mode` is the leg's scenario."""
    if mode == "numbers":
        return '{"requests":7,"tokens_in":100,"tokens_out":40,"cache_hit_tokens":10,' \
               f'"errors":1,"last_seen":"{LAST_SEEN}"}}\n'
    if mode == "empty":
        # ClickHouse's measured empty-window answer: `""`, not null.
        return '{"requests":"0","tokens_in":"0","tokens_out":"0","cache_hit_tokens":"0",' \
               '"errors":"0","last_seen":""}\n'
    if mode == "no_last_seen":
        # The documented shape-drift case: the aggregate SQL always projects `last_seen`.
        return '{"requests":"3","tokens_in":"100","tokens_out":"40","cache_hit_tokens":"10",' \
               '"errors":"1"}\n'
    if mode == "malformed":
        return "this line is not JSON at all\n"
    if mode == "huge":
        # Past the transport's 64 KiB response cap: a cut-off answer, not a malformed one.
        return '{"requests":"3","tokens_in":"100","tokens_out":"40","cache_hit_tokens":"10",' \
               f'"errors":"1","last_seen":"{LAST_SEEN}","pad":"' + ("x" * 70000) + '"}\n'
    return f'{{"requests":"3","tokens_in":"100","tokens_out":"40","cache_hit_tokens":"10",' \
           f'"errors":"1","last_seen":"{LAST_SEEN}"}}\n'


def group_body(sql):
    """One group row per key, in the shape the SQL asked for."""
    def row(key, req, ti, to, ch, err):
        return '{"key":"%s","requests":"%s","tokens_in":"%s","tokens_out":"%s",' \
               '"cache_hit_tokens":"%s","errors":"%s"}' % (key, req, ti, to, ch, err)

    if "coalesce(sub_tenant_id" in sql:
        # The documented empty-string group: usage that matched no sub-tenant prefix.
        return row("", "1", "10", "4", "0", "0") + "\n" + row("sub-1", "2", "90", "36", "8", "1") + "\n"
    if "sub_tenant_id AS key" in sql:
        # What a BARE `sub_tenant_id` really answers (measured on ClickHouse 24.3): SQL NULL
        # becomes JSON `null`, and the decoder requires a string `key` — so the whole window
        # fails as a decode error. This mock branch exists so that dropping the `coalesce`
        # fails the leg for the PRODUCT's reason, not for the harness's.
        return '{"key":null,"requests":"1","tokens_in":"10","tokens_out":"4",' \
               '"cache_hit_tokens":"0","errors":"0"}\n'
    if "substr(created_at" in sql:
        return row("2026-09-29", "3", "100", "40", "10", "1") + "\n"
    return row("echo", "2", "90", "36", "8", "0") + "\n" + row("other", "1", "10", "4", "2", "1") + "\n"


class Received:
    """Every request the node sent to the mock ClickHouse, plus the answer mode."""

    def __init__(self, mode="strings"):
        self.lock = threading.Lock()
        self.requests = []          # (method, path, headers, body)
        self.mode = mode
        self.hang_secs = 0.0

    def record(self, method, path, headers, body):
        with self.lock:
            self.requests.append((method, path, headers, body))

    def snapshot(self):
        with self.lock:
            return list(self.requests)

    def set_mode(self, mode):
        with self.lock:
            self.mode = mode

    def mode_now(self):
        with self.lock:
            return self.mode

    def count_since(self, marker):
        return len(self.snapshot()) - marker


def make_ch_mock(received):
    class H(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_POST(self):
            length = int(self.headers.get("Content-Length", 0))
            body = self.rfile.read(length).decode(errors="replace") if length else ""
            received.record("POST", self.path, {k.lower(): v for k, v in self.headers.items()}, body)
            mode = received.mode_now()
            if received.hang_secs:
                time.sleep(received.hang_secs)
            query = (urllib.parse.parse_qs(urllib.parse.urlparse(self.path).query).get("query")
                     or [""])[0]
            if "INSERT" in query:
                # The writer's path: 200 with an empty body.
                payload, status = b"", 200
            elif mode == "http500":
                # ClickHouse's real failure shape: a non-2xx carrying `Code: N`.
                payload = (b"Code: 241. DB::Exception: Memory limit exceeded: "
                           b"would use 12.34 GiB (attempt to allocate chunk of 1048576 bytes)")
                status = 500
            elif "AS key" in query:
                payload = group_body(query).encode()
                status = 200
            else:
                payload = totals_body(mode).encode()
                status = 200
            self.send_response(status)
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(payload)
            # The node sends `Connection: close`; a mock that keeps the socket open would make
            # every read wait for the deadline instead of ending at EOF.
            self.close_connection = True

        def log_message(self, *a):
            pass

    return H


class Upstream(BaseHTTPRequestHandler):
    """The tenant `auth_url` the data plane never needs here — but `reload` validates it."""

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        payload = json.dumps({"allowed": True, "expires_in": 300}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *a):
        pass


# ---------------------------------------------------------------------------
# HTTP helpers
# ---------------------------------------------------------------------------

def call(method, url, token=None, body=None, timeout=30, host=None):
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


def admin(method, path, body=None, port=ADMIN):
    return call(method, f"http://127.0.0.1:{port}/api/v1{path}", token=TOKEN, body=body)


def usage(query="", port=DATA, timeout=30):
    """The documented read. Returns (status, elapsed_seconds, parsed_body_or_text)."""
    started = time.time()
    st, text = call("GET", f"http://127.0.0.1:{port}/tenant/t1/api/v1/usage?{WINDOW}{query}",
                    token=TENANT_TOKEN, host=DOMAIN, timeout=timeout)
    elapsed = time.time() - started
    try:
        return st, elapsed, json.loads(text)
    except Exception:
        return st, elapsed, text


def metric(name):
    _, body = call("GET", f"http://127.0.0.1:{ADMIN}/metrics", token=TOKEN)
    return [l for l in body.splitlines() if l.startswith(name) and not l.startswith("#")]


def metric_value(name, labels):
    """Sum of the samples of `name` whose line carries all of `labels` (label="value")."""
    total = 0.0
    seen = False
    for line in metric(f"{name}{{"):
        if all(f'{k}="{v}"' in line for k, v in labels.items()):
            seen = True
            total += float(line.rsplit(" ", 1)[1])
    return (seen, total)


def start_node(label, ch_url, admin_port, data_port, extra=None):
    env = dict(os.environ)
    env.update(usage_env())
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{admin_port}",
        "HYDRA_LISTEN": f"127.0.0.1:{data_port}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, label)}.db?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "HYDRA_USAGE_SINK": "clickhouse",
        "HYDRA_CLICKHOUSE_URL": ch_url,
        "RUST_LOG": "info",
    })
    env.update(extra or {})
    log = open(os.path.join(DIR, f"{label}.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def wait_healthy(port=ADMIN, budget=25.0):
    deadline = time.time() + budget
    while time.time() < deadline:
        if admin("GET", "/health", port=port)[0] == 200:
            return True
        time.sleep(0.25)
    return False


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


def seed(admin_port=ADMIN):
    """One provider/model/tenant. The usage reader needs none of it to work, but the tenant
    API requires an authenticated tenant, and its config must load."""
    st, out = admin("POST", "/providers", {"id": "p1", "key": "p1", "name": "P",
                                           "endpoint": f"http://127.0.0.1:{UPSTREAM}", "weight": 1,
                                           "created_at": "", "updated_at": ""}, port=admin_port)
    if st not in (200, 201):
        raise SystemExit(f"[usage-wire] CANNOT VERIFY: seeding provider -> {st} {out[:160]}")
    admin("POST", "/provider-models", {"id": "pm1", "key": "echo", "name": "E", "provider_id": "p1",
                                       "status": 1, "created_at": "", "updated_at": ""},
          port=admin_port)
    admin("POST", "/provider-keys", {"id": "pk1", "provider_id": "p1", "api_key": "sk-up",
                                     "created_at": ""}, port=admin_port)
    st, out = admin("POST", "/tenants", {"id": "t1", "name": "T1", "domain": DOMAIN,
                                         "auth_url": f"http://127.0.0.1:{UPSTREAM}/auth",
                                         "enabled": True, "access_token": TENANT_TOKEN,
                                         "cert_key": None, "cert_file": None,
                                         "created_at": "", "updated_at": ""}, port=admin_port)
    if st not in (200, 201):
        raise SystemExit(f"[usage-wire] CANNOT VERIFY: seeding tenant -> {st} {out[:160]}")
    admin("POST", "/tenant-providers", {"id": "tp1", "tenant_id": "t1", "provider_id": "p1",
                                        "created_at": "", "updated_at": ""}, port=admin_port)
    admin("POST", "/tenant-models", {"id": "tm1", "tenant_id": "t1", "model_key": "echo",
                                     "created_at": "", "updated_at": ""}, port=admin_port)
    admin("POST", "/reload", {}, port=admin_port)
    time.sleep(0.3)


def main():
    if not os.path.exists(BIN):
        print(f"[usage-wire] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()

    # ---- the god-path node: real shapes, then one broken store after another -----
    received = Received()
    ch = ThreadingHTTPServer(("127.0.0.1", CH_A), make_ch_mock(received))
    ch.daemon_threads = True
    threading.Thread(target=ch.serve_forever, daemon=True).start()
    node = start_node("usage_a", f"http://127.0.0.1:{CH_A}/?database=dogress", ADMIN, DATA)
    try:
        if not wait_healthy():
            print("[usage-wire] CANNOT VERIFY: the node never became healthy", file=sys.stderr)
            print(open(os.path.join(DIR, "usage_a.log")).read()[-800:], file=sys.stderr)
            return 2
        seed()
        ok_before, _ = metric_value("hydra_tenant_api_usage_query_total",
                                    {"source": "clickhouse", "group_by": "none", "result": "ok"})
        hist_before, _ = metric_value("hydra_tenant_api_usage_query_seconds_count",
                                      {"source": "clickhouse"})
        inserts_before = len([r for r in received.snapshot() if "INSERT" in r[1]])

        # ---- U1/U2: the measured ClickHouse shape, decoded ---------------------
        st, elapsed, body = usage()
        check("U1: the usage read answers 200 against a ClickHouse-shaped store",
              st == 200 and isinstance(body, dict),
              f"HTTP {st} {str(body)[:110]}")
        totals = body.get("totals", {}) if isinstance(body, dict) else {}
        announce("U1 the totals", json.dumps(totals, sort_keys=True))
        check("U1: string-encoded 64-bit counters (ClickHouse's measured shape) decode to "
              "real numbers — NOT a zeroed window",
              totals.get("requests") == 3 and totals.get("tokens_in") == 100
              and totals.get("tokens_out") == 40 and totals.get("cache_hit_tokens") == 10
              and totals.get("errors") == 1,
              json.dumps(totals, sort_keys=True))
        check("U2: `as_of` is the window's MAX(created_at), and the source is named",
              body.get("as_of") == LAST_SEEN and body.get("source") == "clickhouse"
              and body.get("group_by") == "none" and body.get("rows") == [],
              f"as_of={body.get('as_of')!r} source={body.get('source')!r} "
              f"group_by={body.get('group_by')!r} rows={body.get('rows')!r}")
        check("U2: the window is echoed back normalized (the caller must trust the echo)",
              body.get("since") == "2026-09-29T00:00:00Z"
              and body.get("until") == "2026-09-30T00:00:00Z"
              and body.get("tenant_id") == "t1",
              f"since={body.get('since')!r} until={body.get('until')!r}")
        # A read that ended at EOF, not at the deadline: the default deadline here is 5s.
        check("U2: the answer arrives at EOF, not at the query deadline (measured latency)",
              elapsed < 2.0, f"{elapsed:.2f}s")

        # ---- U3: the SQL and its bindings -------------------------------------
        req = received.snapshot()[-1]
        parsed = urllib.parse.parse_qs(urllib.parse.urlparse(req[1]).query)
        sql = (parsed.get("query") or [""])[0]
        announce("U3 the SQL", f"{sql[:150]}…")
        check("U3: the totals query reads only this tenant's rows in the window",
              "FROM usage_record" in sql and "WHERE tenant_id = {t:String}" in sql
              and "created_at >= {s:String}" in sql and "created_at < {e:String}" in sql,
              f"{sql[:140]}")
        check("U3: the tenant id and the window travel as BOUND parameters, not spliced into "
              "the SQL",
              parsed.get("param_t") == ["t1"]
              and parsed.get("param_s") == ["2026-09-29T00:00:00Z"]
              and parsed.get("param_e") == ["2026-09-30T00:00:00Z"]
              and "t1" not in sql,
              f"param_t={parsed.get('param_t')} param_s={parsed.get('param_s')} "
              f"param_e={parsed.get('param_e')}")

        # ---- U4: grouped reads are a second query -----------------------------
        mark = len(received.snapshot())
        st, _, body = usage("&group_by=model")
        reqs = received.snapshot()[mark:]
        grouped_sql = urllib.parse.parse_qs(
            urllib.parse.urlparse(reqs[-1][1]).query).get("query", [""])[0]
        rows = {r["key"]: r for r in body.get("rows", [])} if isinstance(body, dict) else {}
        check("U4: `group_by=model` is a SECOND query whose rows are decoded",
              st == 200 and sorted(rows) == ["echo", "other"]
              and rows["echo"]["requests"] == 2 and rows["echo"]["tokens_in"] == 90,
              f"HTTP {st} rows={sorted(rows)}")
        check("U4: ...and that query groups and orders by the key itself",
              "GROUP BY key ORDER BY key" in grouped_sql and "AS key" in grouped_sql
              and "count() AS requests" in grouped_sql,
              f"{grouped_sql[:150]}")
        check("U4: the totals line stays the OVERALL totals, not the first group row "
              "(the UNION ALL order trap)",
              isinstance(body, dict) and body.get("totals", {}).get("requests") == 3,
              f"totals={body.get('totals') if isinstance(body, dict) else body}")

        # ---- U5: the two special groupings ------------------------------------
        mark = len(received.snapshot())
        st, _, body = usage("&group_by=sub_tenant")
        sub_sql = urllib.parse.parse_qs(
            urllib.parse.urlparse(received.snapshot()[mark:][-1][1]).query).get("query", [""])[0]
        sub_keys = [r["key"] for r in body.get("rows", [])] if isinstance(body, dict) else []
        check("U5: `group_by=sub_tenant` coalesces the NULL sub-tenant key (one unattributed "
              "row must not fail the whole window)",
              "coalesce(sub_tenant_id, '')" in sub_sql,
              f"{sub_sql[:130]}")
        check("U5: ...and unmatched usage arrives as the documented empty-string group",
              st == 200 and "" in sub_keys and "sub-1" in sub_keys,
              f"HTTP {st} keys={sub_keys}")
        mark = len(received.snapshot())
        st, _, body = usage("&group_by=day")
        day_sql = urllib.parse.parse_qs(
            urllib.parse.urlparse(received.snapshot()[mark:][-1][1]).query).get("query", [""])[0]
        check("U5: `group_by=day` slices the fixed-width `created_at` (no date function, so the "
              "primary key still prunes)",
              "substr(created_at, 1, 10)" in day_sql
              and [r["key"] for r in body.get("rows", [])] == ["2026-09-29"],
              f"{day_sql[:130]}")

        # ---- U6: the other legal number shape ---------------------------------
        received.set_mode("numbers")
        st, _, body = usage()
        received.set_mode("strings")
        check("U6: the JSON-NUMBER shape is accepted too, so a format change cannot silently "
              "zero usage",
              st == 200 and isinstance(body, dict)
              and body.get("totals", {}).get("requests") == 7,
              f"HTTP {st} totals={body.get('totals') if isinstance(body, dict) else body}")

        # ---- U7: the empty window ---------------------------------------------
        received.set_mode("empty")
        st, _, body = usage()
        received.set_mode("strings")
        check("U7: an empty window is a 200 with `as_of: null` (ClickHouse answers `\"\"`, not "
              "null — an empty-string as_of would look like a real value)",
              st == 200 and isinstance(body, dict) and body.get("as_of") is None
              and body.get("totals", {}).get("requests") == 0,
              f"HTTP {st} as_of={body.get('as_of')!r} "
              f"totals={body.get('totals') if isinstance(body, dict) else body}")

        # ---- U8/U9/U10/U11: a store that cannot be read ------------------------
        # The "no false attribution" direction, measured BEFORE the failure legs: after six
        # SUCCESSFUL reads there must not be a single failure sample. (Absence of the sample is
        # the healthy state here — asserting the flag itself would be asserting a bug.)
        dec_mid_flag, dec_mid = metric_value("hydra_tenant_api_usage_query_total",
                                            {"result": "decode_error"})
        check("U12: none of the successful reads was reported as a failure",
              dec_mid == 0.0,
              f"after six good reads: decode_error total={dec_mid} (sample present={dec_mid_flag})")
        messages = {}
        for mode, label, result_label in (
            ("no_last_seen", "U8: an answer missing `last_seen` is SHAPE DRIFT, not an empty "
                             "window", "decode_error"),
            ("malformed", "U9: a body that is not JSON is a decode failure", "decode_error"),
            ("http500", "U10: a 5xx from ClickHouse is a store failure", "store_unavailable"),
            ("huge", "U11: an answer past the response cap is `result_too_large`",
             "result_too_large"),
        ):
            seen_before, before = metric_value("hydra_tenant_api_usage_query_total",
                                              {"result": result_label})
            received.set_mode(mode)
            st, _, body = usage()
            received.set_mode("strings")
            after_n, after = metric_value("hydra_tenant_api_usage_query_total",
                                          {"result": result_label})
            code = body.get("error", {}).get("code") if isinstance(body, dict) else None
            message = body.get("error", {}).get("message") if isinstance(body, dict) else str(body)
            messages[result_label] = message
            check(f"{label} -> 503, never a zeroed 200",
                  st == 503 and code == "usage_store_unavailable"
                  and not (isinstance(body, dict) and "totals" in body),
                  f"HTTP {st} code={code!r} body={str(body)[:110]}")
            check(f"{label.split(':')[0]}: ...and the FAILURE is attributed in "
                  f"`hydra_tenant_api_usage_query_total{{result=\"{result_label}\"}}`",
                  after_n and after == before + 1,
                  f"before={before} after={after}")
        check("U10: a transient store failure and a decode failure read the SAME to the tenant "
              "(both are 'retry or call the operator')",
              messages.get("store_unavailable") == messages.get("decode_error")
              and "retry" in (messages.get("store_unavailable") or ""),
              f"store={messages.get('store_unavailable')!r} decode={messages.get('decode_error')!r}")
        check("U11: ...while `result_too_large` SAYS something different — retrying returns "
              "the same cut-off answer",
              "narrow since/until" in (messages.get("result_too_large") or "")
              and messages["result_too_large"] != messages["decode_error"],
              f"too_large={messages.get('result_too_large')!r}")

        # ---- U12: the good path is attributed, and nothing is over-reported ----
        ok_after, ok_total = metric_value("hydra_tenant_api_usage_query_total",
                                         {"source": "clickhouse", "group_by": "none",
                                          "result": "ok"})
        hist_after, hist_total = metric_value("hydra_tenant_api_usage_query_seconds_count",
                                             {"source": "clickhouse"})
        check("U12: every successful read is counted (`result=\"ok\"`) and timed",
              ok_after and ok_total > ok_before and hist_after and hist_total > hist_before,
              f"ok {ok_before} -> {ok_total}; histogram {hist_before} -> {hist_total}")
        # Exact counts, so a read reported twice (or under two results at once) is visible:
        # three ungrouped reads succeeded (U1, U6, U7) and exactly two were decode failures
        # (U8 missing `last_seen`, U9 not-JSON).
        _, ok_none_total = metric_value("hydra_tenant_api_usage_query_total",
                                       {"source": "clickhouse", "group_by": "none", "result": "ok"})
        _, dec_final = metric_value("hydra_tenant_api_usage_query_total", {"result": "decode_error"})
        check("U12: each read is recorded exactly ONCE, under exactly one result",
              ok_none_total == 3.0 and dec_final == 2.0,
              f"ok(source=clickhouse,group_by=none)={ok_none_total} decode_error={dec_final}")

        # ---- U13: reading usage does not write usage --------------------------
        inserts_after = len([r for r in received.snapshot() if "INSERT" in r[1]])
        check("U13: querying usage writes NO usage (the read path sends no INSERT)",
              inserts_after == inserts_before == 0,
              f"INSERTs before={inserts_before} after={inserts_after}")
        log = open(os.path.join(DIR, "usage_a.log"), errors="replace").read()
        check("U13: ...and the failures were logged for the operator",
              log.count("usage query failed") >= 4,
              f"{log.count('usage query failed')} `usage query failed` lines")
    finally:
        stop(node)
        close_server(ch)

    # ---- U14: the reader has its OWN deadline --------------------------------
    received_b = Received()
    received_b.hang_secs = 5.0
    ch_b = ThreadingHTTPServer(("127.0.0.1", CH_B), make_ch_mock(received_b))
    ch_b.daemon_threads = True
    threading.Thread(target=ch_b.serve_forever, daemon=True).start()
    # 1200ms, far below both the 5000ms default and the WRITER's own 15000ms.
    node_b = start_node("usage_b", f"http://127.0.0.1:{CH_B}/?database=dogress", ADMIN_B, DATA_B,
                        extra={"HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS": "1200"})
    try:
        if not wait_healthy(ADMIN_B):
            check("U14: the second node became healthy", False, "never healthy")
        else:
            seed(ADMIN_B)
            st, elapsed, body = usage(port=DATA_B, timeout=25)
            code = body.get("error", {}).get("code") if isinstance(body, dict) else None
            check("U14: `HYDRA_CLICKHOUSE_QUERY_TIMEOUT_MS` bounds the READ (a hung ClickHouse "
                  "gives the tenant its 503 in ~1.2s, not in the writer's 15s)",
                  st == 503 and code == "usage_store_unavailable" and elapsed < 3.5,
                  f"HTTP {st} code={code!r} elapsed={elapsed:.2f}s")
            log_b = open(os.path.join(DIR, "usage_b.log"), errors="replace").read()
            check("U14: ...and the operator sees the deadline that fired",
                  "clickhouse read: timed out after 1200ms" in log_b,
                  next((l for l in log_b.splitlines() if "timed out" in l), "<no line>")[:170])
    finally:
        stop(node_b)
        close_server(ch_b)
        upstream.shutdown()

    print()
    if failures:
        print(f"USAGE QUERY WIRE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("USAGE QUERY WIRE: PASSED (string/number decoding, bound parameters, grouped reads, "
          "empty window, every unreadable-store shape attributed, no INSERT from the read path, "
          "and the reader's own deadline)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
