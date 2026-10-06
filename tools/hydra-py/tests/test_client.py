import dataclasses
import json
import threading
import time
import unittest
import urllib.error
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from hydra_sdk import (
    MAX_TIMEOUT_MS,
    MIN_TIMEOUT_MS,
    FleetState,
    HTTPError,
    HydraClient,
    InvalidateOutcomeError,
    InvalidatePendingError,
    InvalidateTenantAuthCacheResult,
    InvalidateUnavailableError,
    WaitMode,
    is_done_state,
    is_retryable_state,
)

ENDPOINT_PATH = "/tenant/t-acme/api/v1/auth/cache/invalidate"


def fleet_body(
    state="applied",
    *,
    invalidated=1,
    checked=0,
    tenant_id="t-acme",
    scope="tenant",
    nodes_total=1,
    nodes_applied=1,
    lagging=None,
    event_id="1737-0",
    waited_ms=41,
):
    """Render the documented invalidate response envelope (contract §5.2).

    This is the shape the server actually sends (``InvalidateView`` +
    ``FleetReport``): a 200/202/503 body carries no ``error`` object at all, so
    the fleet state is the only thing a caller may branch on.
    """
    if nodes_applied is None:
        nodes_applied = nodes_total
    body = {
        "invalidated": invalidated,
        "checked": checked,
        "tenant_id": tenant_id,
        "scope": scope,
        "fleet": {
            "state": state,
            "nodes_total": nodes_total,
            "nodes_applied": nodes_applied,
            "lagging": list(lagging or []),
            "event_id": event_id,
            "waited_ms": waited_ms,
        },
    }
    return json.dumps(body).encode("utf-8")


class Node:
    """One fake data-plane node.

    ``body`` overrides the invalidate response verbatim (bytes, str or any
    JSON-able object) and is how the non-fleet-aware answers are expressed.
    When it is None the handler renders the documented envelope for ``state``
    via :func:`fleet_body`, using any extra keyword arguments as its fields.
    ``trace_id`` is sent as ``X-Hydra-Trace-Id`` (None sends no header).
    """

    def __init__(
        self,
        leader_status,
        endpoint_status,
        *,
        state="applied",
        body=None,
        trace_id="trace-node-1",
        **fleet_fields
    ):
        self.leader_status = leader_status
        self.endpoint_status = endpoint_status
        self.state = state
        self.body = body
        self.trace_id = trace_id
        self.fleet_fields = fleet_fields
        self.endpoint_calls = 0
        self.path = None
        self.full_path = None
        self.lock = threading.Lock()

    def set_endpoint(self, status):
        with self.lock:
            self.endpoint_status = status

    def payload(self, status):
        if self.body is not None:
            if isinstance(self.body, (bytes, bytearray)):
                return bytes(self.body)
            if isinstance(self.body, str):
                return self.body.encode("utf-8")
            return json.dumps(self.body).encode("utf-8")
        if status in (200, 202, 503):
            return fleet_body(self.state, **self.fleet_fields)
        # A bare error answer: no fleet object, so it can only be an HTTP
        # failure (this is the `500` / `401` / `429` shape).
        return json.dumps({"error": {"code": "boom"}}).encode("utf-8")

    def respond(self, handler, status, payload):
        handler.send_response(status)
        handler.send_header("Content-Type", "application/json")
        if self.trace_id is not None:
            handler.send_header("X-Hydra-Trace-Id", self.trace_id)
        handler.send_header("Content-Length", str(len(payload)))
        handler.end_headers()
        handler.wfile.write(payload)

    def handle(self, path, method, handler):
        # The server routes on the path and reads `wait` / `timeout_ms` from the
        # query, so the route match must ignore the query string while
        # `full_path` keeps it.
        route = path.split("?", 1)[0]
        if route == "/healthz/leader":
            handler.send_response(self.leader_status)
            handler.send_header("Content-Type", "application/json")
            handler.end_headers()
            if self.leader_status == 200:
                handler.wfile.write(b'{"leader":true}')
            return
        if route == ENDPOINT_PATH and method == "POST":
            with self.lock:
                self.endpoint_calls += 1
                self.path = route
                self.full_path = handler.path
                status = self.endpoint_status
            if handler.headers.get("Authorization") != "Bearer sk-tenant-token":
                self.respond(handler, 401, self.payload(401))
                return
            self.respond(handler, status, self.payload(status))
            return
        handler.send_response(404)
        handler.end_headers()


