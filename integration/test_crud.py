#!/usr/bin/env python3
"""Integration tests for the Hydra admin REST API — mirrors the Rust suite at
``crates/hydra-server/tests/admin_api.rs`` (833 lines), expressed in Python with
**stdlib only** (``urllib.request``/``json``/``sys``/``os``) so it runs anywhere
python3 is available, no pip install.

It exercises the **full CRUD lifecycle for all 7 config entities** plus the
edge cases and non-CRUD endpoints covered by the Rust suite:

  CRUD (POST → GET-list → GET-item → PUT → GET-confirm → DELETE → GET-404):
    providers, provider-models, provider-keys, tenants, tenant-providers,
    tenant-models, limit-roles
  (tenant-providers & tenant-models are association tables with no PUT update
   endpoint — mirrors admin_api.rs / handlers.rs — so their PUT step is skipped.)

  Edge cases:
    - provider-key masking (list masks → first4…last4; ?reveal=1 → plaintext)
    - tenant auth_url mandatory (POST without → 400)
    - UNIQUE conflict → 409 (provider key, provider-model (key,provider_id),
      tenant domain, tenant-provider (tenant_id,provider_id),
      tenant-model (tenant_id,model_key))
    - FK / CHECK violation → 400 (provider-model provider_id=ghost,
      provider-model status=7, limit-role window='z')
    - 401 without token / with wrong token
    - unknown path → 404 (error.code == "not_found")

  Non-CRUD:
    - GET  /api/v1/health      → 200 {status:ok, db:ok}
    - POST /api/v1/reload      → 200 {status:reloaded}
    - DELETE /api/v1/auth/cache → 200 {invalidated:<int>}
    - GET  /api/v1/breaker     → 200 {dead:[…]}
    - GET  /api/v1/stats/usage → 200 {totals, by_tenant, by_provider} (usage stats)

Configuration:
    HYDRA_BASE_URL   (default http://localhost:8081)  — admin listener origin
    HYDRA_ADMIN_TOKEN(default hydra-it-token)         — bearer token

Exit code: 0 if every assertion passed, 1 otherwise. Prints a per-assertion
PASS/FAIL line and a final ``=== N/N passed ===`` summary.
"""

import json
import os
import sys
import urllib.error
import urllib.request
from typing import Any, Optional

# ---------------------------------------------------------------------------
# Configuration
# ---------------------------------------------------------------------------

BASE = os.environ.get("HYDRA_BASE_URL", "http://localhost:8081").rstrip("/")
TOKEN = os.environ.get("HYDRA_ADMIN_TOKEN", "hydra-it-token")
TIMEOUT = 10  # seconds per HTTP request

# Assertion counters.
_passed = 0
_failed = 0


# ---------------------------------------------------------------------------
# Assertion primitive
# ---------------------------------------------------------------------------

def check(label: str, condition: bool, detail: str = "") -> None:
    """Record one PASS/FAIL line and increment the shared counters."""
    global _passed, _failed
    if condition:
        _passed += 1
        print(f"  PASS  {label}")
    else:
        _failed += 1
        msg = f"  FAIL  {label}"
        if detail:
            msg += f"  --  {detail}"
        print(msg)


# ---------------------------------------------------------------------------
# HTTP helper (stdlib urllib)
# ---------------------------------------------------------------------------

