#!/usr/bin/env python3
"""End-to-end proxy test: starts mock LLM + mock auth + Hydra, registers a
localhost tenant, sends a chat completion through the proxy, verifies the
response came from the mock LLM, then cleans up everything.

Usage:
  python3 integration/e2e_proxy_test.py
  # (assumes hydra is built: cargo build -p hydra-server --features server)
  # or it will `cargo run` for you.

Prerequisites: python3 (stdlib only). The Hydra binary is started via cargo run.
"""
import base64, json, os, signal, subprocess, sys, time, urllib.request, urllib.error

from _usage_env import usage_env  # the ONE owner of "which usage sink a drill's node starts with"

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
# Env-overridable: the defaults collide with a running dev stack (or with another
# check on the same box), and a harness that cannot run next to a stack is a
# harness nobody runs — which is exactly what happened to this file (nothing in CI
# invoked it, and a local run failed on the first bind).
MOCK_LLM_PORT = int(os.environ.get("MOCK_LLM_PORT", "9090"))
MOCK_AUTH_PORT = int(os.environ.get("MOCK_AUTH_PORT", "9091"))
HYDRA_PROXY_PORT = int(os.environ.get("HYDRA_PROXY_PORT", "8080"))
HYDRA_ADMIN_PORT = int(os.environ.get("HYDRA_ADMIN_PORT", "8081"))
# Must be >= 16 chars: main.rs fails closed on a shorter admin token
# (AdminService::MIN_ADMIN_TOKEN_LEN). Was "e2e-test-token" (14) — the server
# refused to start and this harness reported only "did not become healthy".
ADMIN_TOKEN = "e2e-test-token-2026"
CLIENT_KEY = "e2e-client-key"
PROVIDER_KEY = "sk-mock-llm-key"
MODEL = "gpt-4o"
DB_FILE = os.path.join(ROOT, "e2e_test.db")

_procs = []

def _cleanup():
    for p in _procs:
        try:
            p.terminate()
            p.wait(timeout=5)
        except Exception:
            try: p.kill()
            except Exception: pass
    if os.path.exists(DB_FILE):
        os.remove(DB_FILE)

def _master_key():
    """A throwaway 32-byte base64 master key for this run.

    `HYDRA_ENCRYPTION_KEY` is required unconditionally (crypto.rs fails closed
    with `KeyMissing`), and this harness uses a fresh throwaway DB each run, so
    a per-run random key is correct and hermetic (same approach as
    `integration/run.sh`).
    """
    return base64.b64encode(os.urandom(32)).decode()

def _start(name, cmd, env=None, cwd=None):
    p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         env=env, cwd=cwd, text=True)
    _procs.append(p)
    print(f"[e2e] started {name} (pid={p.pid})")
    return p

def _wait_ok(url, token=None, timeout=30):
    hdrs = {}
    if token:
        hdrs["Authorization"] = f"Bearer {token}"
    for _ in range(timeout * 10):
        try:
            req = urllib.request.Request(url, headers=hdrs)
            with urllib.request.urlopen(req, timeout=2) as r:
                if r.status == 200:
                    return True
        except Exception:
            pass
        time.sleep(0.1)
    return False

def _admin(method, path, body=None):
    url = f"http://localhost:{HYDRA_ADMIN_PORT}/api/v1{path}"
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers={
        "Authorization": f"Bearer {ADMIN_TOKEN}",
        "Content-Type": "application/json",
    })
    try:
        with urllib.request.urlopen(req, timeout=5) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()[:200]

