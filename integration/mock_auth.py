#!/usr/bin/env python3
"""Mock tenant auth endpoint for Hydra e2e testing.
Always returns {"allowed": true} on POST — simulates a permissive tenant auth service.
Listens on :9091. Stdlib only.
"""
import json
import os, sys
from http.server import HTTPServer, BaseHTTPRequestHandler

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        # Always allow — this is a mock for e2e testing
        resp = {"allowed": True, "reason": "mock-auth-always-allow", "expires_in": 300}
        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        print(f"[mock-auth] {args[0] if args else ''}", file=sys.stderr, flush=True)

# Env-overridable: the default collides with a running dev stack (or with a
# second copy of this mock), and a mock that cannot bind is a harness that
# nobody can run.
MOCK_AUTH_PORT = int(os.environ.get("MOCK_AUTH_PORT", "9091"))

if __name__ == "__main__":
    print(f"[mock-auth] listening on 0.0.0.0:{MOCK_AUTH_PORT}", flush=True)
    HTTPServer(("0.0.0.0", MOCK_AUTH_PORT), Handler).serve_forever()