def make_server(node):
    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):
            node.handle(self.path, "GET", self)

        def do_POST(self):
            length = int(self.headers.get("Content-Length") or 0)
            if length:
                self.rfile.read(length)
            node.handle(self.path, "POST", self)

        def log_message(self, *args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server


def make_fake_urlopen(mode):
    """Build a fake ``urlopen`` for a given invalidation failure mode.

    The leader probe always succeeds so the node is considered alive; the
    invalidation request then raises a timeout (mode="timeout") or a
    connection-refused error (mode="refused").
    """

    class FakeResp:
        status = 200

        def read(self):
            return b'{"leader":true}'

        def __enter__(self):
            return self

        def __exit__(self, *args):
            return False

    def urlopen(req, timeout=None):
        if req.full_url.endswith("/healthz/leader"):
            return FakeResp()
        if mode == "timeout":
            raise TimeoutError("timed out")
        if mode == "refused":
            raise urllib.error.URLError(ConnectionRefusedError(61, "Connection refused"))
        return FakeResp()

    return urlopen


class ClientTest(unittest.TestCase):
    def setUp(self):
        self.servers = []

    def tearDown(self):
        for server in self.servers:
            server.shutdown()
            server.server_close()

    def make_client(self, nodes, **kwargs):
        client = HydraClient(
            "sk-tenant-token",
            nodes,
            disable_background_recheck=True,
            probe_timeout=1.0,
            request_timeout=1.0,
            **kwargs,
        )
        self.addCleanup(client.close)
        return client

    def start(self, node):
        server = make_server(node)
        self.servers.append(server)
        return f"http://127.0.0.1:{server.server_port}"

    # ------------------------------------------------------------------
    # Existing behaviour (kept, now against the documented envelope)
    # ------------------------------------------------------------------
    def test_uses_leader_not_first_node(self):
        standby = Node(503, 200)
        leader = Node(200, 200)
        client = self.make_client([self.start(standby), self.start(leader)])
        client.invalidate_tenant_auth_cache("t-acme")
        self.assertEqual(leader.endpoint_calls, 1)
        self.assertEqual(standby.endpoint_calls, 0)

    def test_fails_over_and_removes_dead_node(self):
        dead = Node(200, 500)
        healthy = Node(503, 200)
        dead_url = self.start(dead)
        healthy_url = self.start(healthy)
        client = self.make_client([dead_url, healthy_url])
        client.invalidate_tenant_auth_cache("t-acme")
        self.assertEqual(dead.endpoint_calls, 1)
        self.assertEqual(healthy.endpoint_calls, 1)
        self.assertEqual(client.nodes, [healthy_url])
        self.assertEqual(client.removed_nodes, [dead_url])

    def test_probe_removed_restores_reachable_node(self):
        node = Node(200, 500)
        healthy = Node(503, 200)
        node_url = self.start(node)
        healthy_url = self.start(healthy)
        client = self.make_client([node_url, healthy_url])
        client.invalidate_tenant_auth_cache("t-acme")
        self.assertEqual(client.nodes, [healthy_url])
        node.set_endpoint(200)
        client.probe_removed_nodes()
        self.assertEqual(set(client.nodes), {node_url, healthy_url})
        self.assertEqual(client.removed_nodes, [])

    def test_single_node_without_leader_probe(self):
        node = Node(404, 200)
        client = self.make_client([self.start(node)])
        client.invalidate_tenant_auth_cache("t-acme")
        self.assertEqual(node.endpoint_calls, 1)

    def test_fallback_triggers_when_leader_probe_is_404(self):
        """``/healthz/leader`` is an ADMIN-port route, so a node list pointing at
        data-plane ports (the documented configuration) gets 404 from the probe
        while being perfectly able to serve the invalidation. The client must
        treat that as "alive, not the leader" and send directly.
        """
        first = Node(404, 200)
        second = Node(404, 200)
        first_url = self.start(first)
        second_url = self.start(second)
        client = self.make_client([first_url, second_url])
        client.invalidate_tenant_auth_cache("t-acme")
        self.assertEqual(first.endpoint_calls, 1)
        self.assertEqual(second.endpoint_calls, 0)
        # A 404 probe is not evidence of a dead node: nothing may be quarantined,
        # or a data-plane node list would empty itself out.
        self.assertEqual(client.removed_nodes, [])
        self.assertEqual(client.nodes, [first_url, second_url])

    def test_invalidate_targets_tenant_data_plane_path(self):
        """Regression: the endpoint lives under the data-plane reserved prefix.

        The 2026-09-17 move took it off the management API, where it used to be
        ``POST /api/v1/tenants/{id}/auth/cache/invalidate`` (a route that no
        longer exists, on the admin port this SDK is not pointed at).
        """
        node = Node(404, 200)
        client = self.make_client([self.start(node)])
        client.invalidate_tenant_auth_cache("t-acme")
        self.assertEqual(node.path, ENDPOINT_PATH)
        self.assertNotIn("/api/v1/tenants/", node.full_path)
        self.assertNotIn("tenants/t-acme", node.full_path)

    def test_tenant_id_is_path_escaped(self):
        seen = {}

        class PathHandler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(404)
                self.end_headers()

            def do_POST(self):
                seen["path"] = self.path
                length = int(self.headers.get("Content-Length") or 0)
                if length:
                    self.rfile.read(length)
                payload = fleet_body("applied")
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), PathHandler)
        self.servers.append(server)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        client = self.make_client([f"http://127.0.0.1:{server.server_port}"])
        client.invalidate_tenant_auth_cache("t/acme")
        self.assertEqual(seen["path"], "/tenant/t%2Facme/api/v1/auth/cache/invalidate")

    def test_auth_error_does_not_remove_node(self):
        node = Node(200, 401)
        url = self.start(node)
        client = self.make_client([url])
        with self.assertRaises(Exception) as ctx:
            client.invalidate_tenant_auth_cache("t-acme")
        self.assertIn("401", str(ctx.exception))
        self.assertEqual(client.removed_nodes, [])
        self.assertEqual(client.nodes, [url])

    def test_background_recheck_restores(self):
        node = Node(200, 500)
        healthy = Node(503, 200)
        node_url = self.start(node)
        healthy_url = self.start(healthy)
        client = HydraClient(
            "sk-tenant-token",
            [node_url, healthy_url],
            probe_timeout=1.0,
            request_timeout=1.0,
            recheck_interval=0.02,
        )
        self.addCleanup(client.close)
        client.invalidate_tenant_auth_cache("t-acme")
        node.set_endpoint(200)
        deadline = time.time() + 2
        while time.time() < deadline:
            if len(client.nodes) == 2 and not client.removed_nodes:
                return
            time.sleep(0.01)
        self.fail(f"node not restored: nodes={client.nodes} removed={client.removed_nodes}")

    def test_invalidate_keys_sends_body(self):
        seen = {}

        class BodyHandler(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.end_headers()

            def do_POST(self):
                length = int(self.headers.get("Content-Length") or 0)
                body = self.rfile.read(length) if length else b""
                seen["body"] = json.loads(body)
                payload = fleet_body("applied", scope="keys", checked=2, invalidated=2)
                self.send_response(200)
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), BodyHandler)
        self.servers.append(server)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        client = self.make_client([f"http://127.0.0.1:{server.server_port}"])
        client.invalidate_tenant_auth_cache_keys("t-acme", ["key1", "key2"])
        self.assertEqual(seen["body"], {"api_keys": ["key1", "key2"]})

    def test_timeout_does_not_remove_node(self):
        client = self.make_client(
            ["http://127.0.0.1:1"], urlopen=make_fake_urlopen("timeout")
        )
        with self.assertRaises(Exception) as ctx:
            client.invalidate_tenant_auth_cache("t-acme")
        self.assertIn("failed", str(ctx.exception))
        self.assertEqual(client.removed_nodes, [])
        self.assertEqual(client.nodes, ["http://127.0.0.1:1"])

    def test_connection_refused_removes_node(self):
        client = self.make_client(
            ["http://127.0.0.1:1"], urlopen=make_fake_urlopen("refused")
        )
        with self.assertRaises(Exception) as ctx:
            client.invalidate_tenant_auth_cache("t-acme")
        self.assertIn("failed", str(ctx.exception))
        self.assertEqual(client.removed_nodes, ["http://127.0.0.1:1"])
        self.assertEqual(client.nodes, [])

    # ------------------------------------------------------------------
    # Fleet state constants and helpers
    # ------------------------------------------------------------------
    def test_fleet_state_tokens_are_the_servers_own(self):
        self.assertEqual(FleetState.APPLIED, "applied")
        self.assertEqual(FleetState.SINGLE_NODE, "single_node")
        self.assertEqual(FleetState.PENDING, "pending")
        self.assertEqual(FleetState.UNAVAILABLE, "unavailable")
        self.assertEqual(WaitMode.CONVERGED, "converged")
        self.assertEqual(WaitMode.NONE, "none")
        self.assertEqual((MIN_TIMEOUT_MS, MAX_TIMEOUT_MS), (1, 60000))

    def test_state_helpers(self):
        self.assertTrue(is_done_state(FleetState.APPLIED))
        self.assertTrue(is_done_state(FleetState.SINGLE_NODE))
        self.assertFalse(is_done_state(FleetState.PENDING))
        self.assertFalse(is_done_state(FleetState.UNAVAILABLE))
        self.assertTrue(is_retryable_state(FleetState.PENDING))
        self.assertTrue(is_retryable_state(FleetState.UNAVAILABLE))
        self.assertFalse(is_retryable_state(FleetState.APPLIED))
        self.assertFalse(is_retryable_state(FleetState.SINGLE_NODE))
        # No state at all (an undecodable body) is neither done nor retryable.
        self.assertFalse(is_done_state(None))
        self.assertFalse(is_retryable_state(None))

    # ------------------------------------------------------------------
    # 200 applied
    # ------------------------------------------------------------------
    def test_applied_is_a_structured_success(self):
        node = Node(
            404,
            200,
            state="applied",
            invalidated=2,
            checked=2,
            scope="keys",
            nodes_total=3,
            nodes_applied=3,
            lagging=[],
            event_id="1737-0",
            waited_ms=41,
            trace_id="trace-applied-1",
        )
        url = self.start(node)
        client = self.make_client([url])

        res = client.invalidate_with_result("t-acme", ["sk-a", "sk-b"])
        self.assertIsInstance(res, InvalidateTenantAuthCacheResult)
        self.assertEqual(res.http_status, 200)
        self.assertEqual(res.state, FleetState.APPLIED)
        self.assertTrue(res.done)
        self.assertFalse(res.retryable)
        self.assertEqual(res.invalidated, 2)
        self.assertEqual(res.checked, 2)
        self.assertEqual(res.scope, "keys")
        self.assertEqual(res.nodes_applied, 3)
        self.assertEqual(res.nodes_total, 3)
        self.assertEqual(res.lagging, [])
        self.assertEqual(res.event_id, "1737-0")
        self.assertEqual(res.waited_ms, 41)
        self.assertEqual(res.trace_id, "trace-applied-1")
        self.assertEqual(res.node, url)

        # The wrapper's success case is unchanged: no exception, no rotation.
        self.assertIsNone(client.invalidate_tenant_auth_cache("t-acme"))
        self.assertEqual(node.endpoint_calls, 2)
        self.assertEqual(client.removed_nodes, [])

    def test_single_node_is_done(self):
        node = Node(
            404,
            200,
            state="single_node",
            nodes_total=1,
            nodes_applied=1,
            event_id=None,
            waited_ms=0,
        )
        url = self.start(node)
        client = self.make_client([url])
        self.assertIsNone(client.invalidate_tenant_auth_cache("t-acme"))
        res = client.invalidate_with_result("t-acme")
        self.assertEqual(res.state, FleetState.SINGLE_NODE)
        self.assertTrue(res.done)
        self.assertFalse(res.retryable)
        self.assertEqual((res.nodes_applied, res.nodes_total), (1, 1))
        self.assertIsNone(res.event_id)
        self.assertEqual(client.nodes, [url])

    # ------------------------------------------------------------------
    # 202 pending
    # ------------------------------------------------------------------
    def make_pending_nodes(self):
        pending = Node(
            200,
            202,
            state="pending",
            nodes_total=3,
            nodes_applied=1,
            lagging=["hydra-2", "hydra-3"],
            event_id="1737-9",
            waited_ms=2000,
            trace_id="trace-pending-1",
        )
        would_be_applied = Node(503, 200, state="applied", trace_id="trace-node-2")
        return pending, would_be_applied

    def test_pending_returns_a_result_and_the_wrapper_raises(self):
        pending, second = self.make_pending_nodes()
        pending_url = self.start(pending)
        second_url = self.start(second)
        client = self.make_client([pending_url, second_url])

        # The structured call does NOT raise: pending is a result, not a failure.
        res = client.invalidate_with_result("t-acme")
        self.assertEqual(res.http_status, 202)
        self.assertEqual(res.state, FleetState.PENDING)
        self.assertFalse(res.done)
        self.assertTrue(res.retryable)
        self.assertEqual(res.lagging, ["hydra-2", "hydra-3"])
        self.assertEqual(res.event_id, "1737-9")
        self.assertEqual(res.waited_ms, 2000)
        self.assertEqual(res.trace_id, "trace-pending-1")
        self.assertEqual((res.nodes_applied, res.nodes_total), (1, 3))

        with self.assertRaises(InvalidatePendingError) as ctx:
            client.invalidate_tenant_auth_cache("t-acme")
        exc = ctx.exception
        self.assertIsInstance(exc, InvalidateOutcomeError)
        self.assertNotIsInstance(exc, InvalidateUnavailableError)
        self.assertIsInstance(exc.result, InvalidateTenantAuthCacheResult)
        self.assertEqual(exc.result.state, FleetState.PENDING)
        # The message must name the state, the node, the counts, event_id and
        # the trace id.
        message = str(exc)
        self.assertIn("pending", message)
        self.assertIn("HTTP 202", message)
        self.assertIn(pending_url, message)
        self.assertIn("1/3 nodes applied", message)
        self.assertIn("1737-9", message)
        self.assertIn("trace-pending-1", message)

        # 202 is not an error at the node level: no quarantine, no rotation.
        self.assertEqual(pending.endpoint_calls, 2)
        self.assertEqual(second.endpoint_calls, 0)
        self.assertEqual(client.removed_nodes, [])
        self.assertEqual(client.nodes, [pending_url, second_url])

    # ------------------------------------------------------------------
    # 503 unavailable
    # ------------------------------------------------------------------
    def test_unavailable_is_not_a_node_failure(self):
        unavailable = Node(
            200,
            503,
            state="unavailable",
            nodes_total=2,
            nodes_applied=0,
            lagging=[],
            event_id=None,
            waited_ms=12,
            trace_id="trace-unavailable-1",
        )
        would_be_applied = Node(503, 200, state="applied", trace_id="trace-node-2")
        unavailable_url = self.start(unavailable)
        second_url = self.start(would_be_applied)
        client = self.make_client([unavailable_url, second_url])

        res = client.invalidate_with_result("t-acme")
        self.assertEqual(res.http_status, 503)
        self.assertEqual(res.state, FleetState.UNAVAILABLE)
        self.assertFalse(res.done)
        self.assertTrue(res.retryable)
        self.assertEqual(res.trace_id, "trace-unavailable-1")
        self.assertEqual((res.nodes_applied, res.nodes_total), (0, 2))
        self.assertEqual(res.lagging, [])

        with self.assertRaises(InvalidateUnavailableError) as ctx:
            client.invalidate_tenant_auth_cache("t-acme")
        exc = ctx.exception
        self.assertIsInstance(exc, InvalidateOutcomeError)
        self.assertNotIsInstance(exc, InvalidatePendingError)
        self.assertEqual(exc.result.state, FleetState.UNAVAILABLE)
        message = str(exc)
        self.assertIn("unavailable", message)
        self.assertIn("HTTP 503", message)
        self.assertIn("NOT notified", message)
        self.assertIn(unavailable_url, message)
        self.assertIn("0/2 nodes applied", message)
        self.assertIn("trace-unavailable-1", message)

        # The node ANSWERED: it is alive and its own cache was cleared, so it
        # must not be quarantined and the call must not rotate away from it.
        self.assertEqual(unavailable.endpoint_calls, 2)
        self.assertEqual(would_be_applied.endpoint_calls, 0)
        self.assertEqual(client.removed_nodes, [])
        self.assertEqual(client.nodes, [unavailable_url, second_url])

    # ------------------------------------------------------------------
    # Failover for everything that is NOT a fleet report
    # ------------------------------------------------------------------
    def test_bare_500_is_still_a_node_failure(self):
        dead = Node(404, 500)
        url = self.start(dead)
        client = self.make_client([url])
        with self.assertRaises(RuntimeError) as ctx:
            client.invalidate_with_result("t-acme")
        self.assertIn("500", str(ctx.exception))
        self.assertEqual(dead.endpoint_calls, 1)
        self.assertEqual(client.removed_nodes, [url])
        self.assertEqual(client.nodes, [])

    def test_non_fleet_aware_503_still_fails_over(self):
        """A 503 WITHOUT a fleet body is not the documented `unavailable`
        outcome — it is a plain server error, and failover must be unchanged.
        """
        dead = Node(200, 503, body={"error": {"code": "not_ready"}})
        healthy = Node(503, 200, state="applied")
        dead_url = self.start(dead)
        healthy_url = self.start(healthy)
        client = self.make_client([dead_url, healthy_url])
        self.assertIsNone(client.invalidate_tenant_auth_cache("t-acme"))
        self.assertEqual(dead.endpoint_calls, 1)
        self.assertEqual(healthy.endpoint_calls, 1)
        self.assertEqual(client.removed_nodes, [dead_url])
        self.assertEqual(client.nodes, [healthy_url])

    # ------------------------------------------------------------------
    # wait / timeout_ms plumbing
    # ------------------------------------------------------------------
    def test_wait_and_timeout_are_sent_when_configured(self):
        node = Node(404, 200, state="applied")
        url = self.start(node)
        client = self.make_client(
            [url], wait_mode=WaitMode.CONVERGED, timeout_ms=2000
        )
        client.invalidate_with_result("t-acme")
        self.assertEqual(node.path, ENDPOINT_PATH)
        self.assertIn("wait=converged", node.full_path)
        self.assertIn("timeout_ms=2000", node.full_path)
        self.assertTrue(node.full_path.startswith(ENDPOINT_PATH + "?"))

    def test_wait_none_is_sent_verbatim(self):
        node = Node(404, 202, state="pending", nodes_total=1, nodes_applied=0)
        client = self.make_client(
            [self.start(node)], wait_mode=WaitMode.NONE, timeout_ms=MAX_TIMEOUT_MS
        )
        client.invalidate_with_result("t-acme")
        self.assertIn("wait=none", node.full_path)
        self.assertIn("timeout_ms=60000", node.full_path)

    def test_no_wait_or_timeout_params_by_default(self):
        node = Node(404, 200, state="applied")
        client = self.make_client([self.start(node)])
        client.invalidate_with_result("t-acme")
        # Sending nothing lets the server apply its own default: wait=converged
        # with its configured 2000 ms budget (HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS).
        self.assertEqual(node.full_path, ENDPOINT_PATH)
        self.assertNotIn("wait", node.full_path)
        self.assertNotIn("timeout_ms", node.full_path)
        self.assertNotIn("?", node.full_path)

    def test_constructor_validation(self):
        for bad in ("", "always", "Converged", "converged ", " converged", "none ", 1, True):
            with self.assertRaises(ValueError, msg=f"wait_mode={bad!r}"):
                HydraClient(
                    "sk-tenant-token",
                    ["http://127.0.0.1:1"],
                    disable_background_recheck=True,
                    wait_mode=bad,
                )
        for bad in (0, -1, MAX_TIMEOUT_MS + 1, 1.5, "2000", True):
            with self.assertRaises(ValueError, msg=f"timeout_ms={bad!r}"):
                HydraClient(
                    "sk-tenant-token",
                    ["http://127.0.0.1:1"],
                    disable_background_recheck=True,
                    timeout_ms=bad,
                )
        # The bounds themselves, and "unset", are accepted.
        for good in (None, MIN_TIMEOUT_MS, MAX_TIMEOUT_MS):
            client = self.make_client(
                ["http://127.0.0.1:1"], wait_mode=WaitMode.NONE, timeout_ms=good
            )
            self.assertIsNotNone(client)
        self.assertIn("wait_mode", str(self.reject(wait_mode="always")))
        self.assertIn("timeout_ms", str(self.reject(timeout_ms=0)))
        self.assertIn("timeout_ms", str(self.reject(timeout_ms=True)))

    def reject(self, **kwargs):
        try:
            HydraClient(
                "sk-tenant-token",
                ["http://127.0.0.1:1"],
                disable_background_recheck=True,
                **kwargs
            )
        except ValueError as exc:
            return exc
        self.fail(f"ValueError expected for {kwargs!r}")
        return None

    # ------------------------------------------------------------------
    # Trace id and undecodable bodies
    # ------------------------------------------------------------------
    def test_trace_id_is_on_the_http_error_path(self):
        node = Node(200, 401, trace_id="trace-401-abc")
        url = self.start(node)
        client = self.make_client([url])
        with self.assertRaises(HTTPError) as ctx:
            client.invalidate_tenant_auth_cache("t-acme")
        exc = ctx.exception
        # §4.2 tells the caller to quote X-Hydra-Trace-Id when reporting a
        # problem, and an HTTP error is a problem.
        self.assertEqual(exc.trace_id, "trace-401-abc")
        self.assertIn("trace-401-abc", str(exc))
        self.assertEqual(exc.status, 401)
        self.assertEqual(client.removed_nodes, [])

    def test_undecodable_2xx_is_not_success(self):
        for body in (
            {"invalidated": 1, "tenant_id": "t-acme"},  # the pre-fleet envelope
            {"fleet": {"state": "in_progress"}},  # a state that is not documented
            {"fleet": {"state": None}},
            {"fleet": []},
            b"not json at all",
        ):
            node = Node(404, 200, body=body)
            client = self.make_client([self.start(node)])
            res = client.invalidate_with_result("t-acme")
            self.assertEqual(res.http_status, 200)
            # Never guessed: no state, so neither done nor retryable.
            self.assertIsNone(res.state)
            self.assertFalse(res.done)
            self.assertFalse(res.retryable)
            self.assertEqual(res.lagging, [])
            with self.assertRaises(InvalidateOutcomeError) as ctx:
                client.invalidate_tenant_auth_cache("t-acme")
            self.assertNotIsInstance(ctx.exception, InvalidatePendingError)
            self.assertNotIsInstance(ctx.exception, InvalidateUnavailableError)
            self.assertIn("unrecognised", str(ctx.exception))
            self.assertEqual(client.removed_nodes, [])
            client.close()

    def test_missing_optional_fields_read_as_defaults(self):
        """A body with only `state` is still decodable: the rest read as
        "not reported", never as a failure and never as a guess.
        """
        node = Node(404, 200, body={"fleet": {"state": "applied"}})
        client = self.make_client([self.start(node)])
        res = client.invalidate_with_result("t-acme")
        self.assertEqual(res.state, FleetState.APPLIED)
        self.assertTrue(res.done)
        self.assertEqual((res.invalidated, res.checked), (0, 0))
        self.assertEqual(res.scope, "")
        self.assertEqual((res.nodes_applied, res.nodes_total), (0, 0))
        self.assertEqual(res.lagging, [])
        self.assertIsNone(res.event_id)
        self.assertEqual(res.waited_ms, 0)

    def test_result_is_frozen_and_lagging_is_always_a_list(self):
        res = InvalidateTenantAuthCacheResult(http_status=202, state=FleetState.PENDING)
        with self.assertRaises(dataclasses.FrozenInstanceError):
            res.state = FleetState.APPLIED
        self.assertEqual(res.lagging, [])
        self.assertFalse(res.done)
        self.assertTrue(res.retryable)

    def test_non_fleet_aware_2xx_json_null_fleet_is_undecodable(self):
        node = Node(404, 200, body={"invalidated": 1, "fleet": None})
        client = self.make_client([self.start(node)])
        with self.assertRaises(InvalidateOutcomeError):
            client.invalidate_tenant_auth_cache("t-acme")


if __name__ == "__main__":
    unittest.main()
