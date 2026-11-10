#!/usr/bin/env python3
"""Real-Redis helpers shared by the integration drills.

Redis is a REAL service in these drills (the in-process double is forbidden).
This module provides only the plumbing the drills need: a stdlib RESP client, a
per-run DB selection/flush, and a cuttable TCP relay so an outage can be
simulated without touching the Redis server the developer's local stack uses.

`HYDRA_TEST_REDIS_URL` is required; there is no default and no silent skip.
"""

from __future__ import annotations

import os
import socket
import socketserver
import threading
import time
from urllib.parse import urlparse

__all__ = [
    "RedisSetupError",
    "endpoint",
    "require_url",
    "redis_cmd",
    "flush_db",
    "db_url",
    "RedisRelay",
]


class RedisSetupError(RuntimeError):
    """Missing/unreachable test Redis, reported as CANNOT VERIFY by drills."""


def require_url() -> str:
    value = os.environ.get("HYDRA_TEST_REDIS_URL", "").strip()
    if not value:
        raise RedisSetupError(
            "HYDRA_TEST_REDIS_URL is not set; these drills need a REAL Redis "
            "(start one with `docker run -d -p 6379:6379 redis:7-alpine`, then "
            "export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6379)."
        )
    return value


def endpoint(url: str | None = None) -> tuple[str, int]:
    raw = url or require_url()
    parsed = urlparse(raw)
    if not parsed.hostname or not parsed.port:
        raise RedisSetupError(
            f"cannot parse HYDRA_TEST_REDIS_URL={raw!r} as redis://host:port"
        )
    return parsed.hostname, parsed.port


def redis_cmd(sock: socket.socket, *parts: object) -> bytes:
    """Send one RESP command and return the raw reply (bounded, 2 s)."""
    payload = b"*%d\r\n" % len(parts)
    for part in parts:
        b = str(part).encode()
        payload += b"$%d\r\n%s\r\n" % (len(b), b)
    sock.sendall(payload)
    sock.settimeout(2)
    try:
        return sock.recv(65536)
    except socket.timeout:
        return b""


def _connect(url: str | None = None) -> socket.socket:
    host, port = endpoint(url)
    try:
        return socket.create_connection((host, port), timeout=5)
    except OSError as e:
        raise RedisSetupError(
            f"test Redis {host}:{port} is unreachable ({e}); start a real Redis and "
            f"set HYDRA_TEST_REDIS_URL"
        ) from e


def flush_db(db: int, url: str | None = None) -> str:
    """Select `db` and FLUSH it so a previous run cannot decide this one."""
    sock = _connect(url)
    try:
        redis_cmd(sock, "SELECT", db)
        before = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
        redis_cmd(sock, "FLUSHDB")
        after = redis_cmd(sock, "DBSIZE").decode(errors="replace").strip().splitlines()[-1]
    finally:
        sock.close()
    return f"db {db} flushed (was {before} keys, now {after})"


def db_url(db: int, url: str | None = None) -> str:
    return f"{(url or require_url()).rstrip('/')}/{db}"


class RedisRelay:
    """A TCP relay in front of the real Redis that can be cut and restored.

    Point every drill node at the relay, not at Redis directly. `set_blocked`
    refuses new connections and severs the pooled ones (a network partition from
    the node's point of view). `set_blackhole` accepts and stays silent, which is
    the mode the Redis command timeout exists for.
    """

    def __init__(self, host: str, port: int):
        self.upstream = (host, port)
        self.server: socketserver.ThreadingTCPServer | None = None
        self.port: int | None = None
        self.blocked = False
        self.blackhole = False
        self.sockets: list[socket.socket] = []
        self.lock = threading.Lock()

    def start(self) -> int:
        relay = self

        class Handler(socketserver.BaseRequestHandler):
            def handle(self) -> None:
                if relay.blocked:
                    return
                if relay.blackhole:
                    while relay.blackhole:
                        time.sleep(0.1)
                    return
                try:
                    up = socket.create_connection(relay.upstream, timeout=5)
                except OSError:
                    return
                with relay.lock:
                    relay.sockets.extend([self.request, up])
                threading.Thread(
                    target=relay._pump, args=(self.request, up), daemon=True
                ).start()
                relay._pump(up, self.request)

        self.server = socketserver.ThreadingTCPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.port = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return self.port

    @staticmethod
    def _pump(src: socket.socket, dst: socket.socket) -> None:
        try:
            while True:
                chunk = src.recv(65536)
                if not chunk:
                    break
                dst.sendall(chunk)
        except OSError:
            pass
        finally:
            for s in (src, dst):
                try:
                    s.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    def _sever(self) -> None:
        with self.lock:
            socks, self.sockets = self.sockets, []
        for s in socks:
            try:
                s.close()
            except OSError:
                pass

    def set_blocked(self, blocked: bool) -> None:
        self.blocked = blocked
        if blocked:
            self._sever()

    def set_blackhole(self, on: bool) -> None:
        self.blackhole = on
        if on:
            self._sever()

    def stop(self) -> None:
        self.set_blocked(True)
        self.set_blackhole(False)
        if self.server is not None:
            try:
                self.server.shutdown()
                self.server.server_close()
            except Exception:
                pass
