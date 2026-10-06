"""Cluster-aware Hydra tenant SDK.

The client accepts one or more Hydra **data-plane** node base URLs (the address
that serves ``/v1/*``, default port 8080 — not the admin port 8081).

The invalidation endpoint is answered **locally by every data-plane node**: it
clears that node's cache synchronously and fans the invalidation out over the
shared bus, so it does NOT need to reach the cluster leader (design
``dev-docs/design-tenant-api.md`` §6.4 decision A-1). Leader preference via the
``/healthz/leader`` probe is therefore a legacy optimization, not a requirement.

Before each invalidation the client still probes ``/healthz/leader`` and tries
nodes that report themselves leader first. That probe is an **admin-port** route:
a node list pointing at data-plane ports gets a 404, which counts as "alive, not
the leader", so the request is sent directly. If the chosen node fails, the
client automatically rotates to the next available node and temporarily removes
the dead node from the active pool. A background thread periodically probes
removed nodes and adds them back once they become reachable again.

The endpoint's three-state contract
-----------------------------------

A 2xx is NOT the same as "done". Per ``dev-docs/tenant-api-integration.md`` §5.2
the fleet state in the response body is what the caller must branch on, and
:class:`InvalidateTenantAuthCacheResult` surfaces it verbatim::

    HTTP  fleet.state    meaning                                caller must
    200   applied        every live node applied it             done
    200   single_node    this node IS the whole data plane      done
    202   pending        published, not confirmed everywhere   retry (idempotent) or report lagging
    503   unavailable    the cluster was NOT notified           retry / escalate, quoting trace_id

Two consequences worth stating because the old behaviour got both wrong:

* ``202 pending`` is a *retryable* outcome, never silent success. The wrapper
  methods raise :class:`InvalidatePendingError` for it; callers that want to
  branch on the state themselves should call :meth:`HydraClient.invalidate_with_result`.
* ``503 unavailable`` is NOT a node failure. The node answered — it is alive and
  its own cache was cleared — so it is neither quarantined nor rotated away
  from, and the "the cluster was not notified" signal is preserved as
  :class:`InvalidateUnavailableError`. Only transport failures and
  non-fleet-aware server errors still trigger failover.
"""

from __future__ import annotations

import json
import socket
import threading
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Callable, List, Optional

__all__ = [
    "MAX_TIMEOUT_MS",
    "MIN_TIMEOUT_MS",
    "FleetState",
    "HTTPError",
    "HydraClient",
    "InvalidateOutcomeError",
    "InvalidatePendingError",
    "InvalidateTenantAuthCacheResult",
    "InvalidateUnavailableError",
    "WaitMode",
    "is_done_state",
    "is_retryable_state",
]

UrlOpen = Callable[..., object]

#: Smallest accepted ``timeout_ms``, mirroring the server
#: (``crates/hydra-server/src/tenant_api/handlers.rs``: ``(1..=MAX_CONVERGE_MS)``).
MIN_TIMEOUT_MS = 1

#: Largest accepted ``timeout_ms``, mirroring the server's ``MAX_CONVERGE_MS``.
#: Above this one caller could hold a worker (and a share of the shared Redis)
#: for as long as it liked.
MAX_TIMEOUT_MS = 60_000


class WaitMode:
    """The ``wait`` query parameter (tenant contract §5.2).

    The values are the server's own tokens, exposed verbatim. The server does
    **not** trim this value (``crates/hydra-server/src/tenant_api/handlers.rs``
    matches it exactly and answers ``400 invalid_wait`` otherwise), so the
    client validates it exactly too.
    """

    #: Block until every live node has applied the event. This is the server's
    #: own default, so setting it is equivalent to leaving ``wait_mode`` unset —
    #: except that it is then sent explicitly.
    CONVERGED = "converged"

    #: Publish and answer ``202`` immediately without waiting, which is the
    #: bulk/script mode. The response's ``event_id`` is for later reconciliation.
    #: A node built without a cluster backbone has no fleet to notify and always
    #: answers ``200 single_node`` (``crates/hydra-server/src/tenant_api/handlers.rs``),
    #: so ``pending`` is the normal outcome, not a guaranteed one.
    NONE = "none"

    ALL = (CONVERGED, NONE)


