#!/usr/bin/env python3
"""Initialize Hydra config from a secure JSON file.

Reads secure/config.json (gitignored) for real provider api-keys + endpoints,
then registers providers, models, keys, tenant, and associations via the admin
REST API. Run AFTER `docker-compose up -d` (Hydra must be healthy on :8081).

Usage:
  python3 environment/init.py                    # reads secure/config.json
  python3 environment/init.py /path/to/config.json  # custom path
"""
import json, os, sys, time, urllib.request, urllib.error

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DEFAULT_CONFIG = os.path.join(ROOT, "secure", "config.json")

def req(method, url, token, body=None):
    data = json.dumps(body).encode() if body is not None else None
    r = urllib.request.Request(url, data=data, method=method, headers={
        "Authorization": f"Bearer {token}",
        "Content-Type": "application/json",
    })
    try:
        with urllib.request.urlopen(r, timeout=5) as resp:
            raw = resp.read()
            return resp.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:200]
    except Exception as e:
        return 0, str(e)[:200]

def wait_health(base, token, timeout=30):
    for _ in range(timeout * 10):
        s, _ = req("GET", f"{base}/health", token)
        if s == 200:
            return True
        time.sleep(0.2)
    return False

def main():
    cfg_path = sys.argv[1] if len(sys.argv) > 1 else DEFAULT_CONFIG
    if not os.path.exists(cfg_path):
        print(f"ERROR: config not found at {cfg_path}")
        print("Copy the template:  cp environment/config.example.json secure/config.json")
        print("Then edit it with your real api-keys.")
        sys.exit(1)

    with open(cfg_path) as f:
        cfg = json.load(f)

    base = cfg.get("admin_url", "http://localhost:8081") + "/api/v1"
    # No default: a guessable admin credential is a full gateway takeover
    # (tenant/provider CRUD + upstream api-key writes). Take the config value or
    # the environment, and enforce the server's own minimum length.
    token = (cfg.get("admin_token") or os.environ.get("HYDRA_ADMIN_TOKEN") or "").strip()
    if len(token) < 16:
        print(
            "[init] FAIL: no usable admin token.\n"
            '       Set "admin_token" in secure/config.json (or export HYDRA_ADMIN_TOKEN)\n'
            "       to the same value the server was started with; minimum 16 chars.\n"
            "       Generate one with:  openssl rand -hex 32"
        )
        sys.exit(1)

    print(f"[init] waiting for Hydra at {base.rsplit('/api/v1',1)[0]} ...")
    if not wait_health(base, token):
        print("[init] FAIL: Hydra not healthy. Is docker-compose up?")
        sys.exit(1)
    print("[init] Hydra healthy ✓")

    now = {"created_at": "", "updated_at": ""}
    ok = 0; fail = 0

    def post(label, path, body):
        nonlocal ok, fail
        s, j = req("POST", f"{base}{path}", token, {**body, **now})
        if s in (200, 201):
            ok += 1
            print(f"  ✓ {label}")
        else:
            # maybe already exists (reload after prior run)
            fail += 1
            print(f"  ⚠ {label} -> {s} {j[:80] if isinstance(j,str) else ''}")

    # ── providers + models + keys ──
    all_model_keys = []
    for p in cfg.get("providers", []):
        post(f"provider {p['id']}", "/providers", {
            "id": p["id"], "key": p["key"], "name": p["name"],
            "endpoint": p["endpoint"], "weight": p.get("weight", 1),
        })
        for model_key in p.get("models", []):
            post(f"model {model_key} @ {p['id']}", "/provider-models", {
                "id": f"{p['id']}-{model_key}", "key": model_key,
                "name": model_key, "provider_id": p["id"], "status": 1,
            })
            all_model_keys.append(model_key)
        for i, api_key in enumerate(p.get("api_keys", [])):
            post(f"key-{i} @ {p['id']}", "/provider-keys", {
                "id": f"{p['id']}-key{i}", "provider_id": p["id"], "api_key": api_key,
            })

    # ── tenant ──
    t = cfg.get("tenant", {})
    post(f"tenant {t.get('id','default')}", "/tenants", {
        "id": t.get("id", "default"), "name": t.get("name", "Default"),
        "domain": t.get("domain", "localhost"),
        "auth_url": t.get("auth_url", "http://mock-tenant:9091"),
        "enabled": True, "cert_key": None, "cert_file": None,
    })
    # associate tenant with all providers
    for p in cfg.get("providers", []):
        post(f"tenant-provider {t.get('id','default')}↔{p['id']}", "/tenant-providers", {
            "id": f"tp-{p['id']}", "tenant_id": t.get("id", "default"),
            "provider_id": p["id"],
        })
    # associate tenant with all models
    for mk in all_model_keys:
        post(f"tenant-model {t.get('id','default')}↔{mk}", "/tenant-models", {
            "id": f"tm-{mk}", "tenant_id": t.get("id", "default"),
            "model_key": mk,
        })

    print(f"\n[init] done: {ok} ok, {fail} warnings (duplicates are fine).")
    print("[init] test: curl http://localhost:8080/v1/chat/completions \\")
    print("           -H 'Authorization: Bearer <your-client-key>' \\")
    print("           -H 'Content-Type: application/json' \\")
    print(f"           -d '{{\"model\":\"{all_model_keys[0] if all_model_keys else 'gpt-4o'}\",\"messages\":[{{\"role\":\"user\",\"content\":\"hi\"}}]}}'")

if __name__ == "__main__":
    main()
