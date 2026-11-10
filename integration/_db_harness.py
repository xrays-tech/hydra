#!/usr/bin/env python3
"""A cuttable TCP relay in front of PostgreSQL/MySQL, plus URL rewriting.

The DB backend is protocol-agnostic here: the relay is a raw TCP pump, exactly what the
Redis drift uses. The node is configured with a URL whose host:port is the relay, while the
relay forwards to the real database host:port. `cut()` refuses new connections and severs the
pooled ones (a network partition from the node's side), `restore()` lets it reconnect.

The per-run schema/database comes from `_shared_db.py`; `rewrite_db_url` keeps its database
name, credentials and query string while replacing only the authority.
"""

from __future__ import annotations

import socket
import socketserver
import threading
import time
from urllib.parse import urlsplit, urlunsplit

__all__ = ["DbRelay", "rewrite_db_url", "db_host_port"]


def db_host_port(url: str) -> tuple[str, int]:
    p = urlsplit(url)
    if not p.hostname or not p.port:
        raise ValueError(f"cannot parse database URL {url!r} as scheme://host:port/db")
    return p.hostname, p.port


def rewrite_db_url(url: str, host: str, port: int) -> str:
    """Replace only the URL authority (keep userinfo, database path and query)."""
    p = urlsplit(url)
    userinfo = ""
    if "@" in p.netloc:
        userinfo = p.netloc.rsplit("@", 1)[0] + "@"
    netloc = f"{userinfo}{host}:{port}"
    return urlunsplit((p.scheme, netloc, p.path, p.query, p.fragment))


class DbRelay:
    """Raw TCP relay for PostgreSQL/MySQL with `cut()` / `restore()`.

    `cut()` closes the accepted sockets and refuses new ones, so a pooled connection is
    observed as broken immediately instead of waiting for a protocol timeout. `cut(blackhole=True)`
    accepts and then stays silent (use it to exercise a client-side statement timeout).
    """

    def __init__(self, upstream_host: str, upstream_port: int):
        self.upstream = (upstream_host, upstream_port)
        self.server: socketserver.ThreadingTCPServer | None = None
        self.port: int | None = None
        self.blocked = False
        self.blackhole = False
        self.sockets: list[socket.socket] = []
        self.lock = threading.Lock()
        self.accepted = 0
        self.refused = 0

    def start(self) -> int:
        relay = self

        class Handler(socketserver.BaseRequestHandler):
            def handle(self) -> None:
                with relay.lock:
                    if relay.blocked:
                        relay.refused += 1
                    else:
                        relay.accepted += 1
                if relay.blocked:
                    return  # close immediately: the pool's next query is an ordinary error
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
            sockets, self.sockets = self.sockets, []
        for s in sockets:
            try:
                s.close()
            except OSError:
                pass

    def cut(self, blackhole: bool = False) -> None:
        self.blocked = True
        self.blackhole = blackhole
        self._sever()

    def restore(self) -> None:
        self.blocked = False
        self.blackhole = False

    def stop(self) -> None:
        self.blocked = True
        self.blackhole = False
        self._sever()
        if self.server is not None:
            try:
                self.server.shutdown()
                self.server.server_close()
            except Exception:
                pass
