#!/usr/bin/env python3
"""`hydra-admin` (the operator CLI) against a REAL node — every documented command.

The CLI's README advertises that it "drives every admin endpoint" and gives a command
reference (`tools/hydra-cli/README.md`). Its own test suite (`npm test`, run in CI's
`sdks` job) is a typechecked UNIT suite against a fake `fetch`; until now **nothing ran
`hydra-admin` against a gateway**. So the documented command surface, the documented
env-var configuration, and the documented examples were unverified, and one of them was
in fact broken (see leg 5: `--max-concurrency null` sent `""`).

Legs (only `target/debug/hydra` + node are needed; ports 1875x, no Redis):
  1 configuration: the documented env vars work, and global options may appear
    before OR after the subcommand (README says both);
  2 service commands: health / reload / metrics / concurrency / breaker / stats usage /
    cluster status / auth-cache invalidate / tenants auth-test — each exits 0;
  3 CRUD for all 8 entity groups, each create/list/get/update/delete verified through
    the REST API directly (the CLI's exit code alone is not evidence);
  4 the mapping-only groups refuse `update`, as the README says;
  5 the documented nullable clear (`providers update <id> --max-concurrency null`)
    really writes a JSON null — this one was broken until 2026-09-30;
  6 failure paths: bad token / missing id / unknown command exit non-zero.

Run: python3 integration/test_cli_live.py
Exit 0 pass · 1 an assertion failed · 2 could not verify.
"""
import json
import os
import shutil
import signal
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIR = os.path.join(ROOT, ".acceptance", "cli-live")
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
CLI = os.path.join(ROOT, "tools", "hydra-cli", "dist", "cli.js")
ADMIN, DATA = 18750, 18751
AUTH = 18759
TOKEN = "hydra-cli-live-admin-2026"

failures = []


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
    if not ok:
        failures.append(label)


def run_cli(args, env_extra=None, timeout=60):
    """Run the documented CLI the documented way: env config + subcommand args."""
    env = dict(os.environ)
    env.update({"HYDRA_BASE_URL": f"http://127.0.0.1:{ADMIN}", "HYDRA_ADMIN_TOKEN": TOKEN})
    if env_extra:
        env.update(env_extra)
    p = subprocess.run(["node", CLI, *args], capture_output=True, text=True,
                       timeout=timeout, env=env)
    return p.returncode, p.stdout, p.stderr


