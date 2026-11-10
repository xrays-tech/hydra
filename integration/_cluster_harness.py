#!/usr/bin/env python3
"""Shared plumbing for the multi-process shared-DB/Redis cluster drills.

Extracted from `test_shared_db_cluster.py` so the deterministic fanout drill and the
node-kill drill use the SAME `Node`, admin/data-plane request, seed and wait helpers instead
of two copies that drift. Nothing here is a product mock: the helpers only start real
`hydra` processes, talk HTTP to them, and read metrics.

The database schema/database is provisioned by `_shared_db.py`; the Redis relay and RESP
helpers live in `_redis_harness.py`. This module owns only the node/process surface.
"""

from __future__ import annotations

import json
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler

from _usage_env import usage_env

__all__ = [
    "Upstream",
    "Node",
    "free_port",
    "call",
    "wait_for",
    "seed_base",
    "rename_tenant",
    "parse_admin_list",
]


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def call(method, url, token=None, body=None, timeout=20, host=None):
    """One HTTP request. Returns `(status, headers, text)`; status 0 on transport failure."""
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
    except Exception as e:  # refused / reset / timeout
        return 0, {}, str(e)


class Upstream(BaseHTTPRequestHandler):
    """A provider endpoint (chat completion + model list for the breaker probe)."""

    def do_GET(self):
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
        body = json.dumps(
            {
                "id": "chatcmpl-harness",
                "object": "chat.completion",
                "choices": [
                    {
                        "index": 0,
                        "message": {"role": "assistant", "content": "ok"},
                        "finish_reason": "stop",
                    }
                ],
                "usage": {"prompt_tokens": 5, "completion_tokens": 8, "total_tokens": 13},
            }
        ).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, format, *args):  # noqa: A002 - base-class signature
        pass


class Node:
    """A real hydra process with its own admin/data ports, sharing `db_url`/`redis_url`."""

    def __init__(
        self,
        name,
        db_url,
        redis_url,
        log_path,
        *,
        admin_token,
        encryption_key,
        poll_ms=None,
        extra_env=None,
    ):
        self.name = name
        self.admin_port = free_port()
        self.data_port = free_port()
        self.db_url = db_url
        self.redis_url = redis_url
        self.log_path = log_path
        self.admin_token = admin_token
        self.encryption_key = encryption_key
        self.poll_ms = poll_ms
        self.extra_env = dict(extra_env or {})
        self.proc = None

    def env(self):
        # Drop every inherited HYDRA_*: a developer shell with a stale value would otherwise
        # silently rewrite the drill.
        env = {k: v for k, v in os.environ.items() if not k.startswith("HYDRA_")}
        env.update(usage_env())
        env.update(
            {
                "HYDRA_ADMIN_TOKEN": self.admin_token,
                "HYDRA_ENCRYPTION_KEY": self.encryption_key,
                "HYDRA_NODE_ID": self.name,
                "HYDRA_ADMIN_ADDR": f"127.0.0.1:{self.admin_port}",
                "HYDRA_LISTEN": f"127.0.0.1:{self.data_port}",
                "HYDRA_DB_URL": self.db_url,
                "HYDRA_REDIS_URL": self.redis_url,
                "HYDRA_REDIS_MODE": "single",
                "RUST_LOG": "warn",
            }
        )
        if self.poll_ms is not None:
            env["HYDRA_CONFIG_POLL_MS"] = str(self.poll_ms)
        env.update(self.extra_env)
        return env

    def start(self):
        with open(self.log_path, "w") as log:
            self.proc = subprocess.Popen(
                [os.environ.get("HYDRA_BIN", os.path.join(os.path.dirname(os.path.dirname(
                    os.path.abspath(__file__))), "target", "debug", "hydra"))],
                env=self.env(),
                stdout=log,
                stderr=subprocess.STDOUT,
            )
        return self

    def alive(self):
        return self.proc is not None and self.proc.poll() is None

    def kill(self):
        proc = self.proc
        if proc is not None and proc.poll() is None:
            os.kill(proc.pid, signal.SIGKILL)
            proc.wait(timeout=10)

    def admin(self, method, path, body=None, timeout=20):
        return call(
            method,
            f"http://127.0.0.1:{self.admin_port}/api/v1{path}",
            token=self.admin_token,
            body=body,
            timeout=timeout,
        )

    def admin_json(self, method, path, body=None, timeout=20):
        st, headers, out = self.admin(method, path, body, timeout)
        try:
            return st, json.loads(out) if out else None
        except json.JSONDecodeError:
            return st, out

    def healthy(self):
        return self.admin("GET", "/health", timeout=3)[0] == 200

    def leader_probe(self):
        # The admin gate runs before routing, so an unauthenticated unknown path is 401. Send the
        # admin token so the assertion observes the ROUTE's absence (404), not the gate.
        return call(
            "GET",
            f"http://127.0.0.1:{self.admin_port}/healthz/leader",
            token=self.admin_token,
            timeout=3,
        )[0]

    def catalog(self, host, timeout=10):
        st, _, out = call(
            "GET",
            f"http://127.0.0.1:{self.data_port}/v1/models",
            timeout=timeout,
            host=host,
        )
        try:
            ids = sorted(m["id"] for m in json.loads(out).get("data", []))
        except Exception:
            ids = None
        return st, ids, out

    def chat(self, api_key, host, model="echo", timeout=20):
        return call(
            "POST",
            f"http://127.0.0.1:{self.data_port}/v1/chat/completions",
            token=api_key,
            host=host,
            body={"model": model, "messages": [{"role": "user", "content": "hi"}]},
            timeout=timeout,
        )

    def status(self):
        st, body = self.admin_json("GET", "/cluster/status", timeout=5)
        return st, body if isinstance(body, dict) else {}

    def version(self):
        st, body = self.status()
        return int(body.get("config_version", -1)) if st == 200 else -1

    def tenants(self):
        st, body = self.admin_json("GET", "/tenants", timeout=10)
        return st, body

    def tenant_name(self, tenant_id):
        st, body = self.tenants()
        if st != 200 or not isinstance(body, list):
            return None
        for t in body:
            if t.get("id") == tenant_id:
                return t.get("name")
        return None

    def metric_values(self, name, needle=None):
        st, _, out = call(
            "GET",
            f"http://127.0.0.1:{self.admin_port}/metrics",
            token=self.admin_token,
        )
        if st != 200:
            return []
        values = []
        for line in out.splitlines():
            if not line.startswith(name) or line.startswith("#"):
                continue
            if needle and needle not in line:
                continue
            try:
                values.append(float(line.rsplit(" ", 1)[1]))
            except (ValueError, IndexError):
                pass
        return values

    def reload_source(self, source):
        return sum(self.metric_values("hydra_config_reload_total", f'source="{source}"'))

    def log_tail(self, n=1200):
        try:
            with open(self.log_path, errors="replace") as fh:
                return fh.read()[-n:]
        except OSError:
            return "<no log>"