def main():
    signal.signal(signal.SIGINT, lambda *_: (_cleanup(), sys.exit(130)))
    signal.signal(signal.SIGTERM, lambda *_: (_cleanup(), sys.exit(143)))

    # Fail fast on a configuration error the server would otherwise report as a
    # boot failure 40 seconds later.
    if len(ADMIN_TOKEN) < 16:
        print(f"[e2e] FAIL: ADMIN_TOKEN is {len(ADMIN_TOKEN)} chars; the server requires >= 16")
        sys.exit(1)

    try:
        # ── 1. start mocks ──
        _start("mock-llm", [sys.executable, os.path.join(ROOT, "integration", "mock_llm.py")])
        _start("mock-auth", [sys.executable, os.path.join(ROOT, "integration", "mock_auth.py")])
        time.sleep(0.5)

        # ── 2. start Hydra ──
        # The admin token must be >= 16 chars and the master key is REQUIRED
        # (unconditional fail-closed, crypto.rs `KeyMissing` -> main.rs). The
        # old token here was 14 chars and the key was not set at all, so this
        # harness could never boot the server it was meant to exercise.
        #
        # The sink must be named EXPLICITLY since ADR-0002 D-1 (no default) — and it comes from
        # the shared `_usage_env` owner rather than a literal here, so this drill cannot drift
        # from the other 39. It is a proxy-path suite (mock auth + mock LLM + failover), so
        # `none` is the honest choice: nothing here reads a metering row.
        env = {**os.environ, **usage_env(),
               "HYDRA_ADMIN_TOKEN": ADMIN_TOKEN,
               "HYDRA_ENCRYPTION_KEY": _master_key(),
               "HYDRA_DB_URL": f"sqlite:{DB_FILE}?mode=rwc",
               "HYDRA_LISTEN": f"0.0.0.0:{HYDRA_PROXY_PORT}",
               "HYDRA_ADMIN_ADDR": f"0.0.0.0:{HYDRA_ADMIN_PORT}",
               "RUST_LOG": "info"}
        _start("hydra",
               ["cargo", "run", "-p", "hydra-server", "--features", "server", "--"],
               env=env, cwd=ROOT)

        # ── 3. wait for health ──
        print("[e2e] waiting for Hydra health...", flush=True)
        if not _wait_ok(f"http://localhost:{HYDRA_ADMIN_PORT}/api/v1/health",
                        token=ADMIN_TOKEN, timeout=40):
            # dump hydra output for debugging
            for p in _procs:
                if p.poll() is not None:
                    out = p.stdout.read() if p.stdout else ""
                    print(f"[e2e] process exited: {out[:500]}")
            print("[e2e] FAIL: Hydra did not become healthy")
            _cleanup(); sys.exit(1)
        print("[e2e] Hydra healthy ✓", flush=True)

        # ── 4. register config ──
        steps = [
            ("provider", "/providers",
             {"id": "mock", "key": "mock", "name": "Mock LLM",
              "endpoint": f"http://localhost:{MOCK_LLM_PORT}", "weight": 1,
              "created_at": "", "updated_at": ""}),
            ("model", "/provider-models",
             {"id": "mm", "key": MODEL, "name": "GPT-4o Mock",
              "provider_id": "mock", "status": 1,
              "created_at": "", "updated_at": ""}),
            ("key", "/provider-keys",
             {"id": "mk", "provider_id": "mock", "api_key": PROVIDER_KEY,
              "created_at": ""}),
            ("tenant", "/tenants",
             {"id": "local", "name": "Local", "domain": "localhost",
              "auth_url": f"http://localhost:{MOCK_AUTH_PORT}", "enabled": True,
              "cert_key": None, "cert_file": None,
              "created_at": "", "updated_at": ""}),
            ("tenant-provider", "/tenant-providers",
             {"id": "tp", "tenant_id": "local", "provider_id": "mock",
              "created_at": "", "updated_at": ""}),
            ("tenant-model", "/tenant-models",
             {"id": "tm", "tenant_id": "local", "model_key": MODEL,
              "created_at": "", "updated_at": ""}),
        ]
        for label, path, body in steps:
            s, j = _admin("POST", path, body)
            assert s in (200, 201), f"[e2e] FAIL: register {label} -> {s}: {j}"
        print("[e2e] config registered (provider + model + key + tenant + associations) ✓", flush=True)

        # ── 5. send chat completion through the proxy ──
        chat_body = json.dumps({
            "model": MODEL,
            "messages": [{"role": "user", "content": "Hello, mock LLM!"}],
        }).encode()
        req = urllib.request.Request(
            f"http://localhost:{HYDRA_PROXY_PORT}/v1/chat/completions",
            data=chat_body, method="POST",
            headers={
                "Authorization": f"Bearer {CLIENT_KEY}",
                "Content-Type": "application/json",
                "Host": "localhost",
            })
        try:
            with urllib.request.urlopen(req, timeout=15) as r:
                resp = json.loads(r.read())
                content = (resp.get("choices", [{}])[0]
                           .get("message", {}).get("content", ""))
                usage = resp.get("usage", {})
                if "Hello from mock LLM" in content:
                    print(f"[e2e] SUCCESS ✓ proxy returned mock LLM response:")
                    print(f'       content = "{content}"')
                    print(f'       usage   = {usage}')
                else:
                    print(f"[e2e] FAIL: unexpected response: {json.dumps(resp)[:300]}")
                    _cleanup(); sys.exit(1)
        except Exception as e:
            print(f"[e2e] FAIL: proxy request error: {e}")
            _cleanup(); sys.exit(1)

        # ── 6. cleanup ──
        _cleanup()
        print("[e2e] all stopped. PASSED ✓")

    except Exception as e:
        print(f"[e2e] ERROR: {e}")
        _cleanup()
        sys.exit(1)


if __name__ == "__main__":
    main()