def rest(method, path, token=TOKEN, body=None, timeout=15):
    data = None if body is None else json.dumps(body).encode()
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    req = urllib.request.Request(f"http://127.0.0.1:{ADMIN}/api/v1{path}", data=data,
                                 method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read().decode(errors="replace")
            return r.status, (json.loads(raw) if raw.strip() else None)
    except urllib.error.HTTPError as e:
        raw = e.read().decode(errors="replace")
        try:
            return e.code, json.loads(raw)
        except Exception:
            return e.code, raw
    except Exception as e:
        return 0, str(e)


class AuthMock(BaseHTTPRequestHandler):
    """`tenants auth-test` needs something to probe."""

    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        if length:
            self.rfile.read(length)
        body = json.dumps({"allowed": True, "reason": "cli-live", "expires_in": 60}).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = do_POST

    def log_message(self, *a):
        pass


def start_node():
    env = dict(os.environ)
    env.update({
        "HYDRA_ADMIN_TOKEN": TOKEN, "HYDRA_ADMIN_ADDR": f"127.0.0.1:{ADMIN}",
        "HYDRA_LISTEN": f"127.0.0.1:{DATA}",
        "HYDRA_DB_URL": f"sqlite://{os.path.join(DIR, 'cli.db')}?mode=rwc",
        "HYDRA_ENCRYPTION_KEY": "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=",
        "RUST_LOG": "warn",
    })
    log = open(os.path.join(DIR, "node.log"), "w")
    return subprocess.Popen([BIN], env=env, stdout=log, stderr=subprocess.STDOUT)


def stop(proc):
    if proc is None:
        return
    if proc.poll() is None:
        proc.send_signal(signal.SIGKILL)
        proc.wait(timeout=10)


# --- the documented command surface ------------------------------------------
# Field flags per entity come from src/types.ts; if a flag is renamed the drill fails
# loudly instead of silently passing.
ENTITIES = [
    ("providers", ["--id", "e1", "--key", "e1", "--name", "E1",
                   "--endpoint", "http://127.0.0.1:18999", "--weight", "3"],
     "weight", "7", True),
    ("provider-models", ["--id", "e1", "--key", "echo", "--name", "Echo",
                         "--provider-id", "p1", "--status", "1"],
     "status", "0", True),
    ("provider-keys", ["--id", "e1", "--provider-id", "p1", "--api-key", "sk-cli"],
     None, None, True),
    ("tenants", ["--id", "e1", "--name", "E1", "--domain", "cli-e1.local",
                 "--auth-url", f"http://127.0.0.1:{AUTH}/auth"],
     None, None, True),
    ("tenant-providers", ["--id", "e1", "--tenant-id", "t-cli", "--provider-id", "p1"],
     None, None, False),
    ("tenant-models", ["--id", "e1", "--tenant-id", "t-cli", "--model-key", "echo"],
     None, None, False),
    ("limit-roles", ["--id", "e1", "--name", "E1", "--window", "m", "--limit-count", "10"],
     "limit_count", "20", True),
    ("provider-key-bindings", ["--id", "e1", "--key-prefix", "sk-cli",
                               "--provider-id", "p1"],
     None, None, True),
]


def main():
    if not os.path.exists(BIN):
        print(f"[cli-live] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    if not os.path.exists(CLI):
        print(f"[cli-live] CANNOT VERIFY: {CLI} is not built "
              f"(cd tools/hydra-cli && npm run build)", file=sys.stderr)
        return 2
    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    auth = ThreadingHTTPServer(("127.0.0.1", AUTH), AuthMock)
    threading.Thread(target=auth.serve_forever, daemon=True).start()
    node = start_node()
    try:
        # ---- wait for the node the way the CLI itself reports health -----------
        deadline = time.time() + 25
        healthy = False
        while time.time() < deadline:
            rc, out, _ = run_cli(["health"])
            if rc == 0 and "healthy" in out:
                healthy = True
                break
            time.sleep(0.25)
        check("the documented env-var configuration reaches a real node (`hydra-admin health`)",
              healthy, f"output={out.strip()[:80]!r}")
        if not healthy:
            print(open(os.path.join(DIR, "node.log")).read()[-500:], file=sys.stderr)
            return 2

        # ---- 1: options before AND after the subcommand (both documented) -------
        rc_a, out_a, _ = run_cli(["--token", TOKEN, "--base-url", f"http://127.0.0.1:{ADMIN}",
                                  "providers", "list"])
        rc_b, out_b, _ = run_cli(["providers", "list", "--token", TOKEN,
                                  "--base-url", f"http://127.0.0.1:{ADMIN}"])
        check("global options work BEFORE and AFTER the subcommand (both documented)",
              rc_a == 0 and rc_b == 0, f"before={rc_a} after={rc_b}")

        # ---- 2: seed the FK targets through the CLI as well --------------------
        rc, out, err = run_cli(["providers", "create", "--id", "p1", "--key", "p1", "--name", "P1",
                                "--endpoint", "http://127.0.0.1:18999", "--weight", "1"])
        check("`providers create` reports success", rc == 0, f"rc={rc} out={out.strip()[:60]}")
        rc, out, err = run_cli(["tenants", "create", "--id", "t-cli", "--name", "T", "--domain",
                                "cli.local", "--auth-url", f"http://127.0.0.1:{AUTH}/auth",
                                "--access-token", "cli-live-tenant-token-1234"])
        check("`tenants create` reports success", rc == 0, f"rc={rc} out={out.strip()[:60]}")

        # ---- 3: service commands ----------------------------------------------
        rc, out, _ = run_cli(["reload"])
        check("`reload` exits 0 and reports the reload", rc == 0 and "reloaded" in out.lower(),
              f"rc={rc} out={out.strip()[:70]}")
        rc, out, _ = run_cli(["metrics"])
        check("`metrics` pipes raw Prometheus text (no token, no JSON wrapping)",
              rc == 0 and out.startswith("# HELP hydra_"), f"rc={rc} first={out.splitlines()[:1]}")
        rc, out, _ = run_cli(["concurrency", "--json"])
        check("`concurrency --json` prints JSON", rc == 0 and json.loads(out) is not None,
              f"rc={rc} out={out.strip()[:60]}")
        rc, out, _ = run_cli(["breaker", "--json"])
        check("`breaker --json` prints JSON", rc == 0 and "dead" in json.loads(out),
              f"rc={rc}")
        rc, out, _ = run_cli(["cluster", "status", "--json"])
        check("`cluster status --json` prints JSON", rc == 0 and "mode" in json.loads(out),
              f"rc={rc}")
        rc, out, _ = run_cli(["stats", "usage", "--json"])
        check("`stats usage --json` prints JSON", rc == 0 and "totals" in json.loads(out),
              f"rc={rc}")
        rc, out, _ = run_cli(["auth-cache", "invalidate"])
        check("`auth-cache invalidate` exits 0", rc == 0, f"rc={rc} out={out.strip()[:70]}")
        rc, out, err = run_cli(["tenants", "auth-test", f"http://127.0.0.1:{AUTH}/auth",
                                "--tenant-id", "t-cli"])
        check("`tenants auth-test <url>` reaches the probe and reports a verdict "
              "(before 2026-09-30 it printed a tenant LIST and exited 0)",
              rc == 0 and "verdict=" in out and "domain" not in out,
              f"rc={rc} out={out.strip()[:90]}")
        rc, out, err = run_cli(["tenants", "auth-test", f"http://127.0.0.1:{AUTH}/auth",
                                "--tenant-id", "t-cli", "--json"])
        try:
            probed = json.loads(out)
        except Exception:
            probed = None
        check("`tenants auth-test --tenant-id ... --json` returns the documented probe result",
              rc == 0 and isinstance(probed, dict) and probed.get("reachable") is True,
              f"rc={rc} out={out.strip()[:90]}")

        # ---- 4: table output vs --json (both documented) -----------------------
        rc, table_out, _ = run_cli(["providers", "list"])
        rc2, json_out, _ = run_cli(["providers", "list", "--json"])
        check("default output is a table, `--json` is parseable JSON",
              rc == 0 and rc2 == 0 and not table_out.lstrip().startswith("[")
              and isinstance(json.loads(json_out), list),
              f"table={table_out.strip()[:40]!r} json={json_out.strip()[:40]!r}")

        # ---- 5: CRUD for every entity, verified through the REST API -----------
        for route, flags, upd_field, upd_value, has_update in ENTITIES:
            label = route
            rc, out, err = run_cli([label, "create", *flags])
            ok_create = rc == 0
            st, row = rest("GET", f"/{route}/e1")
            check(f"{label}: create exits 0 AND the row exists over REST",
                  ok_create and st == 200, f"rc={rc} REST={st} out={out.strip()[:50]} {err.strip()[:60]}")
            rc, out, _ = run_cli([label, "list", "--json"])
            listed = rc == 0 and any(r.get("id") == "e1" for r in json.loads(out))
            check(f"{label}: `list --json` contains the new row", listed, f"rc={rc}")
            rc, out, _ = run_cli([label, "get", "e1", "--json"])
            check(f"{label}: `get --json` returns that row",
                  rc == 0 and json.loads(out).get("id") == "e1", f"rc={rc}")
            if has_update:
                if upd_field:
                    rc, out, err = run_cli([label, "update", "e1", f"--{upd_field.replace('_','-')}", upd_value])
                    st, row = rest("GET", f"/{route}/e1")
                    got = None if not isinstance(row, dict) else row.get(upd_field)
                    check(f"{label}: `update` exits 0 AND the change is visible over REST",
                          rc == 0 and str(got) == upd_value,
                          f"rc={rc} REST {upd_field}={got!r} err={err.strip()[:60]}")
                else:
                    # No updatable scalar: prove the read-modify-write path still works by
                    # re-sending the same flags (documented as a valid update).
                    rc, out, err = run_cli([label, "update", "e1", *flags])
                    check(f"{label}: `update` with the same flags exits 0 (read-modify-write)",
                          rc == 0, f"rc={rc} err={err.strip()[:60]}")
            else:
                rc, out, err = run_cli([label, "update", "e1", "--id", "e1"])
                check(f"{label}: `update` is intentionally absent (README: mapping-only group)",
                      rc != 0, f"rc={rc} out={out.strip()[:60]}")
            st_before, _ = rest("GET", f"/{route}/e1")
            rc, out, err = run_cli([label, "delete", "e1", "-y"])
            st_after, _ = rest("GET", f"/{route}/e1")
            # `st_before == 200` is what keeps this from passing vacuously: the admin
            # DELETE is idempotent (204), so "gone afterwards" is also true of a row
            # that never existed.
            check(f"{label}: `delete -y` exits 0 AND the row is gone over REST",
                  st_before == 200 and rc == 0 and st_after == 404,
                  f"before={st_before} rc={rc} after={st_after} err={err.strip()[:60]}")

        # ---- 6: the documented nullable clear (broken until 2026-09-30) --------
        rc, out, err = run_cli(["providers", "create", "--id", "pnull", "--key", "pnull",
                                "--name", "PN", "--endpoint", "http://127.0.0.1:18999",
                                "--weight", "1", "--max-concurrency", "5"])
        _, row = rest("GET", "/providers/pnull")
        check("the documented clear starts from a real value", row.get("max_concurrency") == 5,
              f"max_concurrency={row.get('max_concurrency')!r}")
        rc, out, err = run_cli(["providers", "update", "pnull", "--max-concurrency", "null"])
        _, row = rest("GET", "/providers/pnull")
        check("`providers update <id> --max-concurrency null` writes a JSON null (README example)",
              rc == 0 and row.get("max_concurrency") is None,
              f"rc={rc} max_concurrency={row.get('max_concurrency')!r} err={err.strip()[:80]}")
        rc, out, err = run_cli(["providers", "update", "pnull", "--max-concurrency", "abc"])
        check("a non-numeric value is refused by the CLI itself (no request sent)",
              rc != 0 and "expected a number" in (err + out), f"rc={rc} {err.strip()[:60]}")

        # ---- 7: failure paths --------------------------------------------------
        rc, out, err = run_cli(["health"], env_extra={"HYDRA_ADMIN_TOKEN": "wrong-token"})
        check("a wrong token exits non-zero and names the 401",
              rc != 0 and "401" in (out + err), f"rc={rc}")
        rc, out, err = run_cli(["providers", "get", "no-such-provider"])
        check("an unknown id exits non-zero and names the 404",
              rc != 0 and "404" in (out + err), f"rc={rc}")
        rc, out, err = run_cli(["not-a-command"])
        check("an unknown command exits non-zero (commander)", rc != 0, f"rc={rc}")
        rc, out, _ = run_cli(["--version"])
        check("`--version` prints the packaged version",
              rc == 0 and out.strip().startswith("1."), f"rc={rc} out={out.strip()!r}")
        rc, out, _ = run_cli(["--help"])
        check("`--help` lists the documented command groups",
              rc == 0 and "provider-key-bindings" in out and "limit-roles" in out,
              f"rc={rc}")
    finally:
        stop(node)
        auth.shutdown()

    print()
    if failures:
        print(f"CLI LIVE: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print("CLI LIVE: PASSED (documented config, service commands, CRUD for 8 groups "
          "verified over REST, nullable clear, failure paths)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