def wait_for(cond, budget, what):
    deadline = time.time() + budget
    while time.time() < deadline:
        try:
            if cond():
                return True
        except Exception:
            pass
        time.sleep(0.05)
    print(f"   !!  timed out waiting for {what}", file=sys.stderr)
    return False


def parse_admin_list(text):
    try:
        body = json.loads(text)
        return body if isinstance(body, list) else None
    except Exception:
        return None


def seed_base(
    node,
    *,
    tenant_id,
    domain,
    model,
    upstream_port,
    prefix,
    tenant_name=None,
):
    """Provider + model + key + tenant + both grants, through the node's ADMIN API."""
    p = prefix
    name = tenant_name or f"{prefix} tenant"
    for path, payload in (
        (
            "/providers",
            {
                "id": f"p-{p}",
                "key": f"p-{p}",
                "name": f"{p} upstream",
                "endpoint": f"http://127.0.0.1:{upstream_port}",
                "weight": 1,
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/provider-models",
            {
                "id": f"pm-{p}",
                "key": model,
                "name": model,
                "provider_id": f"p-{p}",
                "status": 1,
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/provider-keys",
            {
                "id": f"pk-{p}",
                "provider_id": f"p-{p}",
                "api_key": "sk-upstream",
                "created_at": "",
            },
        ),
        (
            "/tenants",
            {
                "id": tenant_id,
                "name": name,
                "domain": domain,
                "cert_key": None,
                "cert_file": None,
                "enabled": True,
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/tenant-providers",
            {
                "id": f"tp-{p}",
                "tenant_id": tenant_id,
                "provider_id": f"p-{p}",
                "created_at": "",
                "updated_at": "",
            },
        ),
        (
            "/tenant-models",
            {
                "id": f"tm-{p}",
                "tenant_id": tenant_id,
                "model_key": model,
                "created_at": "",
                "updated_at": "",
            },
        ),
    ):
        st, _, out = node.admin("POST", path, payload, timeout=25)
        if st not in (200, 201):
            raise RuntimeError(f"seeding {path} -> HTTP {st} {out[:200]}")


def rename_tenant(node, tenant_id, new_name):
    """A management write on `node`: read the tenant, change its name, PUT it back."""
    st, _, out = node.admin("GET", f"/tenants/{tenant_id}", timeout=10)
    if st != 200:
        return st, out
    body = json.loads(out)
    body.pop("snapshot_stale", None)
    body["name"] = new_name
    st, _, out = node.admin("PUT", f"/tenants/{tenant_id}", body, timeout=25)
    return st, out