class FleetState:
    """The server's ``fleet.state`` token.

    The four values are the server's own (``crates/hydra-server/src/cluster/events.rs``,
    ``FleetReport.state``) and are exposed verbatim — there is deliberately no
    mapping layer, because a mapping is a second owner of the contract and two
    owners always eventually disagree.
    """

    #: Every live node has applied the invalidation. HTTP 200.
    APPLIED = "applied"

    #: This node IS the whole data plane, so its own clear is the whole answer.
    #: HTTP 200.
    SINGLE_NODE = "single_node"

    #: Published but not confirmed everywhere. HTTP 202. NOT an error, and not
    #: yet "done": retry (the call is idempotent) or hand ``lagging`` to the
    #: operator.
    PENDING = "pending"

    #: An invalidation channel exists but did not answer, so the cluster was NOT
    #: notified. HTTP 503. The node itself is alive.
    UNAVAILABLE = "unavailable"

    ALL = (APPLIED, SINGLE_NODE, PENDING, UNAVAILABLE)


def is_done_state(state: Optional[str]) -> bool:
    """Return whether ``state`` means the invalidation is actually complete.

    Only ``applied`` and ``single_node`` qualify: ``pending`` is still in
    flight and ``unavailable`` never reached the fleet. Any other value — a
    state the SDK does not know, or ``None`` because the body could not be
    decoded — is **not** done.
    """
    return state in (FleetState.APPLIED, FleetState.SINGLE_NODE)


def is_retryable_state(state: Optional[str]) -> bool:
    """Return whether the caller should try the invalidation again.

    ``pending`` and ``unavailable`` are both retryable — the call is idempotent
    — while a done state has nothing left to retry.
    """
    return state in (FleetState.PENDING, FleetState.UNAVAILABLE)


def _is_timeout_error(exc: Exception) -> bool:
    """Return True when exc is (or wraps) a timeout/cancellation error.

    A request timeout is not evidence that a node is dead (it may simply be
    slow), so it must not cause the node to be quarantined. This aligns with
    the Go SDK, which treats context.DeadlineExceeded/Canceled as non-failures.
    ``socket.timeout`` is the same class as ``TimeoutError`` on Python 3.10+
    but a distinct OSError subclass on 3.9, so both are checked.
    """
    if isinstance(exc, (TimeoutError, socket.timeout)):
        return True
    reason = getattr(exc, "reason", None)
    if isinstance(reason, (TimeoutError, socket.timeout)):
        return True
    return False


def _header_value(headers: object, name: str) -> Optional[str]:
    """Read one response header, or None when it is absent or empty.

    ``urllib`` hands back an ``http.client.HTTPMessage`` (case-insensitive
    ``get``), but a caller-supplied ``urlopen`` may hand back anything, so
    ``getheader`` is tried as well. An absent ``X-Hydra-Trace-Id`` is ``None``
    rather than ``""``, so a result never carries an empty trace id that looks
    like a real one.
    """
    if headers is None:
        return None
    value = None
    getter = getattr(headers, "get", None)
    if callable(getter):
        value = getter(name)
    if value is None:
        getheader = getattr(headers, "getheader", None)
        if callable(getheader):
            value = getheader(name)
    if value is None:
        return None
    text = value if isinstance(value, str) else str(value)
    text = text.strip()
    return text or None


class HTTPError(Exception):
    """Raised when the server responds with a status that is NOT a fleet report.

    That is a non-2xx answer whose body is not the documented invalidate
    envelope: ``401`` / ``403`` / ``413`` / ``429``, a bare ``500``, and so on.
    A ``503 unavailable`` does **not** produce one — it carries a normal fleet
    body, so it is reported as a result (or as
    :class:`InvalidateUnavailableError` by the wrappers).
    """

    def __init__(
        self,
        method: str,
        url: str,
        status: int,
        body: str = "",
        *,
        trace_id: Optional[str] = None,
    ) -> None:
        # The trace id is carried on the error itself because the tenant
        # contract §4.2 tells callers to quote `X-Hydra-Trace-Id` when reporting
        # a problem — and an HTTP error is a problem.
        trace = f" (X-Hydra-Trace-Id: {trace_id})" if trace_id else ""
        super().__init__(
            f"{method} {url}: unexpected HTTP {status}: {body[:300]}{trace}"
        )
        self.method = method
        self.url = url
        self.status = status
        self.body = body
        self.trace_id = trace_id


