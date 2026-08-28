"""Cluster-aware Hydra tenant SDK.

The client accepts one or more Hydra cluster node base URLs. Before each
invalidation it probes ``/healthz/leader`` to discover the current active
leader. If the chosen node fails, the client automatically rotates to the next
available node and temporarily removes the dead node from the active pool. A
background thread periodically probes removed nodes and adds them back once
they become reachable again.
"""

from __future__ import annotations

import json
import threading
import urllib.error
import urllib.request
from typing import Callable, List, Optional

__all__ = ["HTTPError", "HydraClient"]

UrlOpen = Callable[..., object]


class HTTPError(Exception):
    """Raised when the server responds with a non-2xx status."""

    def __init__(self, method: str, url: str, status: int, body: str = "") -> None:
        super().__init__(f"{method} {url}: unexpected HTTP {status}: {body[:300]}")
        self.method = method
        self.url = url
        self.status = status
        self.body = body


class HydraClient:
    """A concurrency-safe Hydra tenant SDK client with automatic failover."""

    def __init__(
        self,
        token: str,
        nodes: List[str],
        *,
        probe_timeout: float = 2.0,
        request_timeout: float = 10.0,
        recheck_interval: float = 30.0,
        disable_background_recheck: bool = False,
        urlopen: Optional[UrlOpen] = None,
    ) -> None:
        if not token or not token.strip():
            raise ValueError("token is required")
        if not nodes:
            raise ValueError("at least one node is required")

        self._token = token.strip()
        self._probe_timeout = probe_timeout
        self._request_timeout = request_timeout
        self._recheck_interval = recheck_interval
        self._urlopen = urlopen or urllib.request.urlopen

        self._lock = threading.RLock()
        self._active: List[str] = []
        self._removed: List[str] = []
        seen = set()
        for node in nodes:
            node = node.strip().rstrip("/")
            if not node:
                continue
            if node in seen:
                continue
            seen.add(node)
            self._active.append(node)
        if not self._active:
            raise ValueError("no valid node URLs")

        self._stop = threading.Event()
        self._thread: Optional[threading.Thread] = None
        if not disable_background_recheck:
            self._thread = threading.Thread(
                target=self._recheck_loop, name="hydra-sdk-recheck", daemon=True
            )
            self._thread.start()

    @property
    def nodes(self) -> List[str]:
        with self._lock:
            return list(self._active)

    @property
    def removed_nodes(self) -> List[str]:
        with self._lock:
            return list(self._removed)

    def close(self) -> None:
        """Stop the background rechecker thread."""
        self._stop.set()
        if self._thread is not None and self._thread.is_alive():
            self._thread.join(timeout=5.0)

    def __enter__(self) -> "HydraClient":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        self.close()

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------
    def invalidate_tenant_auth_cache(self, tenant_id: str) -> None:
        """Invalidate the tenant's complete auth cache."""
        self._invalidate(tenant_id, None)

    def invalidate_tenant_auth_cache_keys(self, tenant_id: str, api_keys: List[str]) -> None:
        """Invalidate only the supplied api-keys for the tenant."""
        if not api_keys:
            self.invalidate_tenant_auth_cache(tenant_id)
            return
        self._invalidate(tenant_id, {"api_keys": api_keys})

    def invalidate(self, tenant_id: str) -> None:
        """Alias for invalidate_tenant_auth_cache."""
        self.invalidate_tenant_auth_cache(tenant_id)

    def invalidate_tenant_cache(self, tenant_id: str) -> None:
        """Alias for invalidate_tenant_auth_cache."""
        self.invalidate_tenant_auth_cache(tenant_id)

    def invalidate_cache(self, tenant_id: str) -> None:
        """Alias for invalidate_tenant_auth_cache."""
        self.invalidate_tenant_auth_cache(tenant_id)

    def invalidate_cache_keys(self, tenant_id: str, api_keys: List[str]) -> None:
        """Alias for invalidate_tenant_auth_cache_keys."""
        self.invalidate_tenant_auth_cache_keys(tenant_id, api_keys)

    def probe_removed_nodes(self) -> None:
        """Check quarantined nodes and add reachable ones back."""
        with self._lock:
            removed = list(self._removed)
        if not removed:
            return

        restored = []
        for node in removed:
            alive, _ = self._probe_leader(node)
            if alive:
                restored.append(node)

        if not restored:
            return
        with self._lock:
            for node in restored:
                if node not in self._active:
                    self._active.append(node)
                if node in self._removed:
                    self._removed.remove(node)

    # ------------------------------------------------------------------
    # Internals
    # ------------------------------------------------------------------
    def _recheck_loop(self) -> None:
        while not self._stop.wait(self._recheck_interval):
            self.probe_removed_nodes()

    def _invalidate(self, tenant_id: str, body: Optional[dict]) -> None:
        if not tenant_id or not tenant_id.strip():
            raise ValueError("tenant_id is required")
        tenant_id = tenant_id.strip()

        with self._lock:
            nodes = list(self._active)
        if not nodes:
            raise RuntimeError(
                "no available nodes (removed: {})".format(", ".join(self.removed_nodes))
            )

        leaders: List[str] = []
        alive: List[str] = []
        seen = set()
        for node in nodes:
            ok, leader = self._probe_leader(node)
            if not ok:
                # Do not quarantine nodes because the caller supplied a
                # cancelled/closed context; this Python API has no context, so
                # any probe failure is treated as a node failure.
                self._remove_node(node)
                continue
            if node in seen:
                continue
            seen.add(node)
            alive.append(node)
            if leader:
                leaders.append(node)

        attempts = list(leaders)
        for node in alive:
            if node not in attempts:
                attempts.append(node)

        if not attempts:
            raise RuntimeError(
                "no reachable nodes (removed: {})".format(", ".join(self.removed_nodes))
            )

        errors = []
        for node in attempts:
            try:
                self._do_invalidate(node, tenant_id, body)
                return
            except HTTPError as exc:
                if exc.status in (401, 403):
                    raise
                if self._is_node_failure(exc):
                    self._remove_node(node)
                errors.append(exc)
            except Exception as exc:  # noqa: BLE001 - network/transport errors
                if self._is_node_failure(exc):
                    self._remove_node(node)
                errors.append(exc)

        raise RuntimeError(f"all {len(attempts)} node(s) failed: {errors}")

    def _do_invalidate(self, node: str, tenant_id: str, body: Optional[dict]) -> None:
        import urllib.parse

        endpoint = (
            node
            + "/api/v1/tenants/"
            + urllib.parse.quote(tenant_id, safe="")
            + "/auth/cache/invalidate"
        )
        data = None
        headers = {
            "Authorization": "Bearer " + self._token,
            "Accept": "application/json",
        }
        if body is not None:
            data = json.dumps(body, separators=(",", ":")).encode("utf-8")
            headers["Content-Type"] = "application/json"

        req = urllib.request.Request(
            endpoint, data=data, headers=headers, method="POST"
        )
        try:
            resp = self._urlopen(req, timeout=self._request_timeout)
        except urllib.error.HTTPError as exc:
            body = exc.read().decode("utf-8", "replace")
            raise HTTPError("POST", endpoint, exc.code, body) from exc
        with resp:
            resp.read()

    def _probe_leader(self, node: str):
        req = urllib.request.Request(node + "/healthz/leader", method="GET")
        try:
            resp = self._urlopen(req, timeout=self._probe_timeout)
        except urllib.error.HTTPError as exc:
            with exc:
                exc.read()
            return True, exc.code == 200
        except Exception:
            return False, False
        with resp:
            resp.read()
        return True, resp.status == 200

    def _remove_node(self, node: str) -> None:
        with self._lock:
            if node in self._active:
                self._active.remove(node)
            if node not in self._removed:
                self._removed.append(node)

    @staticmethod
    def _is_node_failure(exc: Exception) -> bool:
        if isinstance(exc, HTTPError):
            return exc.status >= 500 or exc.status in (404, 405)
        if isinstance(exc, (urllib.error.URLError, OSError, TimeoutError)):
            return True
        return True