def req(method: str, path: str, body: Any = None, expect: Optional[int] = None,
        token: Optional[str] = TOKEN):
    """Perform one admin API request.

    Returns ``(status, parsed)``:
      - ``status``: the HTTP status code (int), or ``None`` on a transport-level
        failure (connection refused / timeout).
      - ``parsed``: the JSON-decoded body (``dict``/``list``) when the response
        is JSON; the raw response ``str`` for non-JSON bodies; ``None`` when
        there is no body or transport failed.

    Parameters
    ----------
    method : str                  HTTP verb (GET/POST/PUT/DELETE).
    path : str                    Absolute path beginning with ``/api/v1``.
    body : dict | None            JSON body (serialized automatically).
    expect : int | None           If set, the status is asserted == expect via
                                   ``check`` (records a PASS/FAIL line).
    token : str | None            Bearer token; ``None`` ⇒ omit the header.
    """
    url = BASE + path
    headers = {"Accept": "application/json"}
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    data = None
    if body is not None:
        data = json.dumps(body).encode("utf-8")
        headers["Content-Type"] = "application/json"

    request = urllib.request.Request(url, data=data, method=method, headers=headers)

    try:
        with urllib.request.urlopen(request, timeout=TIMEOUT) as resp:
            status = resp.getcode()
            raw = resp.read()
    except urllib.error.HTTPError as e:
        # Non-2xx: read the error body so callers can inspect error.code etc.
        status = e.code
        raw = e.read()
    except (urllib.error.URLError, OSError) as e:
        # Transport-level failure (server down, refused, timeout).
        if expect is not None:
            check(f"{method} {path} -> {expect}", False, f"transport error: {e}")
        return None, None

    text = raw.decode("utf-8", errors="replace")
    parsed: Any = None
    if text:
        try:
            parsed = json.loads(text)
        except ValueError:
            parsed = text  # non-JSON body (e.g. empty 204, plain text)

    if expect is not None:
        check(
            f"{method} {path} -> HTTP {expect}",
            status == expect,
            f"got {status}: {text[:200]}",
        )
    return status, parsed


# ---------------------------------------------------------------------------
# Small helpers (typed; isinstance guards keep .get() safe)
# ---------------------------------------------------------------------------

def as_dict(j: Any) -> Optional[dict]:
    """Return ``j`` if it is a dict, else None."""
    return j if isinstance(j, dict) else None


def as_list(j: Any) -> Optional[list]:
    """Return ``j`` if it is a list, else None."""
    return j if isinstance(j, list) else None


def assert_in(label: str, collection: list, item: Any, detail: str = "") -> None:
    """Assert ``item`` appears in ``collection`` (a list)."""
    check(label, item in collection, detail)


def assert_eq(label: str, actual: Any, expected: Any) -> None:
    """Assert ``actual == expected`` with a readable diff on failure."""
    check(label, actual == expected, f"expected {expected!r}, got {actual!r}")


def list_ids(j: Any) -> list:
    """Extract the ``id`` field from each row of a list response."""
    rows = as_list(j) or []
    return [r.get("id") for r in rows if isinstance(r, dict)]


# ---------------------------------------------------------------------------
# CRUD lifecycle for each entity
# ---------------------------------------------------------------------------
# FK ordering is respected by (re)creating parent rows at the start of each
# child test. Each test cleans up after itself (DELETE at the end).

