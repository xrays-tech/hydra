#!/usr/bin/env python3
"""Mock LLM backend for Hydra e2e testing.
Returns OpenAI-compatible chat completions on POST /v1/chat/completions.
Listens on :9090. Stdlib only — no pip deps.
"""
import json
import os, sys
from http.server import HTTPServer, BaseHTTPRequestHandler

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(length) if length else b""
        try:
            req = json.loads(body)
            model = req.get("model", "unknown")
            messages = req.get("messages", [])
            prompt = messages[-1].get("content", "") if messages else ""
        except Exception:
            model, prompt = "parse-err", ""

        resp = {
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": f"Hello from mock LLM! (model={model}, prompt={prompt[:40]})"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": max(len(prompt) // 4, 5), "completion_tokens": 8, "total_tokens": max(len(prompt) // 4, 5) + 8},
        }
        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        """Mock /v1/models endpoint."""
        resp = {"object": "list", "data": [{"id": "gpt-4o", "object": "model"}]}
        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, fmt, *args):
        print(f"[mock-llm] {args[0] if args else ''}", file=sys.stderr, flush=True)

# Env-overridable: the default collides with a running dev stack (or with a
# second copy of this mock), and a mock that cannot bind is a harness that
# nobody can run.
MOCK_LLM_PORT = int(os.environ.get("MOCK_LLM_PORT", "9090"))

if __name__ == "__main__":
    print(f"[mock-llm] listening on 0.0.0.0:{MOCK_LLM_PORT}", flush=True)
    HTTPServer(("0.0.0.0", MOCK_LLM_PORT), Handler).serve_forever()