@dataclass(frozen=True)
class InvalidateTenantAuthCacheResult:
    """The full outcome of one invalidation.

    It is deliberately not a boolean: the endpoint has a three-state contract
    and every one of the three tells the caller to do something different.
    """

    #: The status the winning node answered with: 200 (applied / single_node),
    #: 202 (pending) or 503 (unavailable) for the three documented outcomes, and
    #: the node's actual 2xx when its body was not the documented envelope (in
    #: which case ``state`` is ``None``).
    http_status: int

    #: The fleet state, verbatim from the response body. ``None`` only when the
    #: body could not be decoded into the documented shape — the SDK never
    #: guesses a state. Use :attr:`done` rather than the HTTP status.
    state: Optional[str] = None

    #: How many entries THIS node removed from its own cache. Not a fleet
    #: count. ``invalidated == 0`` with ``checked > 0`` means those keys were
    #: not cached on this node, which is not a failure.
    invalidated: int = 0

    #: How many keys the request named (0 for a whole-tenant clear).
    checked: int = 0

    #: ``"keys"`` or ``"tenant"``.
    scope: str = ""

    #: The confirmed / live node counts.
    nodes_applied: int = 0
    nodes_total: int = 0

    #: The nodes that did not confirm. Always a list, never None, so callers can
    #: iterate unconditionally. Non-empty only for ``pending``, or for an
    #: ``unavailable`` whose convergence barrier (rather than whose publish)
    #: failed — and even then only when the server actually looked: with
    #: ``wait=none`` nobody looked and every live node is listed, and a
    #: ``nodes_total`` of 0 means the server could not enumerate the fleet.
    lagging: List[str] = field(default_factory=list)

    #: This invalidation's id on the internal bus, for later reconciliation.
    #: ``None`` when the server reported none (``single_node``, and
    #: ``unavailable`` when the publish itself failed).
    event_id: Optional[str] = None

    #: How long the server actually spent waiting for the fleet, in ms. ``0``
    #: when the server reported none (a ``single_node`` answer waits for
    #: nothing), and ``None`` only when no body was decoded at all.
    waited_ms: Optional[int] = None

    #: The ``X-Hydra-Trace-Id`` response header — quote it in a support request
    #: (§4.2). ``None`` when the node did not send one.
    trace_id: Optional[str] = None

    #: The base URL of the node that answered. ``trace_id`` plus ``node`` is what
    #: makes a partial failure diagnosable.
    node: str = ""

    @property
    def done(self) -> bool:
        """Whether the invalidation is complete (``applied`` or ``single_node``)."""
        return is_done_state(self.state)

    @property
    def retryable(self) -> bool:
        """Whether the caller should retry (``pending`` or ``unavailable``)."""
        return is_retryable_state(self.state)


def _waited_text(result: "InvalidateTenantAuthCacheResult") -> str:
    """Render ``waited_ms`` for an error message, tolerating its absence."""
    return "unknown" if result.waited_ms is None else "{}ms".format(result.waited_ms)


class InvalidateOutcomeError(Exception):
    """Base class for the non-done outcomes the wrappers raise.

    Subclassing one base lets a caller catch either outcome with a single
    ``except`` while still telling them apart with ``isinstance`` (or by
    catching :class:`InvalidatePendingError` /
    :class:`InvalidateUnavailableError` directly).

    The result is always attached as :attr:`result`, and every message names the
    state, the node, the node counts, ``event_id`` and the trace id, because
    those five are what makes the outcome diagnosable.
    """

    def __init__(self, result: InvalidateTenantAuthCacheResult) -> None:
        self.result = result
        super().__init__(self._message(result))

    @staticmethod
    def _message(result: InvalidateTenantAuthCacheResult) -> str:
        return (
            "auth cache invalidation did not complete: node {node} answered HTTP {status} "
            "with unrecognised fleet.state={state!r} ({applied}/{total} nodes applied, "
            "event_id {event_id!r}, trace_id {trace!r})".format(
                node=result.node,
                status=result.http_status,
                state=result.state,
                applied=result.nodes_applied,
                total=result.nodes_total,
                event_id=result.event_id,
                trace=result.trace_id,
            )
        )


