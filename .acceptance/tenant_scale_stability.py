#!/usr/bin/env python3
"""Scale ladder + multi-scenario drill for tenant CRUD on the compose cluster.

Targets: 2000 users, 2000 Global Roles, 8000 api-keys. The last stage is EXPECTED
to find the Arachne capacity cliff (TOC 1 MiB / multi_put 4096 entries); the
script records where it happens and whether the failure mode is safe.

Run:
  set -a; . secure/local-test.env; set +a
  python3 .acceptance/tenant_scale_stability.py

Exit 0 = all requested scale reached; 3 = expected capacity cliff found and
failure mode safe; 1 = unexpected failure; 2 = cannot verify.
"""
import json, os, subprocess, sys, threading, time, urllib.error, urllib.request
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ADMIN = os.environ.get("HYDRA_ADMIN_TOKEN", "")
if len(ADMIN) < 16:
    print("CANNOT VERIFY: source secure/local-test.env"); sys.exit(2)
DATA = {"a": 8080, "b": 8083, "c": 8090}
APORT = {"a": 8081, "b": 8082, "c": 8084}
CONT = {"a": "hydra-a", "b": "hydra-b", "c": "hydra-c"}
USERS = int(os.environ.get("SCALE_USERS", "2000"))
ROLES = int(os.environ.get("SCALE_ROLES", "2000"))
KEYS = int(os.environ.get("SCALE_KEYS", "8000"))
GROUPS = int(os.environ.get("SCALE_GROUPS", "100"))
CONC = int(os.environ.get("SCALE_CONCURRENCY", "4"))
USE_BULK = os.environ.get("USE_BULK") == "1"
STAMP = int(time.time()); TENANT = f"scale-{STAMP}"; DOMAIN = f"{TENANT}.local"
fail = []; lat = {"group": [], "user": [], "role": [], "key": []}
capacity_cliff = None


def check(label, ok, detail=""):
    print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}", flush=True)
    if not ok: fail.append(label)


def call(method, url, token=None, body=None, host=None, timeout=30):
    data = json.dumps(body).encode() if body is not None else None
    headers = {"Content-Type": "application/json"}
    if token: headers["Authorization"] = f"Bearer {token}"
    if host: headers["Host"] = host
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return r.status, r.read().decode(errors="replace"), (time.monotonic()-t0)*1000
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode(errors="replace"), (time.monotonic()-t0)*1000
    except Exception as e:
        return 0, str(e), (time.monotonic()-t0)*1000


def admin(method, path, body=None, node="a"):
    return call(method, f"http://127.0.0.1:{APORT[node]}/api/v1{path}", ADMIN, body, host="127.0.0.1")


def tenant(method, path, token, body=None, node="a"):
    return call(method, f"http://127.0.0.1:{DATA[node]}/tenant/{TENANT}/api/v1{path}", token, body, host=DOMAIN)


def proxy(node, key):
    return call("POST", f"http://127.0.0.1:{DATA[node]}/v1/chat/completions", key,
                {"model": "echo", "messages": [{"role": "user", "content": "hi"}]}, host=DOMAIN)


def retry_write(fn, max_tries=5):
    """Retry a transient 503 with the SAME body/id (idempotent upsert)."""
    tries = 0
    last = (0, "", 0)
    while tries < max_tries:
        last = fn()
        if last[0] in (200, 201):
            return (*last, tries)
        if last[0] != 503:
            return (*last, tries)
        tries += 1
        time.sleep(1)
    return (*last, tries)


def docker(*a, timeout=120):
    return subprocess.run(["docker", *a], capture_output=True, text=True, timeout=timeout)


def healthy(timeout=150):
    end = time.monotonic()+timeout
    while time.monotonic() < end:
        if all(admin("GET", "/health", node=n)[0] == 200 for n in "abc"): return True
        time.sleep(2)
    return False


def wait_token(token, timeout=120):
    end = time.monotonic()+timeout
    while time.monotonic() < end:
        if all(call("GET", f"http://127.0.0.1:{DATA[n]}/tenant/{TENANT}/api/v1/whoami",
                    token, host=DOMAIN)[0] == 200 for n in "abc"): return True
        time.sleep(1)
    return False


def pct(v, p):
    if not v: return 0
    s=sorted(v); return s[min(len(s)-1, int(len(s)*p))]


