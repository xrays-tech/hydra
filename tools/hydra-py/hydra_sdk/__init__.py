"""Hydra tenant self-service auth-cache invalidation SDK."""

from .client import HTTPError, HydraClient

Client = HydraClient

__all__ = ["HTTPError", "HydraClient", "Client"]
