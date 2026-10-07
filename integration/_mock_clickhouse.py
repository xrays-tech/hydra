#!/usr/bin/env python3
"""A REAL HTTP ClickHouse double for drills that assert what was written (ADR-0002 T3.10).

It answers READS the way ClickHouse does — an aggregate over an empty window returns one row of
zeros (NOT an empty body, which the reader correctly treats as a decode failure) — so a drill that
reads `/usage` is measuring the real reader.

This is not a mock of Hydra: the drill still starts the real binary, which still goes through the
real writer, the real transport and real HTTP. What stands in is only the database, and it RECORDS
what it was sent — so "the usage row landed" stays a measurement. That matters because the three
drills that used to read their evidence out of the SQLite table (`test_streaming_path.py` S4,
`test_shutdown_drain.py` N3, `test_client_disconnect.py` C0/C1) would otherwise lose their subject
when SQLite is retired, and asserting on a log line is not the same claim.

`integration/test_clickhouse_sink_wire.py` deliberately keeps its OWN handler: it asserts the request
LINE (percent-encoded SQL in `query=`, `param_*` absence, Basic auth from the URL userinfo, path
trimming), which is a different question from "which rows arrived".

Usage:

    ch = MockClickHouse()
    ch.start()
    ...  # start the node with usage_env("clickhouse", ch.url)
    rows = ch.wait_for_rows(1, timeout=15)
    ch.stop()
"""

from __future__ import annotations

import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse


class _Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *args):  # keep the drill output readable
        pass

    def _respond(self, code: int = 200, body: str = "") -> None:
        payload = body.encode()
        self.send_response(code)
        self.send_header("Content-Type", "text/plain; charset=UTF-8")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def do_POST(self) -> None:  # noqa: N802 (http.server's naming)
        length = int(self.headers.get("Content-Length") or 0)
        raw = self.rfile.read(length) if length else b""
        parsed = urlparse(self.path)
        query = (parse_qs(parsed.query).get("query") or [""])[0]
        server = self.server
        upper = query.strip().upper()
        with server.lock:
            server.requests.append({"path": self.path, "query": query, "body": raw.decode("utf-8", "replace")})
            if upper.startswith("INSERT"):
                server.inserts += 1
                for line in raw.decode("utf-8", "replace").splitlines():
                    line = line.strip()
                    if not line:
                        continue
                    try:
                        server.rows.append(json.loads(line))
                    except json.JSONDecodeError:
                        server.bad_lines.append(line)
                return self._respond(200)
            if upper.startswith("SELECT"):
                # AGGREGATE queries over an empty window do NOT return an empty body: ClickHouse's
                # `count()`/`sum()` with no GROUP BY returns exactly ONE row (of zeros here), and
                # the reader treats an empty body as a DECODE FAILURE on purpose ("I could not read
                # the store" must never look like "you used nothing").
                #
                # Measured 2026-10-07: answering with an empty body made three drill legs report
                # `decode_error: totals: Malformed("empty body")` — the double was wrong, not the
                # reader. `last_seen: ""` is what ClickHouse returns for `MAX(created_at)` over no
                # rows, and the reader maps it to `as_of: null`.
                if "GROUP BY" in upper:
                    return self._respond(200, "")  # zero grouped rows is the honest answer
                totals = {
                    "requests": "0",
                    "tokens_in": "0",
                    "tokens_out": "0",
                    "cache_hit_tokens": "0",
                    "errors": "0",
                    "last_seen": "",
                }
                return self._respond(200, json.dumps(totals) + "\n")
        self._respond(200)

    def do_GET(self) -> None:  # noqa: N802
        self._respond(200, "")


class MockClickHouse:
    """A ClickHouse HTTP endpoint that answers 200 and remembers the rows it was sent."""

    def __init__(self) -> None:
        self._server: ThreadingHTTPServer | None = None
        self._thread: threading.Thread | None = None

    def start(self) -> str:
        self._server = ThreadingHTTPServer(("127.0.0.1", 0), _Handler)
        self._server.lock = threading.Lock()       # type: ignore[attr-defined]
        self._server.rows = []                     # type: ignore[attr-defined]
        self._server.bad_lines = []                # type: ignore[attr-defined]
        self._server.requests = []                 # type: ignore[attr-defined]
        self._server.inserts = 0                   # type: ignore[attr-defined]
        self._thread = threading.Thread(target=self._server.serve_forever, daemon=True)
        self._thread.start()
        return self.url

    @property
    def url(self) -> str:
        if self._server is None:
            raise RuntimeError("start() the mock first")
        host, port = self._server.server_address[:2]
        return f"http://{host}:{port}"

    @property
    def rows(self) -> list[dict]:
        return list(getattr(self._server, "rows", []))

    @property
    def insert_requests(self) -> int:
        return int(getattr(self._server, "inserts", 0))

    @property
    def bad_lines(self) -> list[str]:
        return list(getattr(self._server, "bad_lines", []))

    def by_trace(self, trace_id: str) -> list[dict]:
        key = "trace_id"
        return [r for r in self.rows if r.get(key) == trace_id]

    def wait_for_rows(self, n: int, timeout: float = 15.0) -> list[dict]:
        """Poll until `n` rows have arrived (the sink flushes on a size OR time threshold)."""
        deadline = time.time() + timeout
        while time.time() < deadline:
            if len(self.rows) >= n:
                break
            time.sleep(0.05)
        return self.rows

    def stop(self) -> None:
        if self._server is not None:
            self._server.shutdown()
            self._server.server_close()
            self._server = None
        if self._thread is not None:
            self._thread.join(timeout=5)
            self._thread = None


if __name__ == "__main__":  # a smoke test an operator can run
    import urllib.request

    mock = MockClickHouse()
    url = mock.start()
    req = urllib.request.Request(f"{url}/?query=INSERT%20INTO%20usage_record%20FORMAT%20JSONEachRow",
                                 data=b'{"trace_id":"smoke","tokens_in":1}\n', method="POST")
    with urllib.request.urlopen(req, timeout=5) as resp:
        print("status", resp.status)
    print("rows", mock.rows, "inserts", mock.insert_requests, "bad", mock.bad_lines)
    mock.stop()