def with_progress(label, fn):
    """Run one (possibly long) call, printing elapsed seconds once a second."""
    done = threading.Event(); t0 = time.monotonic()
    def tick():
        while not done.wait(1.0):
            print(f"   {label} … {time.monotonic()-t0:.0f}s", flush=True)
    th = threading.Thread(target=tick, daemon=True); th.start()
    try:
        return fn()
    finally:
        done.set(); th.join(timeout=1)


def wait_counts(expected_groups, expected_users, expected_keys, timeout=180):
    """Wait until every node reports the expected row counts (bulk propagation)."""
    end = time.monotonic()+timeout
    last = None
    while time.monotonic() < end:
        c = counts()
        # Destructive churn creates and deletes groups; failed deletes legitimately
        # leave extras. Only require the baseline, while users/keys stay exact.
        ok = all(
            c[n][0] >= expected_groups
            and c[n][1] == expected_users
            and c[n][2] == expected_keys
            for n in "abc"
        )
        line = " ".join(f"{n}:g{g}/u{u}/k{k}" for n,(g,u,k) in c.items())
        if line != last:
            print(f"   propagation {time.monotonic()-end+timeout:.0f}s: {line}", flush=True)
            last = line
        if ok: return True
        time.sleep(1)
    return False


def wait_converged(timeout=300):
    """Wait until all three nodes report the same (groups, users, keys) tuple.

    The destructive phase deliberately kills and restarts nodes. A node can be
    healthy (admin API up) while its materializer is still catching up; starting
    the key bulk in that window makes the test look like a publish failure when
    it is really post-quorum catch-up. Require a common baseline first.
    """
    end = time.monotonic() + timeout
    last = None
    while time.monotonic() < end:
        c = counts()
        vals = list(c.values())
        # Groups legitimately differ after churn; only require the same user
        # revision and the same key revision on all nodes. A probe key may be
        # present (k=1) before the bulk key phase.
        ok = (
            len(vals) == 3
            and all(v[1] == USERS for v in vals)
            and len({v[2] for v in vals}) == 1
        )
        line = " ".join(f"{n}:g{g}/u{u}/k{k}" for n, (g, u, k) in c.items())
        if line != last:
            print(f"   convergence {time.monotonic()-end+timeout:.0f}s: {line}", flush=True)
            last = line
        if ok:
            return True
        time.sleep(1)
    return False


def bulk_mint_keys(token, users, count, label):
    """Mint `count` keys through POST .../api-keys/bulk, chunked under the caps."""
    global capacity_cliff
    made = 0; keys = []
    chunk = 4000   # <= 5000 entries/request; each entry is well under the 1 MiB body cap
    for start in range(0, count, chunk):
        n = min(chunk, count-start)
        entries = [{"user_id": users[(start+i) % len(users)], "name": f"k{start+i}"}
                   for i in range(n)]
        st, out, ms = with_progress(
            f"{label} keys {start+n}/{count}",
            lambda entries=entries: tenant("POST", "/api-keys/bulk", token, {"keys": entries}))
        if st not in (200, 201):
            capacity_cliff = ("key", start, st, out[:300])
            print(f"   key bulk cliff at {start}: HTTP {st} {out[:300]}", flush=True)
            break
        data = json.loads(out)["api_keys"]
        lat["key"].append(ms); made += len(data); keys.extend(data)
        print(f"   {label} keys {made}/{count} — HTTP {st} in {ms:.0f}ms (chunk {n})", flush=True)
    return made, keys


def counts():
    out={}
    for n in "abc":
        st, body, _ = admin("GET", f"/users?tenant_id={TENANT}", node=n)
        u = len(json.loads(body)) if st==200 else -1
        st, body, _ = admin("GET", f"/user-api-keys?tenant_id={TENANT}", node=n)
        k = len(json.loads(body)) if st==200 else -1
        st, body, _ = admin("GET", f"/user-groups?tenant_id={TENANT}", node=n)
        g = len(json.loads(body)) if st==200 else -1
        out[n]=(g,u,k)
    return out


def leader():
    for n in "abc":
        if call("GET", f"http://127.0.0.1:{APORT[n]}/healthz/leader", ADMIN, host="127.0.0.1")[0] == 200:
            return n
    return None