def test_providers() -> None:
    print("\n[providers] full CRUD + UNIQUE conflict")
    body = {
        "id": "it-prov", "key": "openai-it", "name": "IT Provider",
        "endpoint": "https://api.openai.com", "weight": 1,
        "created_at": "", "updated_at": "",
    }
    # POST -> 201
    s, j = req("POST", "/api/v1/providers", body, expect=201)
    d = as_dict(j)
    if d:
        assert_eq("  providers POST returns id", d.get("id"), "it-prov")
        check("  providers POST fills created_at", bool(d.get("created_at")))
    # GET list contains it
    s, j = req("GET", "/api/v1/providers", expect=200)
    assert_in("  providers GET list contains it-prov", list_ids(j), "it-prov")
    # GET item
    s, j = req("GET", "/api/v1/providers/it-prov", expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  providers GET item weight", d.get("weight"), 1)
    # PUT update (weight 9, renamed)
    upd = dict(body, name="IT Provider Renamed", weight=9)
    s, j = req("PUT", "/api/v1/providers/it-prov", upd, expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  providers PUT weight", d.get("weight"), 9)
        assert_eq("  providers PUT name", d.get("name"), "IT Provider Renamed")
    # GET confirms update
    s, j = req("GET", "/api/v1/providers/it-prov", expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  providers GET-after-PUT weight", d.get("weight"), 9)
    # UNIQUE conflict on same key (POST again with same body)
    req("POST", "/api/v1/providers", body, expect=409)
    # DELETE -> 204
    req("DELETE", "/api/v1/providers/it-prov", expect=204)
    # GET -> 404
    req("GET", "/api/v1/providers/it-prov", expect=404)


def test_provider_models(parent_provider_body: dict) -> None:
    print("\n[provider-models] full CRUD + FK/CHECK/UNIQUE violations")
    # Ensure parent provider exists.
    req("POST", "/api/v1/providers", parent_provider_body, expect=201)

    # FK violation: model -> non-existent provider -> 400
    bad_fk = {"id": "m-ghost", "key": "gk", "name": "g",
              "provider_id": "ghost-no-such", "status": 1}
    s, j = req("POST", "/api/v1/provider-models", bad_fk, expect=400)
    d = as_dict(j)
    if d:
        err = d.get("error") or {}
        assert_eq("  provider-models FK error code", err.get("code"),
                  "foreign_key_violation")

    # CHECK violation: status=7 (only 1/0/-1 allowed)
    bad_status = {"id": "m-bad", "key": "gk2", "name": "g",
                  "provider_id": parent_provider_body["id"], "status": 7}
    req("POST", "/api/v1/provider-models", bad_status, expect=400)

    # Valid create
    body = {"id": "it-pm", "key": "gpt-4-it", "name": "GPT-4 IT",
            "provider_id": parent_provider_body["id"], "status": 1}
    req("POST", "/api/v1/provider-models", body, expect=201)
    # GET list
    s, j = req("GET", "/api/v1/provider-models", expect=200)
    assert_in("  provider-models GET list contains it-pm", list_ids(j), "it-pm")
    # GET item
    s, j = req("GET", "/api/v1/provider-models/it-pm", expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  provider-models GET item key", d.get("key"), "gpt-4-it")
    # PUT update (status 0, renamed)
    upd = {"id": "it-pm", "key": "gpt-4-it", "name": "GPT-4 IT Offline",
           "provider_id": parent_provider_body["id"], "status": 0}
    s, j = req("PUT", "/api/v1/provider-models/it-pm", upd, expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  provider-models PUT status", d.get("status"), 0)
        assert_eq("  provider-models PUT name", d.get("name"), "GPT-4 IT Offline")
    # UNIQUE conflict: same (key, provider_id)
    req("POST", "/api/v1/provider-models", body, expect=409)
    # DELETE -> 204, then 404
    req("DELETE", "/api/v1/provider-models/it-pm", expect=204)
    req("GET", "/api/v1/provider-models/it-pm", expect=404)


def test_provider_keys(parent_provider_body: dict) -> None:
    print("\n[provider-keys] CRUD + masking (P1-5: ALWAYS masked, ?reveal=1 no-op)")
    req("POST", "/api/v1/providers", parent_provider_body, expect=201)

    plaintext = "sk-it-supersecret-12345"
    body = {"id": "it-pk", "provider_id": parent_provider_body["id"],
            "api_key": plaintext, "created_at": ""}
    # POST returns masked by default
    s, j = req("POST", "/api/v1/provider-keys", body, expect=201)
    d = as_dict(j)
    if d:
        masked = d.get("api_key")
        check("  provider-keys POST masks api_key (≠ plaintext)",
              masked != plaintext, f"got {masked!r}")
        check("  provider-keys POST mask uses stars (first10…last4)",
              isinstance(masked, str) and "*" in masked, f"got {masked!r}")
    # GET list masked
    s, j = req("GET", "/api/v1/provider-keys", expect=200)
    rows = as_list(j) or []
    if rows and isinstance(rows[0], dict):
        k = rows[0].get("api_key")
        check("  provider-keys list masks api_key (≠ plaintext)",
              k != plaintext, f"got {k!r}")
    # ?reveal=1 is a NO-OP since P1-5 (admin API never returns plaintext)
    s, j = req("GET", "/api/v1/provider-keys?reveal=1", expect=200)
    rows = as_list(j) or []
    if rows and isinstance(rows[0], dict):
        check("  provider-keys ?reveal=1 stays masked (P1-5 no-op)",
              rows[0].get("api_key") != plaintext, f"got {rows[0].get('api_key')!r}")
    # GET item (masked)
    s, j = req("GET", "/api/v1/provider-keys/it-pk", expect=200)
    d = as_dict(j)
    if d:
        check("  provider-keys GET item masked (≠ plaintext)",
              d.get("api_key") != plaintext)
    # PUT upsert (new api_key)
    upd = {"id": "it-pk", "provider_id": parent_provider_body["id"],
           "api_key": "sk-it-rotated-987654321", "created_at": ""}
    s, j = req("PUT", "/api/v1/provider-keys/it-pk", upd, expect=200)
    d = as_dict(j)
    if d:
        check("  provider-keys PUT masks rotated key",
              d.get("api_key") != "sk-it-rotated-987654321")
    # DELETE -> 204, then 404
    req("DELETE", "/api/v1/provider-keys/it-pk", expect=204)
    req("GET", "/api/v1/provider-keys/it-pk", expect=404)


def test_tenants() -> None:
    print("\n[tenants] CRUD + auth_url mandatory + domain UNIQUE")
    # auth_url empty -> 400 (missing_required_field)
    bad = {"id": "it-tenant", "name": "T", "domain": "it.example.com",
           "auth_url": "", "cert_key": None, "cert_file": None,
           "enabled": True, "created_at": "", "updated_at": ""}
    s, j = req("POST", "/api/v1/tenants", bad, expect=400)
    d = as_dict(j)
    if d:
        err = d.get("error") or {}
        assert_eq("  tenants empty auth_url error code",
                  err.get("code"), "missing_required_field")

    # Valid
    body = {"id": "it-tenant", "name": "IT Tenant", "domain": "it.example.com",
            "auth_url": "https://auth.it.example.com/v",
            "cert_key": None, "cert_file": None, "enabled": True,
            "created_at": "", "updated_at": ""}
    req("POST", "/api/v1/tenants", body, expect=201)
    # GET list
    s, j = req("GET", "/api/v1/tenants", expect=200)
    assert_in("  tenants GET list contains it-tenant", list_ids(j), "it-tenant")
    # GET item
    s, j = req("GET", "/api/v1/tenants/it-tenant", expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  tenants GET item domain", d.get("domain"), "it.example.com")
    # PUT update (auth_url v2, disabled)
    upd = dict(body, name="IT Tenant v2",
               auth_url="https://auth.it.example.com/v2", enabled=False)
    s, j = req("PUT", "/api/v1/tenants/it-tenant", upd, expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  tenants PUT auth_url", d.get("auth_url"),
                  "https://auth.it.example.com/v2")
    # domain UNIQUE conflict
    req("POST", "/api/v1/tenants", body, expect=409)
    # DELETE -> 204, then 404
    req("DELETE", "/api/v1/tenants/it-tenant", expect=204)
    req("GET", "/api/v1/tenants/it-tenant", expect=404)


def test_tenant_providers(provider_body: dict, tenant_body: dict) -> None:
    """Association table: POST/GET/DELETE only (no PUT — handlers route to 405)."""
    print("\n[tenant-providers] POST/GET/DELETE + UNIQUE conflict")
    # Section-unique parents (id+domain) so we never collide with other sections.
    prov = {**provider_body, "id": "it-tp-prov"}
    ten = {**tenant_body, "id": "it-tp-tenant", "domain": "it-tp.example.com"}
    req("POST", "/api/v1/providers", prov, expect=201)
    req("POST", "/api/v1/tenants", ten, expect=201)

    body = {"id": "it-tp", "tenant_id": ten["id"],
            "provider_id": prov["id"]}
    req("POST", "/api/v1/tenant-providers", body, expect=201)
    s, j = req("GET", "/api/v1/tenant-providers", expect=200)
    assert_in("  tenant-providers GET list contains it-tp", list_ids(j), "it-tp")
    s, j = req("GET", "/api/v1/tenant-providers/it-tp", expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  tenant-providers GET item provider_id",
                  d.get("provider_id"), prov["id"])
    # UNIQUE(tenant_id, provider_id) conflict
    req("POST", "/api/v1/tenant-providers", body, expect=409)
    # DELETE -> 204, then 404
    req("DELETE", "/api/v1/tenant-providers/it-tp", expect=204)
    req("GET", "/api/v1/tenant-providers/it-tp", expect=404)
    # cleanup section parents
    req("DELETE", "/api/v1/tenants/it-tp-tenant", expect=204)
    req("DELETE", "/api/v1/providers/it-tp-prov", expect=204)


def test_tenant_models(tenant_body: dict, model_key: str) -> None:
    """Association table: POST/GET/DELETE only (no PUT)."""
    print("\n[tenant-models] POST/GET/DELETE + UNIQUE conflict")
    # Section-unique tenant (id+domain) to avoid colliding with other sections.
    ten = {**tenant_body, "id": "it-tm-tenant", "domain": "it-tm.example.com"}
    req("POST", "/api/v1/tenants", ten, expect=201)
    body = {"id": "it-tm", "tenant_id": ten["id"], "model_key": model_key}
    req("POST", "/api/v1/tenant-models", body, expect=201)
    s, j = req("GET", "/api/v1/tenant-models", expect=200)
    assert_in("  tenant-models GET list contains it-tm", list_ids(j), "it-tm")
    s, j = req("GET", "/api/v1/tenant-models/it-tm", expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  tenant-models GET item model_key", d.get("model_key"), model_key)
    # UNIQUE(tenant_id, model_key) conflict
    req("POST", "/api/v1/tenant-models", body, expect=409)
    req("DELETE", "/api/v1/tenant-models/it-tm", expect=204)
    req("GET", "/api/v1/tenant-models/it-tm", expect=404)
    # cleanup section parent
    req("DELETE", "/api/v1/tenants/it-tm-tenant", expect=204)


def test_limit_roles() -> None:
    print("\n[limit-roles] CRUD + window CHECK violation")
    # Invalid window -> CHECK violation -> 400
    bad = {"id": "it-lr", "name": "r", "matching_key": None, "matching_model": None,
           "matching_tenant": "it-tenant", "matching_provider": None,
           "limit_count": 100, "limit_token": None, "window": "z",
           "enabled": True, "created_at": ""}
    req("POST", "/api/v1/limit-roles", bad, expect=400)

    # Valid
    body = {"id": "it-lr", "name": "IT Role", "matching_key": None,
            "matching_model": None, "matching_tenant": "it-tenant",
            "matching_provider": None, "limit_count": 100, "limit_token": None,
            "window": "m", "enabled": True, "created_at": ""}
    req("POST", "/api/v1/limit-roles", body, expect=201)
    s, j = req("GET", "/api/v1/limit-roles", expect=200)
    assert_in("  limit-roles GET list contains it-lr", list_ids(j), "it-lr")
    s, j = req("GET", "/api/v1/limit-roles/it-lr", expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  limit-roles GET item window", d.get("window"), "m")
    # PUT update (limit_count 50, window h)
    upd = dict(body, name="IT Role v2", limit_count=50, window="h")
    s, j = req("PUT", "/api/v1/limit-roles/it-lr", upd, expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  limit-roles PUT limit_count", d.get("limit_count"), 50)
        assert_eq("  limit-roles PUT window", d.get("window"), "h")
    req("DELETE", "/api/v1/limit-roles/it-lr", expect=204)
    req("GET", "/api/v1/limit-roles/it-lr", expect=404)


# ---------------------------------------------------------------------------
# Auth gate + 404
# ---------------------------------------------------------------------------

def test_auth_and_routing() -> None:
    print("\n[auth] 401 without/wrong token; 404 unknown path")
    # No token -> 401
    req("GET", "/api/v1/health", token=None, expect=401)
    # Wrong token -> 401
    req("GET", "/api/v1/health", token="definitely-wrong", expect=401)
    # Correct token -> 200 (sanity)
    req("GET", "/api/v1/health", expect=200)
    # Unknown path -> 404 with error.code == not_found
    s, j = req("GET", "/api/v1/nope", expect=404)
    d = as_dict(j)
    if d:
        err = d.get("error") or {}
        assert_eq("  unknown path error.code == not_found",
                  err.get("code"), "not_found")
        check("  unknown path returns a trace_id", bool(err.get("trace_id")))


# ---------------------------------------------------------------------------
# Non-CRUD endpoints
# ---------------------------------------------------------------------------

def test_health_reload_authcache_breaker() -> None:
    print("\n[health/reload/auth-cache/breaker] non-CRUD endpoints")

    # health
    s, j = req("GET", "/api/v1/health", expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  health status", d.get("status"), "ok")
        assert_eq("  health db", d.get("db"), "ok")
        check("  health has breaker_dead", "breaker_dead" in d)

    # reload -> 200, status reloaded, snapshot counts present
    s, j = req("POST", "/api/v1/reload", body={}, expect=200)
    d = as_dict(j)
    if d:
        assert_eq("  reload status", d.get("status"), "reloaded")
        check("  reload returns providers count", "providers" in d)

    # auth cache invalidation (unknown key -> invalidated 0, no error)
    s, j = req("DELETE", "/api/v1/auth/cache",
               body={"tenant_id": "it-tenant", "api_keys": ["sk-nonexistent-aaa"]},
               expect=200)
    d = as_dict(j)
    if d:
        check("  auth/cache invalidated is int",
              isinstance(d.get("invalidated"), int))
        check("  auth/cache returns tenant_id", "tenant_id" in d)
    # Whole-tenant invalidation (no keys)
    s, j = req("DELETE", "/api/v1/auth/cache",
               body={"tenant_id": "it-tenant"}, expect=200)
    d = as_dict(j)
    if d:
        check("  auth/cache tenant-only invalidated is int",
              isinstance(d.get("invalidated"), int))

    # breaker inspect -> 200, dead is a list
    s, j = req("GET", "/api/v1/breaker", expect=200)
    d = as_dict(j)
    if d:
        check("  breaker returns dead array", isinstance(d.get("dead"), list))

    # usage stats (Admin UI Stats page) -> 200, well-formed aggregate
    s, j = req("GET", "/api/v1/stats/usage", expect=200)
    d = as_dict(j)
    if d:
        check("  stats/usage has totals", isinstance(d.get("totals"), dict))
        check("  stats/usage totals.requests is int",
              isinstance((d.get("totals") or {}).get("requests"), int))
        check("  stats/usage totals.tokens is int",
              isinstance((d.get("totals") or {}).get("tokens"), int))
        check("  stats/usage by_tenant is list", isinstance(d.get("by_tenant"), list))
        check("  stats/usage by_provider is list", isinstance(d.get("by_provider"), list))
        check("  stats/usage generated_at present", bool(d.get("generated_at")))


# ---------------------------------------------------------------------------
# Shared body templates (FK-ordered creation)
# ---------------------------------------------------------------------------

_IT_N = [0]


def _uid() -> int:
    # Monotonic unique id so each provider_body()/tenant_body() call yields a
    # distinct id+key/domain — the app enforces UNIQUE(provider.key)/UNIQUE(tenant.domain),
    # and multiple test sections each create their own parent rows.
    _IT_N[0] += 1
    return _IT_N[0]


def provider_body() -> dict:
    n = _uid()
    return {
        "id": f"it-prov-{n}", "key": f"openai-it-{n}", "name": "IT Provider",
        "endpoint": "https://api.openai.com", "weight": 1,
        "created_at": "", "updated_at": "",
    }


def tenant_body() -> dict:
    n = _uid()
    return {
        "id": f"it-tenant-{n}", "name": "IT Tenant", "domain": f"it-{n}.example.com",
        "auth_url": "https://auth.it.example.com/v",
        "cert_key": None, "cert_file": None, "enabled": True,
        "created_at": "", "updated_at": "",
    }


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> None:
    print("=== Hydra admin API integration tests ===")
    print(f"    base:  {BASE}")
    print(f"    token: {'<set>' if TOKEN else '<unset>'}")
    print()

    # Order matters: parents before children. Each test cleans up after itself
    # (DELETE at the end), but to keep UNIQUE constraints happy across tests we
    # (re)create the parent rows at the start of each child test.

    test_auth_and_routing()
    test_providers()
    test_provider_models(parent_provider_body=provider_body())
    test_provider_keys(parent_provider_body=provider_body())
    test_tenants()
    test_tenant_providers(provider_body=provider_body(), tenant_body=tenant_body())
    test_tenant_models(tenant_body=tenant_body(), model_key="gpt-4-it")
    test_limit_roles()
    test_health_reload_authcache_breaker()

    print()
    total = _passed + _failed
    print(f"=== {_passed}/{total} passed ===")
    if _failed:
        print(f"!!! {_failed} assertion(s) FAILED")
        sys.exit(1)
    sys.exit(0)


if __name__ == "__main__":
    main()
