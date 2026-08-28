import json
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from hydra_sdk import HydraClient


class Node:
    def __init__(self, leader_status, endpoint_status):
        self.leader_status = leader_status
        self.endpoint_status = endpoint_status
        self.endpoint_calls = 0
        self.lock = threading.Lock()

    def set_endpoint(self, status):
        with self.lock:
            self.endpoint_status = status

    def handle(self, path, method, handler):
        if path == "/healthz/leader":
            handler.send_response(self.leader_status)
            handler.send_header("Content-Type", "application/json")
            handler.end_headers()
            if self.leader_status == 200:
                handler.wfile.write(b'{"leader":true}')
            return
        if path == "/api/v1/tenants/t-acme/auth/cache/invalidate" and method == "POST":
            with self.lock:
                self.endpoint_calls += 1
                status = self.endpoint_status
            if handler.headers.get("Authorization") != "Bearer sk-tenant-token":
                handler.send_response(401)
                handler.end_headers()
                return
            handler.send_response(status)
            handler.send_header("Content-Type", "application/json")
            handler.end_headers()
            if 200 <= status < 300:
                handler.wfile.write(b'{"invalidated":1,"tenant_id":"t-acme"}')
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
                self.send_response(200)
                self.end_headers()

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(("127.0.0.1", 0), BodyHandler)
        self.servers.append(server)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        client = self.make_client([f"http://127.0.0.1:{server.server_port}"])
        client.invalidate_tenant_auth_cache_keys("t-acme", ["key1", "key2"])
        self.assertEqual(seen["body"], {"api_keys": ["key1", "key2"]})


if __name__ == "__main__":
    unittest.main()