class InvalidatePendingError(InvalidateOutcomeError):
    """Raised for HTTP 202 ``pending``: published, but not confirmed everywhere.

    It is NOT a failure of the request — it is an incomplete outcome, and the
    caller must retry or hand the lagging nodes to an operator. It exists as a
    distinct type so that callers who previously treated "no error" as "done"
    can no longer do so by accident, while still being able to tell it apart
    from a hard error.
    """

    @staticmethod
    def _message(result: InvalidateTenantAuthCacheResult) -> str:
        return (
            "auth cache invalidation is pending, not complete: node {node} answered "
            "HTTP {status} fleet.state='pending' ({applied}/{total} nodes applied, "
            "waited {waited}, lagging {lagging}, event_id {event_id!r}, "
            "trace_id {trace!r})".format(
                node=result.node,
                status=result.http_status,
                applied=result.nodes_applied,
                total=result.nodes_total,
                waited=_waited_text(result),
                lagging=result.lagging,
                event_id=result.event_id,
                trace=result.trace_id,
            )
        )


class InvalidateUnavailableError(InvalidateOutcomeError):
    """Raised for HTTP 503 ``unavailable``: THE CLUSTER WAS NOT NOTIFIED.

    An invalidation channel exists on that node but could not do its job. The
    node itself answered, which is why this is not a node failure: the node is
    not quarantined and the client does NOT rotate away from it.
    """

    @staticmethod
    def _message(result: InvalidateTenantAuthCacheResult) -> str:
        return (
            "auth cache invalidation is unavailable: node {node} answered HTTP {status} "
            "fleet.state='unavailable', so the cluster was NOT notified "
            "({applied}/{total} nodes applied, waited {waited}, event_id {event_id!r}, "
            "trace_id {trace!r}); retry or escalate to an operator".format(
                node=result.node,
                status=result.http_status,
                applied=result.nodes_applied,
                total=result.nodes_total,
                waited=_waited_text(result),
                event_id=result.event_id,
                trace=result.trace_id,
            )
        )


#: Sentinel for "the field was present but is not the documented type".
_INVALID = object()


def _int_field(source: dict, key: str):
    """Read a documented integer field out of a decoded body.

    An absent key and a JSON ``null`` both mean "not reported" and read as 0 —
    the same thing the Go SDK's nil pointers do. Anything else that is not an
    integer (including ``true``, which Python would otherwise accept as 1) makes
    the whole body undecodable, so no number is ever guessed.
    """
    if key not in source:
        return 0
    value = source[key]
    if value is None:
        return 0
    if isinstance(value, bool) or not isinstance(value, int):
        return _INVALID
    return value