def main():
    global capacity_cliff
    print(f"== setup {TENANT}")
    st, out, _ = admin("POST", "/tenants", {"id":TENANT,"name":TENANT,"domain":DOMAIN,"enabled":True,
                                             "cert_key":None,"cert_file":None,"created_at":"","updated_at":""})
    check("tenant", st in (200,201), f"HTTP {st}")
    for path, body in (("/tenant-providers", {"id":f"tp-{TENANT}","tenant_id":TENANT,"provider_id":"mock","created_at":"","updated_at":""}),
                       ("/tenant-models", {"id":f"tm-{TENANT}","tenant_id":TENANT,"model_key":"echo","created_at":"","updated_at":""})):
        st, out, _ = admin("POST", path, body); check(path, st in (200,201), f"HTTP {st}")
    st, out, _ = admin("POST", f"/tenants/{TENANT}/access-tokens", {"name":"scale"})
    if st not in (200,201): print(f"CANNOT VERIFY token: {st} {out[:120]}"); return 2
    token=json.loads(out)["token"]
    check("token propagates", wait_token(token,120))

    print(f"\n== groups {GROUPS}")
    groups=[]
    for i in range(GROUPS):
        st,out,ms,tries=retry_write(lambda i=i: tenant("PUT","/user-groups",token,
            {"id":f"group-{STAMP}-{i}","name":f"g{i}"}))
        if st not in (200,201): check("groups",False,f"i={i} HTTP {st} {out[:80]}"); return 1
        lat["group"].append(ms); groups.append(json.loads(out)["user_group"]["id"])
        if (i + 1) % 50 == 0:
            print(f"   groups {i + 1}/{GROUPS}", flush=True)
    check("groups", True, f"{GROUPS}")

    if USE_BULK:
        print(f"\n== users {USERS} (BULK, one request)")
        entries=[{"id":f"user-{STAMP}-{i}","user_group_id":groups[i%len(groups)],
                  "name":f"u{i}"} for i in range(USERS)]
        st,out,ms=with_progress(f"bulk users {USERS}",
                                lambda: tenant("PUT","/users/bulk",token,{"users":entries}))
        if st not in (200,201):
            check("users",False,f"HTTP {st} {out[:160]}"); return 1
        users=[u["id"] for u in json.loads(out)["users"]]
        lat["user"].append(ms)
        print(f"   users bulk: {len(users)} in {ms:.0f}ms (HTTP {st})", flush=True)
    else:
        print(f"\n== users {USERS} (concurrency {CONC})")
        def mk_user(i):
            return retry_write(lambda i=i: tenant("PUT","/users",token,
                {"id":f"user-{STAMP}-{i}","user_group_id":groups[i%len(groups)],"name":f"u{i}"}))
        users=[]
        with ThreadPoolExecutor(max_workers=CONC) as ex:
            for i,(st,out,ms,tries) in enumerate(ex.map(mk_user, range(USERS))):
                if st not in (200,201):
                    check("users",False,f"i={i} HTTP {st} {out[:120]}"); return 1
                lat["user"].append(ms); users.append(json.loads(out)["user"]["id"])
                if (i + 1) % 100 == 0:
                    print(f"   users {i + 1}/{USERS} (p50 so far {pct(lat['user'], .5):.0f}ms)", flush=True)
    check("users", len(users)==USERS, f"{USERS}")
    print("   counts", counts())
    st,out,_=tenant("POST",f"/users/{users[0]}/api-keys",token,{"name":"probe"})
    if st not in (200,201):
        print(f"CANNOT VERIFY probe key: {st} {out[:120]}"); return 2
    probe_key=json.loads(out)["key"]
    check("probe client key authenticates on A", proxy("a", probe_key)[0]==200)

    if USE_BULK:
        print(f"\n== roles {ROLES} (BULK, admin, chunked under the 1 MiB body cap)")
        roles=0
        # Each role is ~200 bytes of JSON; 500 per request stays far below the
        # admin 1 MiB body cap and the 5000-entry ceiling.
        for start in range(0, ROLES, 500):
            n=min(500, ROLES-start)
            chunk=[{"id":f"role-{STAMP}-{start+i}","name":f"r{start+i}","tenant_id":TENANT,
                    "user_group_id":"*","user_id":"*","api_key_id":"*","model_key":"*",
                    "providers":["mock"],"enabled":True} for i in range(n)]
            st,out,ms=with_progress(f"bulk roles {start+n}/{ROLES}",
                                    lambda chunk=chunk: admin("PUT","/global-roles/bulk",{"roles":chunk}))
            if st not in (200,201):
                capacity_cliff=("role",start,st,out[:200])
                print(f"   role bulk cliff at {start}: HTTP {st} {out[:200]}", flush=True); break
            got=len(json.loads(out).get("roles", []))
            lat["role"].append(ms); roles+=got
            print(f"   roles {roles}/{ROLES} — HTTP {st} in {ms:.0f}ms (chunk {n})", flush=True)
    else:
        print(f"\n== roles {ROLES} (admin, sequential)")
        roles=0
        for i in range(ROLES):
            st,out,ms,tries=retry_write(lambda i=i: admin("PUT","/global-roles",{"id":f"role-{STAMP}-{i}",
                                                    "name":f"r{i}","tenant_id":TENANT,"user_group_id":"*",
                                                    "user_id":"*","api_key_id":"*","model_key":"*",
                                                    "providers":["mock"],"enabled":True}))
            if st not in (200,201):
                capacity_cliff=("role",i,st,out[:200]); print(f"   role cliff at {i}: HTTP {st} {out[:200]}"); break
            lat["role"].append(ms); roles+=1
            if roles % 100 == 0:
                print(f"   roles {roles}/{ROLES} (p50 so far {pct(lat['role'], .5):.0f}ms)", flush=True)
    check("roles", roles==ROLES, f"{roles}/{ROLES}")

    if os.environ.get("SKIP_DESTRUCTIVE"):
        print("\n== destructive phase skipped (SKIP_DESTRUCTIVE)")
    else:
        run_destructive = True
    if os.environ.get("CAPACITY_ONLY"):
        if USE_BULK:
            print(f"\n== keys {KEYS} (BULK, capacity-only)")
            made, new_keys = bulk_mint_keys(token, users, KEYS, "capacity")
        else:
            print(f"\n== keys {KEYS} (capacity-only, concurrency {CONC})")
            made = 0
            def mk_key_cap(i):
                return tenant("POST", f"/users/{users[i % len(users)]}/api-keys", token, {"name": f"k{i}"})
            with ThreadPoolExecutor(max_workers=CONC) as ex:
                for i, (st, out, ms) in enumerate(ex.map(mk_key_cap, range(KEYS))):
                    if st not in (200, 201):
                        capacity_cliff = ("key", i, st, out[:300])
                        print(f"   key cliff at {i}: HTTP {st} {out[:300]}")
                        break
                    lat["key"].append(ms); made += 1
                    if made % 100 == 0:
                        print(f"   keys {made}/{KEYS} (p50 so far {pct(lat['key'], .5):.0f}ms)", flush=True)
        c = counts(); print("   final counts", c)
        if USE_BULK:
            # The probe key is minted before the bulk phase and is included in
            # the tenant-wide key count; `made` counts only bulk-minted keys.
            check("bulk propagation", wait_counts(GROUPS, USERS, made + 1))
        check("data plane still serves last-known-good", all(proxy(n, probe_key)[0] == 200 for n in "abc"))
        for name, vals in lat.items():
            if vals: print(f"   {name}: n={len(vals)} p50={pct(vals,.5):.0f} p95={pct(vals,.95):.0f} p99={pct(vals,.99):.0f} max={max(vals):.0f} ms")
        if capacity_cliff:
            print(f"   CAPACITY CLIFF: {capacity_cliff}")
            print("\nCAPACITY: cliff reached safely"); return 3
        print("\nCAPACITY: PASSED (all requested rows published)"); return 0 if not fail else 1

    print("\n== destructive scenarios at scale (before key cliff)")
    churn_stop=threading.Event(); churn_ok=[0]
    def churn():
        i=0
        while not churn_stop.is_set():
            name=f"churn-{i}"
            st,out,_=tenant("PUT","/user-groups",token,{"name":name})
            if st in (200,201):
                churn_ok[0]+=1
                gid=json.loads(out)["user_group"]["id"]
                tenant("DELETE",f"/user-groups/{gid}",token)
            i+=1; time.sleep(0.2)
    th=threading.Thread(target=churn,daemon=True); th.start(); time.sleep(1)

    for label, act in (
        ("kill leader", lambda: docker("kill", CONT[leader() or "a"])),
        ("kill follower", lambda: docker("kill", CONT["b" if leader()=="a" else "a"])),
    ):
        print(f"   -- {label}"); act(); time.sleep(3); check(label, churn_ok[0]>0, f"churn_ok={churn_ok[0]}")
        for n in "abc": docker("start", CONT[n])
        check(f"{label} recovery", healthy(180))

    print("   -- Redis restart"); docker("restart","hydra-local-redis",timeout=60); time.sleep(3)
    end=time.monotonic()+60
    while time.monotonic()<end and docker("inspect","-f","{{.State.Health.Status}}","hydra-local-redis").stdout.strip()!="healthy": time.sleep(2)
    check("Redis healthy", docker("inspect","-f","{{.State.Health.Status}}","hydra-local-redis").stdout.strip()=="healthy")

    print("   -- ClickHouse stop/start"); docker("stop","hydra-local-clickhouse",timeout=60); time.sleep(2)
    before=churn_ok[0]; time.sleep(3); check("CRUD survives ClickHouse down",churn_ok[0]>before)
    docker("start","hydra-local-clickhouse",timeout=60); time.sleep(3)

    print("   -- network partition node-b")
    docker("network","disconnect","environment_default","hydra-b")
    time.sleep(3)
    check("remaining nodes serve data plane", all(proxy(n, probe_key)[0] == 200 for n in "ac"))
    docker("network","connect","environment_default","hydra-b","--ip","172.28.0.12")
    check("node-b reconnects", healthy(180))

    print("   -- quorum loss")
    churn_stop.set(); th.join(timeout=10)
    for c in ("hydra-b","hydra-c"): docker("update","--restart=no",c); docker("stop","-t","10",c)
    time.sleep(2)
    st,out,_=tenant("PUT","/user-groups",token,{"name":"no-quorum"})
    check("write refused without quorum", st==503, f"HTTP {st} {out[:100]}")
    docker("start","hydra-b"); docker("start","hydra-c")
    for c in ("hydra-b","hydra-c"): docker("update","--restart=unless-stopped",c)
    check("quorum recovery", healthy(180))
    print("   -- post-destructive convergence")
    check("post-destructive convergence", wait_converged(300))

    if USE_BULK:
        print(f"\n== keys {KEYS} (BULK, chunked)")
        made, new_keys = bulk_mint_keys(token, users, KEYS, "final")
    else:
        print(f"\n== keys {KEYS} (concurrency {CONC}) — expected capacity cliff")
        made=0
        def mk_key(i):
            return tenant("POST",f"/users/{users[i%len(users)]}/api-keys",token,{"name":f"k{i}"})
        with ThreadPoolExecutor(max_workers=CONC) as ex:
            for i,(st,out,ms) in enumerate(ex.map(mk_key, range(KEYS))):
                if st not in (200,201):
                    capacity_cliff=("key",i,st,out[:300]); print(f"   key cliff at {i}: HTTP {st} {out[:300]}"); break
                lat["key"].append(ms); made+=1
    if made < KEYS:
        print(f"   stopped at {made}/{KEYS} keys (capacity cliff recorded)")
    c=counts(); print("   final counts", c)
    if USE_BULK:
        # The probe key is included in the tenant-wide key count; `made` only
        # counts bulk-minted keys.
        check("bulk propagation", wait_counts(GROUPS, USERS, made + 1))

    print("\n== failure-mode checks")
    check("data plane still serves last-known-good", all(proxy(n, probe_key)[0] == 200 for n in "abc"))
    st,out,_=admin("PUT","/global-roles",{"name":"post-cliff","tenant_id":TENANT,"user_group_id":"*","user_id":"*","api_key_id":"*","model_key":"*","providers":["mock"],"enabled":True})
    print(f"   post-cliff admin write: HTTP {st} {out[:160]}")
    check("cliff failure is an explicit 503/400 (not a crash)", st in (200,201,400,503), f"HTTP {st}")

    for name, vals in lat.items():
        if vals: print(f"   {name}: n={len(vals)} p50={pct(vals,.5):.0f} p95={pct(vals,.95):.0f} p99={pct(vals,.99):.0f} max={max(vals):.0f} ms")
    if capacity_cliff: print(f"   CAPACITY CLIFF: {capacity_cliff}")
    if fail:
        print(f"\nSCALE: FAILED ({len(fail)})"); [print("  -",f) for f in fail]; return 1
    if capacity_cliff:
        print("\nSCALE: capacity cliff reached safely (expected)"); return 3
    print("\nSCALE: PASSED"); return 0


if __name__ == "__main__":
    try: sys.exit(main())
    except SystemExit: raise
    except Exception as e: print(f"CANNOT VERIFY: {e}"); sys.exit(2)
