import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
class H(BaseHTTPRequestHandler):
    def _record(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n).decode() if n else ""
        print(f"{self.command} {self.path} -> {body}", flush=True)
        return body
    def do_GET(self):
        self._record()
        payload = json.dumps({"id": "p1", "key": "p1", "name": "P", "endpoint": "http://x",
                              "weight": 10, "max_concurrency": 5, "max_queue_depth": 2,
                              "queue_wait_timeout_ms": 1000, "created_at": "", "updated_at": ""}).encode()
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload))); self.end_headers(); self.wfile.write(payload)
    def do_PUT(self):
        self._record()
        payload = b'{}'
        self.send_response(200); self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", "2"); self.end_headers(); self.wfile.write(payload)
    def log_message(self, *a): pass
ThreadingHTTPServer(("127.0.0.1", 18746), H).serve_forever()