def _parse_invalidate_body(raw: bytes) -> Optional[dict]:
    """Decode the documented invalidate response envelope, strictly.

    Returns the fleet fields as a dict, or ``None`` when the body is not JSON,
    is not an object, has no ``fleet`` object, or carries a ``state`` outside
    the documented four. Callers then report "no state" instead of guessing one
    — guessing is how a response nobody understood becomes "done".
    """
    if isinstance(raw, (bytes, bytearray)):
        try:
            text = bytes(raw).decode("utf-8")
        except UnicodeDecodeError:
            return None
    else:
        text = raw
    try:
        decoded = json.loads(text)
    except ValueError:
        return None
    if not isinstance(decoded, dict):
        return None
    fleet = decoded.get("fleet")
    if not isinstance(fleet, dict):
        return None
    state = fleet.get("state")
    if not isinstance(state, str) or state not in FleetState.ALL:
        return None

    invalidated = _int_field(decoded, "invalidated")
    checked = _int_field(decoded, "checked")
    nodes_applied = _int_field(fleet, "nodes_applied")
    nodes_total = _int_field(fleet, "nodes_total")
    waited_ms = _int_field(fleet, "waited_ms")
    for value in (invalidated, checked, nodes_applied, nodes_total, waited_ms):
        if value is _INVALID:
            return None

    scope = decoded.get("scope")
    if scope is None:
        scope = ""
    if not isinstance(scope, str):
        return None

    event_id = fleet.get("event_id")
    if event_id is not None and not isinstance(event_id, str):
        return None

    lagging = fleet.get("lagging")
    if lagging is None:
        lagging = []
    if not isinstance(lagging, list) or any(not isinstance(n, str) for n in lagging):
        return None

    return {
        "state": state,
        "invalidated": invalidated,
        "checked": checked,
        "scope": scope,
        "nodes_applied": nodes_applied,
        "nodes_total": nodes_total,
        "lagging": list(lagging),
        "event_id": event_id,
        "waited_ms": waited_ms,
    }


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
        wait_mode: Optional[str] = None,
        timeout_ms: Optional[int] = None,
    ) -> None:
        """Create a client.

        ``wait_mode`` is the ``wait`` query parameter sent on every invalidation
        request. Accepts ``WaitMode.CONVERGED`` or ``WaitMode.NONE``; any other
        value is rejected here rather than by a remote ``400 invalid_wait``.

        ``timeout_ms`` is this request's budget in milliseconds for waiting on
        fleet confirmation. Must be in ``MIN_TIMEOUT_MS..MAX_TIMEOUT_MS``
        (1..60000) inclusive.

        Leaving either ``None`` (the default) sends NO such parameter at all, so
        the server applies its own default: ``wait=converged`` with a 2000 ms
        budget (``HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS``,
        ``crates/hydra-server/src/tenant_api/mod.rs``). Note the difference
        between "unset" (server default) and ``WaitMode.NONE`` (do not wait at
        all): the latter is a statement about behaviour, the former is not.
        """
        if not token or not token.strip():
            raise ValueError("token is required")
        if not nodes:
            raise ValueError("at least one node is required")

        # The wait/timeout_ms pair is validated rather than clamped or ignored:
        # the server treats a bad value as a hard 400, and silently substituting
        # a default would leave the caller believing it had asked for (or
        # skipped) a wait it never got.
        if wait_mode is not None:
            if not isinstance(wait_mode, str) or wait_mode not in WaitMode.ALL:
                raise ValueError(
                    "invalid wait_mode {!r}: must be exactly {!r} or {!r} "
                    "(the server does not trim this value)".format(
                        wait_mode, WaitMode.CONVERGED, WaitMode.NONE
                    )
                )
        if timeout_ms is not None:
            # isinstance(True, int) is True in Python, so a bool is rejected
            # explicitly: it is a caller bug, not a budget.
            if (
                isinstance(timeout_ms, bool)
                or not isinstance(timeout_ms, int)
                or not MIN_TIMEOUT_MS <= timeout_ms <= MAX_TIMEOUT_MS
            ):
                raise ValueError(
                    "invalid timeout_ms {!r}: must be an int in {}..{} inclusive, "
                    "or None to use the server default".format(
                        timeout_ms, MIN_TIMEOUT_MS, MAX_TIMEOUT_MS
                    )
                )

        self._token = token.strip()
        self._probe_timeout = probe_timeout
        self._request_timeout = request_timeout
        self._recheck_interval = recheck_interval
        self._urlopen = urlopen or urllib.request.urlopen
        self._wait_mode = wait_mode
        self._timeout_ms = timeout_ms

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
    def invalidate_with_result(
        self, tenant_id: str, api_keys: Optional[List[str]] = None
    ) -> InvalidateTenantAuthCacheResult:
        """Invalidate the tenant's auth cache and return the full structured outcome.

        Prefer this method: it is the only one that can report the endpoint's
        middle state. Use it when you need to branch on the fleet state, to
        reconcile later with ``event_id``, or to quote ``trace_id`` in a support
        request.

        It sends ``POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate`` to a
        data-plane node, preferring any node that reports itself leader, and
        fails over to other nodes when the chosen node is unreachable or returns
        a non-fleet-aware server error.

        Unlike the thin wrappers below it never turns a non-done outcome into an
        error: ``202 pending`` and ``503 unavailable`` come back as results, and
        so does a 2xx whose body carried no documented state (reported as
        ``state is None``, never guessed as success). It raises only when no
        fleet report could be obtained at all: a transport failure, a
        non-fleet-aware HTTP error (``401``, ``429``, a bare ``500``, ...), or
        every node being dead. A blank ``tenant_id`` raises ``ValueError``.

        ``api_keys`` may be ``None`` or empty for a whole-tenant clear. Check
        :attr:`InvalidateTenantAuthCacheResult.done` — not the HTTP status.
        """
        body = None
        if api_keys:
            body = {"api_keys": list(api_keys)}
        return self._invalidate_with_result(tenant_id, body)

    def invalidate_tenant_auth_cache(self, tenant_id: str) -> None:
        """Invalidate the tenant's complete auth cache.

        Sends ``POST /tenant/{tenant_id}/api/v1/auth/cache/invalidate`` to a
        data-plane node, preferring any node that reports itself leader, and
        fails over to other nodes when the chosen node is unreachable or returns
        a non-fleet-aware server error.

        It is a thin wrapper over :meth:`invalidate_with_result` and returns
        successfully only when the fleet state is ``applied`` or
        ``single_node`` — a 2xx is NOT sufficient. ``202 pending`` raises
        :class:`InvalidatePendingError`, ``503 unavailable`` raises
        :class:`InvalidateUnavailableError`, and a 2xx with no documented state
        raises :class:`InvalidateOutcomeError`; catch that base class to handle
        all three, or call :meth:`invalidate_with_result` to branch on the state.
        """
        self._invalidate_or_raise(tenant_id, None)

    def invalidate_tenant_auth_cache_keys(self, tenant_id: str, api_keys: List[str]) -> None:
        """Invalidate only the supplied api-keys for the tenant.

        When ``api_keys`` is empty this is the same as
        :meth:`invalidate_tenant_auth_cache`. Like it, it succeeds only when the
        fleet state was ``applied`` or ``single_node``.
        """
        if not api_keys:
            self.invalidate_tenant_auth_cache(tenant_id)
            return
        self._invalidate_or_raise(tenant_id, api_keys)

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

    def _query_string(self) -> str:
        """Render the optional ``wait`` / ``timeout_ms`` parameters.

        An unset field is OMITTED rather than sent as its default, so the server
        applies its own configured default (``wait=converged`` and ``timeout_ms``
        = ``HYDRA_TENANT_API_CONVERGE_TIMEOUT_MS``, 2000 ms out of the box) — and
        so a caller can tell "I did not ask" from "I asked for the default". Both
        were validated in the constructor, so no escaping is needed: the values
        are a fixed token and an integer.
        """
        params = {}
        if self._wait_mode is not None:
            params["wait"] = self._wait_mode
        if self._timeout_ms is not None:
            params["timeout_ms"] = str(self._timeout_ms)
        return urllib.parse.urlencode(params)

    def _invalidate_or_raise(
        self, tenant_id: str, api_keys: Optional[List[str]]
    ) -> None:
        """Adapt a structured result to the wrappers' error-only signature.

        Success only for a state that actually means "done", with the two
        distinguishable non-done outcomes surfaced as their own error types, and
        an unrecognised state refused rather than reported as success.
        """
        result = self.invalidate_with_result(tenant_id, api_keys)
        if result.done:
            return
        if result.state == FleetState.PENDING:
            raise InvalidatePendingError(result)
        if result.state == FleetState.UNAVAILABLE:
            raise InvalidateUnavailableError(result)
        # A 2xx whose body did not carry a documented fleet state: we cannot
        # claim the invalidation is complete, so we must not report success.
        raise InvalidateOutcomeError(result)

    def _invalidate_with_result(
        self, tenant_id: str, body: Optional[dict]
    ) -> InvalidateTenantAuthCacheResult:
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
                # Only a transport failure quarantines a node. An HTTP answer
                # (including the 404 a data-plane port gives for the
                # admin-only /healthz/leader route) means "alive, not the
                # leader" and is handled by _probe_leader returning ok=True.
                self._remove_node(node)
                continue
            if node in seen:
                continue
            seen.add(node)
            alive.append(node)
            if leader:
                leaders.append(node)

        # Invalidation is served LOCALLY by every data-plane node (design §6.4
        # decision A-1: deliberately NOT forwarded to the leader's admin API),
        # so leader preference is only a legacy ordering optimization — any
        # alive node can serve it. Leaders are tried first, then the rest.
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
                # A fleet-aware answer — including `503 unavailable` — is the
                # node's verdict about the CLUSTER, and the endpoint is answered
                # locally by whichever node received it, so another node would
                # answer the same question the same way. Return it rather than
                # rotating: rotating here was the defect that made "the cluster
                # was not notified" indistinguishable from "this node is broken".
                #
                # `503 unavailable` is deliberately NOT run through
                # _is_node_failure: the node answered, it is alive, and its own
                # cache was cleared, so it must not be quarantined. The same goes
                # for `202 pending`, which is not an error at the node level at
                # all.
                return self._do_invalidate(node, tenant_id, body)
            except HTTPError as exc:
                if exc.status in (401, 403):
                    # Invalid/forbidden tenant token is a client-side error;
                    # rotating to another node will not fix it.
                    raise
                if self._is_node_failure(exc):
                    self._remove_node(node)
                errors.append(exc)
            except Exception as exc:  # noqa: BLE001 - network/transport errors
                if self._is_node_failure(exc):
                    self._remove_node(node)
                errors.append(exc)

        raise RuntimeError(f"all {len(attempts)} node(s) failed: {errors}")

    def _do_invalidate(
        self, node: str, tenant_id: str, body: Optional[dict]
    ) -> InvalidateTenantAuthCacheResult:
        """Send one invalidation to one node and return its fleet report.

        A result is returned whenever the node produced a fleet report —
        including the ``202 pending`` and ``503 unavailable`` bodies, which
        arrive on a non-2xx status. :class:`HTTPError` is raised for an HTTP
        answer that is NOT fleet-aware (a bare ``500``, ``401``, ``429``, ...):
        those are the answers that still mean "try another node".
        """
        endpoint = (
            node
            + "/tenant/"
            + urllib.parse.quote(tenant_id, safe="")
            + "/api/v1/auth/cache/invalidate"
        )
        query = self._query_string()
        if query:
            endpoint += "?" + query

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
            # Read this body before deciding it is a failure: a `503
            # unavailable` carries a perfectly NORMAL fleet body (with no
            # `error.code`), and reporting it as a bare HTTPError is what lost
            # the "the cluster was not notified" signal. Note the local name:
            # `body` is the request body and must not be shadowed here. A body
            # that cannot be read at all is an empty body — the status and the
            # trace id still decide what happens next.
            try:
                with exc:
                    err_raw = exc.read()
            except Exception:  # noqa: BLE001 - an unreadable body is no body
                err_raw = b""
            trace_id = _header_value(getattr(exc, "headers", None), "X-Hydra-Trace-Id")
            parsed = _parse_invalidate_body(err_raw)
            if parsed is not None:
                return InvalidateTenantAuthCacheResult(
                    http_status=exc.code, trace_id=trace_id, node=node, **parsed
                )
            raise HTTPError(
                "POST",
                endpoint,
                exc.code,
                err_raw.decode("utf-8", "replace"),
                trace_id=trace_id,
            ) from exc

        with resp:
            raw = resp.read()
            status = int(getattr(resp, "status", None) or getattr(resp, "code", 200))
            # The trace id is captured on BOTH paths: §4.2 tells the tenant to
            # quote `X-Hydra-Trace-Id` when reporting a problem, and a problem
            # is exactly what the error paths are.
            trace_id = _header_value(getattr(resp, "headers", None), "X-Hydra-Trace-Id")

        parsed = _parse_invalidate_body(raw)
        if parsed is not None:
            return InvalidateTenantAuthCacheResult(
                http_status=status, trace_id=trace_id, node=node, **parsed
            )
        if not 200 <= status < 300:
            # A non-2xx that is not the documented envelope. (urllib normally
            # raises HTTPError for these; this path exists for a caller-supplied
            # `urlopen` that returns the response instead.)
            raise HTTPError(
                "POST",
                endpoint,
                status,
                raw.decode("utf-8", "replace"),
                trace_id=trace_id,
            )
        # A 2xx we cannot read: do not invent a state. The caller gets a result
        # with `state is None`, which `done` reports as False and the wrappers
        # refuse to report as success.
        return InvalidateTenantAuthCacheResult(
            http_status=status, trace_id=trace_id, node=node
        )

    def _probe_leader(self, node: str):
        """Probe ``/healthz/leader``; return ``(alive, leader)``.

        That route lives on the **admin port**, not the data plane (200 active /
        503 standby / 404 on non-candidate nodes). Only a transport failure
        means the node is unreachable; any HTTP answer — including the 404 a
        data-plane port gives — is "alive, not the leader". Quarantining on a
        non-200 would make the documented data-plane node list empty itself out.
        """
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
        # A request timeout is not a node failure (the node may be healthy but
        # slow); connection refusals and other network errors are.
        if _is_timeout_error(exc):
            return False
        return True
