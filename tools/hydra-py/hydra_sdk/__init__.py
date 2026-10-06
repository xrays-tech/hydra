"""Hydra tenant self-service auth-cache invalidation SDK.

The entry point is :class:`HydraClient`. A 2xx is not the same as "done": branch
on :class:`FleetState` (or
:attr:`InvalidateTenantAuthCacheResult.done`) rather than on the HTTP status.
"""

from .client import (
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

Client = HydraClient

__all__ = [
    "Client",
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
