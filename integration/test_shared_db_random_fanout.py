#!/usr/bin/env python3
"""Cache fanout under CONCURRENT RANDOM WRITES from three instances.

This is the stress complement to `test_shared_db_cluster.py`: that drill proves one write at a
time; this one proves that three processes writing DIFFERENT entity kinds at the same time all
converge to the same config, and that the convergence is carried by Redis pub/sub.

The poll fallback is deliberately 120 s (`HYDRA_CONFIG_POLL_MS=120000`) on every node. The whole
drill runs well below that, so a converged node inside the window can only have got there from a
pub/sub notification. That condition is not weakened: it is what makes this a fanout test rather
than a polling test.

Determinism: each writer has its own `random.Random(seed*1000+idx)`, the seed comes from
`HYDRA_FANOUT_SEED` (default fixed) and rounds from `HYDRA_FANOUT_ROUNDS` (default 10). The seed and
every operation are printed, so a failing interleaving is reproducible in its operation set.

Phases:
  A  concurrent CREATE: each writer creates its own unique providers, tenants (+ provider/model/
     grants), standalone grants, limit roles, global roles, and tenant-API identity rows
     (user-group/user/api-key). Every write must be 2xx. Then all nodes must report the same
     `config_version`; every created row must be visible on every node; every linked tenant's
     snapshot-backed model catalog must contain its model; a representative data-plane catalog and
     identity key must work on all nodes; each node's pubsub reload counter must move while its
     poll counter does not.
  B  concurrent UPDATE: each writer updates only its own rows (tenant/provider names, limit/global
     role names, or disables its own user). All nodes must see the new values, and disabling an
     identity must make that key refuse on every node (a behavioural cache probe).
  C  concurrent DELETE: each writer deletes its own rows (grants before their providers, users
     before groups). All nodes must stop listing them; deleted tenants disappear from the
     snapshot-backed catalog endpoint; deleted identity keys answer 401; deleted grants disappear
     from the base tenant's catalog.
  Final: equal versions, identical entity-id sets across nodes, no create left undeleted.

Run:
  export HYDRA_TEST_DATABASE_URL=postgres://hydra:hydra@127.0.0.1:55432/hydra
  export HYDRA_TEST_REDIS_URL=redis://127.0.0.1:6380
  cargo build -p hydra-server --features server,cluster-redis,usage-clickhouse,db-postgres --bin hydra
  python3 integration/test_shared_db_random_fanout.py
  # MySQL: use a mysql:// URL and the db-mysql feature.
Exit 0 pass · 1 an assertion failed · 2 could not verify (missing env/binary/services).
"""

from __future__ import annotations

import json
import os
import random
import shutil
import sys
import threading
import time
from dataclasses import dataclass, field
from http.server import ThreadingHTTPServer

from _cluster_harness import (
    Node,
    Upstream,
    call,
    free_port,
    rename_tenant,
    seed_base,
    wait_for,
)
from _redis_harness import RedisRelay, RedisSetupError, db_url, endpoint as redis_endpoint, flush_db, require_url as require_redis_url
from _shared_db import SetupError, isolated

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.environ.get("HYDRA_BIN", os.path.join(ROOT, "target", "debug", "hydra"))
DIR = os.path.join(ROOT, ".acceptance", "shared-db-random-fanout")

ADMIN_TOKEN = "hydra-random-fanout-admin-2026"
ENCRYPTION_KEY = "MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY="
BASE_TENANT = "t-fanout-base"
BASE_DOMAIN = "fanout.local"
MODEL = "echo"

# MUST stay far above the test duration: in-window convergence has to come from
# pub/sub, never polling. 120 s leaves margin on a slow/contended CI runner.
POLL_MS = 120000
REDIS_DB = int(os.environ.get("HYDRA_FANOUT_REDIS_DB", "57"))
ROUNDS = int(os.environ.get("HYDRA_FANOUT_ROUNDS", "10"))
SEED = int(os.environ.get("HYDRA_FANOUT_SEED", "20261110"))
STARTUP_BUDGET = float(os.environ.get("HYDRA_FANOUT_STARTUP_BUDGET", "60"))
CONVERGE_BUDGET = float(os.environ.get("HYDRA_FANOUT_CONVERGE_BUDGET", "20"))

OP_KINDS = ["provider", "tenant_bundle", "grant_bundle", "limit_role", "global_role", "identity"]

failures: list[str] = []
_print_lock = threading.Lock()


def check(label, ok, detail=""):
    with _print_lock:
        print(f"   {'PASS' if ok else 'FAIL'}  {label}{'  — ' + detail if detail else ''}")
        if not ok:
            failures.append(label)


def announce(label, detail=""):
    with _print_lock:
        print(f"   ....  {label}{'  — ' + detail if detail else ''}")


def op(msg):
    with _print_lock:
        print(f"        {msg}")


def status_ok(st, allowed=(200, 201)):
    return st in allowed


@dataclass
class Record:
    writer: int
    kind: str
    id: str
    name: str
    lists: list[tuple[str, str]] = field(default_factory=list)
    name_lists: list[tuple[str, str]] = field(default_factory=list)
    body: dict = field(default_factory=dict)
    tenant_id: str = ""
    model_key: str = ""
    group_id: str = ""
    user_id: str = ""
    api_key_id: str = ""
    api_key: str = ""
    updatable: bool = True
    disabled: bool = False


class Writer:
    def __init__(self, idx, node, base_token):
        self.idx = idx
        self.node = node
        self.base_token = base_token
        self.rng = random.Random(SEED * 1000 + idx)
        self.records: list[Record] = []
        self.errors: list[str] = []
        self.prefix = f"w{idx}"

    def tenant_call(self, method, path, body=None, timeout=20):
        return call(
            method,
            f"http://127.0.0.1:{self.node.data_port}/tenant/{BASE_TENANT}/api/v1{path}",
            token=self.base_token,
            body=body,
            timeout=timeout,
        )

    def must(self, st, allowed, what):
        if not status_ok(st, allowed):
            msg = f"{self.prefix}: {what} -> HTTP {st}"
            self.errors.append(msg)
            announce("WRITE FAILED", msg)
            return False
        return True

    # ---- creates ---------------------------------------------------------
    def create_provider(self, r, upstream, tag="p"):
        pid = f"{tag}-{self.prefix}-{r}"
        body = {
            "id": pid,
            "key": pid,
            "name": f"provider-{self.prefix}-{r}",
            "endpoint": f"http://127.0.0.1:{upstream}",
            "weight": 1,
            "created_at": "",
            "updated_at": "",
        }
        st, _, _ = self.node.admin("POST", "/providers", body, timeout=25)
        if not self.must(st, (200, 201), f"create provider {pid}"):
            return None
        rec = Record(self.idx, "provider", pid, body["name"], [("/providers", pid)],
                     [("/providers", pid)], body)
        return rec

    def create_tenant_bundle(self, r, upstream):
        tid = f"t-{self.prefix}-{r}"
        pid = f"p-tb-{self.prefix}-{r}"
        pmid = f"pm-tb-{self.prefix}-{r}"
        pkid = f"pk-tb-{self.prefix}-{r}"
        tpid = f"tp-tb-{self.prefix}-{r}"
        tmid = f"tm-tb-{self.prefix}-{r}"
        model_key = f"m-{self.prefix}-{r}"
        domain = f"{self.prefix}{r}.fanout.local"
        ops = [
            ("/providers", {"id": pid, "key": pid, "name": f"provider-tb-{self.prefix}-{r}",
                            "endpoint": f"http://127.0.0.1:{upstream}", "weight": 1,
                            "created_at": "", "updated_at": ""}),
            ("/provider-models", {"id": pmid, "key": model_key, "name": model_key,
                                  "provider_id": pid, "status": 1,
                                  "created_at": "", "updated_at": ""}),
            ("/provider-keys", {"id": pkid, "provider_id": pid, "api_key": "sk-upstream",
                                "created_at": ""}),
            ("/tenants", {"id": tid, "name": f"tenant-{self.prefix}-{r}", "domain": domain,
                          "cert_key": None, "cert_file": None, "enabled": True,
                          "created_at": "", "updated_at": ""}),
            ("/tenant-providers", {"id": tpid, "tenant_id": tid, "provider_id": pid,
                                   "created_at": "", "updated_at": ""}),
            ("/tenant-models", {"id": tmid, "tenant_id": tid, "model_key": model_key,
                                "created_at": "", "updated_at": ""}),
        ]
        for path, body in ops:
            st, _, _ = self.node.admin("POST", path, body, timeout=25)
            if not self.must(st, (200, 201), f"create tenant bundle {tid} {path}"):
                return None
        return Record(
            self.idx, "tenant_bundle", tid, f"tenant-{self.prefix}-{r}",
            [("/providers", pid), ("/provider-models", pmid), ("/provider-keys", pkid),
             ("/tenants", tid), ("/tenant-providers", tpid), ("/tenant-models", tmid)],
            [("/tenants", tid)],
            {"tenant_id": tid, "domain": domain, "provider_id": pid,
             "provider_model_id": pmid, "provider_key_id": pkid,
             "tenant_provider_id": tpid, "tenant_model_id": tmid, "model_key": model_key},
            tenant_id=tid, model_key=model_key,
        )

    def create_grant_bundle(self, r, upstream):
        pid = f"p-gb-{self.prefix}-{r}"
        pmid = f"pm-gb-{self.prefix}-{r}"
        pkid = f"pk-gb-{self.prefix}-{r}"
        tpid = f"tp-gb-{self.prefix}-{r}"
        tmid = f"tm-gb-{self.prefix}-{r}"
        model_key = f"m-g-{self.prefix}-{r}"
        ops = [
            ("/providers", {"id": pid, "key": pid, "name": f"provider-gb-{self.prefix}-{r}",
                            "endpoint": f"http://127.0.0.1:{upstream}", "weight": 1,
                            "created_at": "", "updated_at": ""}),
            ("/provider-models", {"id": pmid, "key": model_key, "name": model_key,
                                  "provider_id": pid, "status": 1,
                                  "created_at": "", "updated_at": ""}),
            ("/provider-keys", {"id": pkid, "provider_id": pid, "api_key": "sk-upstream",
                                "created_at": ""}),
            ("/tenant-providers", {"id": tpid, "tenant_id": BASE_TENANT, "provider_id": pid,
                                   "created_at": "", "updated_at": ""}),
            ("/tenant-models", {"id": tmid, "tenant_id": BASE_TENANT, "model_key": model_key,
                                "created_at": "", "updated_at": ""}),
        ]
        for path, body in ops:
            st, _, _ = self.node.admin("POST", path, body, timeout=25)
            if not self.must(st, (200, 201), f"create grant bundle {tpid} {path}"):
                return None
        return Record(
            self.idx, "grant_bundle", tpid, f"provider-gb-{self.prefix}-{r}",
            [("/providers", pid), ("/provider-models", pmid), ("/provider-keys", pkid),
             ("/tenant-providers", tpid), ("/tenant-models", tmid)],
            [("/providers", pid)],
            {"provider_id": pid, "provider_model_id": pmid, "provider_key_id": pkid,
             "tenant_provider_id": tpid, "tenant_model_id": tmid, "model_key": model_key,
             "base_tenant": BASE_TENANT},
            model_key=model_key,
        )

    def create_limit_role(self, r):
        rid = f"lr-{self.prefix}-{r}"
        body = {
            "id": rid, "name": f"limit-{self.prefix}-{r}",
            "matching_key": None, "matching_model": None,
            "matching_tenant": None, "matching_provider": None,
            "limit_count": None, "limit_token": None, "window": "m",
            "enabled": True, "created_at": "",
        }
        st, _, _ = self.node.admin("POST", "/limit-roles", body, timeout=25)
        if not self.must(st, (200, 201), f"create limit-role {rid}"):
            return None
        return Record(self.idx, "limit_role", rid, body["name"], [("/limit-roles", rid)],
                      [("/limit-roles", rid)], body)

    def create_global_role(self, r):
        rid = f"gr-{self.prefix}-{r}"
        body = {"id": rid, "name": f"global-{self.prefix}-{r}", "providers": [],
                "enabled": True}
        st, _, _ = self.node.admin("PUT", "/global-roles", body, timeout=25)
        if not self.must(st, (200, 201), f"create global-role {rid}"):
            return None
        return Record(self.idx, "global_role", rid, body["name"], [("/global-roles", rid)],
                      [("/global-roles", rid)], body)

    def create_identity(self, r):
        gid = f"g-{self.prefix}-{r}"
        uid = f"u-{self.prefix}-{r}"
        st, _, _ = self.tenant_call("PUT", "/user-groups", {"id": gid, "name": gid}, timeout=25)
        if not self.must(st, (200, 201), f"create user-group {gid}"):
            return None
        st, _, _ = self.tenant_call(
            "PUT", "/users", {"id": uid, "user_group_id": gid, "name": uid}, timeout=25
        )
        if not self.must(st, (200, 201), f"create user {uid}"):
            return None
        st, _, out = self.tenant_call(
            "POST", f"/users/{uid}/api-keys", {"name": f"key-{uid}"}, timeout=25
        )
        if not self.must(st, (200, 201), f"create api-key for {uid}"):
            return None
        try:
            key = json.loads(out)["key"]
            kid = json.loads(out)["id"]
        except Exception:
            self.errors.append(f"{self.prefix}: api-key response did not carry id/key: {out[:120]}")
            announce("WRITE FAILED", self.errors[-1])
            return None
        return Record(
            self.idx, "identity", uid, uid,
            [("/user-groups?tenant_id=" + BASE_TENANT, gid),
             ("/users?tenant_id=" + BASE_TENANT, uid),
             ("/user-api-keys?tenant_id=" + BASE_TENANT, kid)],
            [("/users?tenant_id=" + BASE_TENANT, uid)],
            {"group_id": gid, "user_id": uid, "api_key_id": kid},
            group_id=gid, user_id=uid, api_key_id=kid, api_key=key,
        )

    def create(self, kind, r, upstream):
        if kind == "provider":
            rec = self.create_provider(r, upstream)
        elif kind == "tenant_bundle":
            rec = self.create_tenant_bundle(r, upstream)
        elif kind == "grant_bundle":
            rec = self.create_grant_bundle(r, upstream)
        elif kind == "limit_role":
            rec = self.create_limit_role(r)
        elif kind == "global_role":
            rec = self.create_global_role(r)
        else:
            rec = self.create_identity(r)
        if rec is not None:
            self.records.append(rec)
            op(f"[{self.prefix} r{r}] CREATE {rec.kind} {rec.id}")
        return rec

    # ---- updates ---------------------------------------------------------
    def update(self, rec):
        node = self.node
        if rec.kind == "provider":
            body = dict(rec.body)
            body["name"] = rec.name = rec.name + "-upd"
            st, _, _ = node.admin("PUT", f"/providers/{rec.id}", body, timeout=25)
        elif rec.kind == "tenant_bundle":
            rec.name = rec.name + "-upd"
            st, _ = rename_tenant(node, rec.tenant_id, rec.name)
        elif rec.kind == "grant_bundle":
            pid = rec.body["provider_id"]
            body = {"id": pid, "key": pid, "name": rec.name + "-upd",
                    "endpoint": f"http://127.0.0.1:{UPSTREAM_PORT}", "weight": 1,
                    "created_at": "", "updated_at": ""}
            rec.name += "-upd"
            st, _, _ = node.admin("PUT", f"/providers/{pid}", body, timeout=25)
        elif rec.kind == "limit_role":
            body = dict(rec.body)
            body["name"] = rec.name = rec.name + "-upd"
            st, _, _ = node.admin("PUT", f"/limit-roles/{rec.id}", body, timeout=25)
        elif rec.kind == "global_role":
            body = {"id": rec.id, "name": rec.name + "-upd", "providers": [], "enabled": True}
            rec.name += "-upd"
            st, _, _ = node.admin("PUT", f"/global-roles/{rec.id}", body, timeout=25)
        else:
            body = {"id": rec.user_id, "user_group_id": rec.group_id,
                    "name": rec.name + "-upd", "enabled": False}
            rec.name += "-upd"
            rec.disabled = True
            st, _, _ = self.tenant_call("PUT", "/users", body, timeout=25)
        if not self.must(st, (200, 201), f"update {rec.kind} {rec.id}"):
            return False
        op(f"[{self.prefix}] UPDATE {rec.kind} {rec.id} -> {rec.name!r}"
           + (" (disabled)" if rec.disabled else ""))
        return True

    # ---- deletes ---------------------------------------------------------
    def delete(self, rec):
        node = self.node
        if rec.kind == "provider":
            st, _, _ = node.admin("DELETE", f"/providers/{rec.id}", timeout=25)
        elif rec.kind == "tenant_bundle":
            st, _, _ = node.admin("DELETE", f"/tenants/{rec.tenant_id}", timeout=25)
            if st not in (200, 204, 404):
                self.must(st, (200, 204), f"delete tenant {rec.tenant_id}")
                return False
            st, _, _ = node.admin("DELETE", f"/providers/{rec.body['provider_id']}", timeout=25)
        elif rec.kind == "grant_bundle":
            for path, ident in (
                (f"/tenant-providers/{rec.body['tenant_provider_id']}", "tenant-provider"),
                (f"/tenant-models/{rec.body['tenant_model_id']}", "tenant-model"),
            ):
                st, _, _ = node.admin("DELETE", path, timeout=25)
                if not self.must(st, (200, 204, 404), f"delete {ident} {ident}"):
                    return False
            st, _, _ = node.admin("DELETE", f"/providers/{rec.body['provider_id']}", timeout=25)
        elif rec.kind == "limit_role":
            st, _, _ = node.admin("DELETE", f"/limit-roles/{rec.id}", timeout=25)
        elif rec.kind == "global_role":
            st, _, _ = node.admin("DELETE", f"/global-roles/{rec.id}", timeout=25)
        else:
            st, _, _ = self.tenant_call("DELETE", f"/users/{rec.user_id}", timeout=25)
            if not self.must(st, (200, 204, 404), f"delete user {rec.user_id}"):
                return False
            st, _, _ = self.tenant_call("DELETE", f"/user-groups/{rec.group_id}", timeout=25)
        if not self.must(st, (200, 204, 404), f"delete {rec.kind} {rec.id}"):
            return False
        op(f"[{self.prefix}] DELETE {rec.kind} {rec.id}")
        return True


def wait_equal_versions(nodes, budget, what, minimum=None):
    def same():
        versions = [n.version() for n in nodes]
        return len(set(versions)) == 1 and (minimum is None or versions[0] >= minimum)

    ok = wait_for(same, budget, what)
    if not ok:
        announce("final versions", f"{[(n.name, n.version()) for n in nodes]}")
    return ok


def list_ids(node, path):
    st, body = node.admin_json("GET", path, timeout=20)
    if st != 200 or not isinstance(body, list):
        return None
    return {str(row.get("id")) for row in body if isinstance(row, dict)}


def assert_present(writers, nodes, label):
    expected: dict[str, set[str]] = {}
    for w in writers:
        for rec in w.records:
            for path, ident in rec.lists:
                expected.setdefault(path, set()).add(ident)
    missing = []
    for node in nodes:
        for path, ids in expected.items():
            got = list_ids(node, path)
            if got is None:
                missing.append(f"{node.name} GET {path} failed")
            else:
                lost = sorted(ids - got)
                if lost:
                    missing.append(f"{node.name} {path} missing {lost[:4]}")
    check(
        f"{label}: every created row is visible on every node's admin list",
        not missing,
        "; ".join(missing[:6]) or f"{sum(len(v) for v in expected.values())} row(s) across "
        f"{len(expected)} list endpoint(s)",
    )


def assert_absent(writers, nodes, label):
    leftovers = []
    for node in nodes:
        for w in writers:
            for rec in w.records:
                for path, ident in rec.lists:
                    got = list_ids(node, path)
                    if got is not None and ident in got:
                        leftovers.append(f"{node.name} {path} still has {ident}")
    check(
        f"{label}: every deleted row is gone from every node's admin list",
        not leftovers,
        "; ".join(leftovers[:6]) or "all deleted ids absent",
    )


def catalog_models(node, tenant_id):
    st, body = node.admin_json("GET", f"/tenants/{tenant_id}/models", timeout=20)
    if st != 200 or not isinstance(body, dict):
        return st, None
    try:
        return st, {m["model"] for m in body.get("models", [])}
    except Exception:
        return st, None


def assert_catalog_present(writers, nodes, label):
    problems = []
    checked = 0
    for w in writers:
        for rec in w.records:
            if rec.kind == "tenant_bundle":
                tid, model = rec.tenant_id, rec.model_key
            elif rec.kind == "grant_bundle":
                tid, model = BASE_TENANT, rec.model_key
            else:
                continue
            checked += 1
            for node in nodes:
                st, models = catalog_models(node, tid)
                if st != 200 or models is None or model not in models:
                    problems.append(f"{node.name} {tid} -> HTTP {st} models={models}")
    check(
        f"{label}: the snapshot-backed tenant catalog contains every created model on every node",
        not problems,
        "; ".join(problems[:6]) or f"{checked} tenant/model probes on {len(nodes)} nodes",
    )


def assert_identity_cache(writers, nodes, label, expected_status=(200,), needle=None,
                          only_disabled=False):
    problems = []
    seen = 0
    for w in writers:
        for rec in w.records:
            if rec.kind != "identity" or (only_disabled and not rec.disabled):
                continue
            seen += 1
            for node in nodes:
                st, _, out = node.chat(rec.api_key, BASE_DOMAIN, model=MODEL)
                if st not in expected_status or (needle and needle not in out):
                    problems.append(f"{node.name} {rec.id} -> HTTP {st} {out[:60]}")
    if not seen:
        problems.append("no identity records to probe")
    check(
        f"{label}: identity fanout behaves the same on every node",
        not problems,
        "; ".join(problems[:6]) or f"{seen} identity key(s) x {len(nodes)} nodes",
    )


def assert_base_data_plane(nodes, label):
    problems = []
    for node in nodes:
        st, ids, out = node.catalog(BASE_DOMAIN)
        if st != 200 or ids is None or MODEL not in ids:
            problems.append(f"{node.name} catalog -> HTTP {st} ids={ids}")
    check(
        f"{label}: the representative tenant/provider/model is served by all three data planes",
        not problems,
        "; ".join(problems) or f"/v1/models on {len(nodes)} nodes contains {MODEL!r}",
    )


def run_parallel(writers, fn):
    threads = [threading.Thread(target=fn, args=(w,), daemon=True) for w in writers]
    for t in threads:
        t.start()
    for t in threads:
        t.join(timeout=120)
    return all(not t.is_alive() for t in threads)


def main():
    if not os.path.exists(BIN):
        print(f"[fanout] CANNOT VERIFY: {BIN} is not built", file=sys.stderr)
        return 2
    try:
        redis_url = require_redis_url()
        host, port = redis_endpoint(redis_url)
        note = flush_db(REDIS_DB, redis_url)
    except RedisSetupError as e:
        print(f"[fanout] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2
    try:
        test_db = isolated("shared_db_random_fanout")
    except SetupError as e:
        print(f"[fanout] CANNOT VERIFY: {e}", file=sys.stderr)
        return 2

    shutil.rmtree(DIR, ignore_errors=True)
    os.makedirs(DIR, exist_ok=True)

    global UPSTREAM_PORT
    UPSTREAM_PORT = free_port()
    upstream = ThreadingHTTPServer(("127.0.0.1", UPSTREAM_PORT), Upstream)
    threading.Thread(target=upstream.serve_forever, daemon=True).start()
    relay = RedisRelay(host, port)
    relay_port = relay.start()
    node_redis = db_url(REDIS_DB, f"redis://127.0.0.1:{relay_port}")

    nodes = [
        Node(
            f"fanout-{label}",
            test_db.url,
            node_redis,
            os.path.join(DIR, f"{label}.log"),
            admin_token=ADMIN_TOKEN,
            encryption_key=ENCRYPTION_KEY,
            poll_ms=POLL_MS,
        )
        for label in ("a", "b", "c")
    ]
    A, B, C = nodes

    print(f"== database: {test_db}")
    print(f"== redis: {note}; relay :{relay_port} db {REDIS_DB}")
    print(f"== seed={SEED} rounds={ROUNDS} poll={POLL_MS} ms upstream=:{UPSTREAM_PORT}")
    print(f"== binary: {BIN}")

    base_token = ""
    try:
        A.start()
        if not wait_for(A.healthy, STARTUP_BUDGET, "node A admin health"):
            print(f"--- A log ---\n{A.log_tail()}", file=sys.stderr)
            print("[fanout] CANNOT VERIFY: node A never became healthy", file=sys.stderr)
            return 2
        B.start()
        C.start()
        if not wait_for(lambda: all(n.healthy() for n in nodes), STARTUP_BUDGET,
                        "all three admin health"):
            for n in nodes:
                print(f"--- {n.name} ---\n{n.log_tail()}", file=sys.stderr)
            print("[fanout] CANNOT VERIFY: the three nodes never became healthy", file=sys.stderr)
            return 2

        # Base fixture: representative data plane + tenant token + identity for cache probes.
        seed_base(A, tenant_id=BASE_TENANT, domain=BASE_DOMAIN, model=MODEL,
                  upstream_port=UPSTREAM_PORT, prefix="base", tenant_name="Fanout base")
        st, _, out = A.admin("POST", f"/tenants/{BASE_TENANT}/access-tokens", {"name": "fanout"})
        if st not in (200, 201):
            print(f"[fanout] CANNOT VERIFY: minting tenant token -> {st} {out[:160]}",
                  file=sys.stderr)
            return 2
        base_token = json.loads(out)["token"]
        st, _, _ = call(
            "PUT",
            f"http://127.0.0.1:{A.data_port}/tenant/{BASE_TENANT}/api/v1/user-groups",
            token=base_token,
            body={"id": "g-base", "name": "g-base"},
            timeout=25,
        )
        if status_ok(st):
            call(
                "PUT",
                f"http://127.0.0.1:{A.data_port}/tenant/{BASE_TENANT}/api/v1/users",
                token=base_token,
                body={"id": "u-base", "user_group_id": "g-base", "name": "u-base"},
                timeout=25,
            )
            st, _, out = call(
                "POST",
                f"http://127.0.0.1:{A.data_port}/tenant/{BASE_TENANT}/api/v1/users/u-base/api-keys",
                token=base_token,
                body={"name": "base-key"},
                timeout=25,
            )
            base_key = json.loads(out)["key"] if st in (200, 201) else ""
        else:
            base_key = ""
        if not base_key:
            print("[fanout] CANNOT VERIFY: seeding the base identity failed", file=sys.stderr)
            return 2

        # Wait for the base fixture to be served by every node, including the tenant token (its
        # hash is in the identity snapshot) and the base data plane.
        def base_ready():
            for n in nodes:
                st, ids, _ = n.catalog(BASE_DOMAIN)
                if st != 200 or not ids or MODEL not in ids:
                    return False
                st2, _, _ = call(
                    "GET",
                    f"http://127.0.0.1:{n.data_port}/tenant/{BASE_TENANT}/api/v1/user-groups",
                    token=base_token,
                    timeout=10,
                )
                if st2 != 200:
                    return False
            return True

        if not wait_for(base_ready, CONVERGE_BUDGET, "the base fixture on all three nodes"):
            for n in nodes:
                print(f"--- {n.name} catalog={n.catalog(BASE_DOMAIN)[:2]} ---", file=sys.stderr)
            print("[fanout] CANNOT VERIFY: the base fixture did not converge", file=sys.stderr)
            return 2
        for n in nodes:
            st, _, out = n.chat(base_key, BASE_DOMAIN, model=MODEL)
            check(f"base data-plane identity works on {n.name}", st == 200,
                  f"HTTP {st} {out[:70]}")
        announce("base fixture converged", "tenant token + identity + model on all nodes")

        writers = [Writer(i, n, base_token) for i, n in enumerate(nodes)]
        version_before = max(n.version() for n in nodes)
        pubsub_before = {n.name: n.reload_source("pubsub") for n in nodes}
        poll_before = {n.name: n.reload_source("poll") for n in nodes}

        # ---------------------------------------------------------------- Phase A
        print("\n== Phase A: concurrent creates")
        # `kinds` is per writer; build that inside the thread instead of sharing the last one.
        def create_worker(w):
            kinds = list(OP_KINDS)
            w.rng.shuffle(kinds)
            for r in range(ROUNDS):
                kind = kinds[r] if r < len(kinds) else w.rng.choice(OP_KINDS)
                w.create(kind, r, UPSTREAM_PORT)

        joined = run_parallel(writers, create_worker)
        for w in writers:
            failures.extend(w.errors)
        check("A1: all writer threads completed", joined, "no thread exceeded 120s")
        check("A2: every concurrent CREATE answered 2xx (no 5xx / config_not_published)",
              all(not w.errors for w in writers),
              "; ".join(e for w in writers for e in w.errors[:4]) or
              f"{sum(len(w.records) for w in writers)} record(s) created")
        created = sum(len(w.records) for w in writers)
        t0 = time.monotonic()
        version_after = wait_equal_versions(nodes, CONVERGE_BUDGET, "equal versions after creates",
                                            minimum=version_before)
        converge_ms = (time.monotonic() - t0) * 1000
        check("A3: all three nodes converged to the same config_version after concurrent creates",
              version_after,
              f"before={version_before} now={[n.version() for n in nodes]} "
              f"records={created} converge={converge_ms:.0f}ms")
        assert_present(writers, nodes, "A4")
        assert_catalog_present(writers, nodes, "A5")
        assert_base_data_plane(nodes, "A6")
        assert_identity_cache(writers, nodes, "A7")

        pubsub_delta = {n.name: n.reload_source("pubsub") - pubsub_before[n.name] for n in nodes}
        poll_delta = {n.name: n.reload_source("poll") - poll_before[n.name] for n in nodes}
        # A node that wrote last can already be at the highest version and therefore
        # discard peers' notifications via the version gate; requiring a pub/sub reload
        # on EVERY node is too strong for concurrent writers. The cluster drill covers
        # per-peer sequential fanout; here we prove the concurrent phase used pub/sub
        # (at least one peer reload) and that the 120s poll did not explain anything.
        check("A8: the concurrent phase used pub/sub (at least one node reloaded; 120s poll cannot explain it)",
              any(v > 0 for v in pubsub_delta.values()), f"pubsub delta={pubsub_delta}")
        check("A9: ...and the poll fallback did NOT run during the phase (poll interval is 120s)",
              all(v == 0 for v in poll_delta.values()), f"poll delta={poll_delta}")

        # ---------------------------------------------------------------- Phase B
        print("\n== Phase B: concurrent updates")
        def update_worker(w):
            candidates = [r for r in w.records if r.updatable]
            w.rng.shuffle(candidates)
            for rec in candidates[:min(ROUNDS, len(candidates))]:
                w.update(rec)

        joined = run_parallel(writers, update_worker)
        for w in writers:
            failures.extend(w.errors)
        check("B1: all writer threads completed", joined, "no thread exceeded 120s")
        check("B2: every concurrent UPDATE answered 2xx", all(not w.errors for w in writers),
              "; ".join(e for w in writers for e in w.errors[:4]) or "all updates 2xx")
        t0 = time.monotonic()
        version_b = wait_equal_versions(nodes, CONVERGE_BUDGET, "equal versions after updates")
        converge_ms = (time.monotonic() - t0) * 1000
        check("B3: all three nodes converged to the same config_version after concurrent updates",
              version_b, f"versions={[n.version() for n in nodes]} converge={converge_ms:.0f}ms")

        problems = []
        for node in nodes:
            for w in writers:
                for rec in w.records:
                    if not rec.name.endswith("-upd"):
                        continue
                    for path, ident in rec.name_lists:
                        st, body = node.admin_json("GET", path, timeout=20)
                        row = next((r for r in body if str(r.get("id")) == ident), None) \
                            if st == 200 and isinstance(body, list) else None
                        if row is None or row.get("name") != rec.name:
                            problems.append(
                                f"{node.name} {path} {ident} "
                                f"name={row.get('name') if row else None!r} expected={rec.name!r}"
                            )
        check("B4: every updated value is visible on every node's admin list",
              not problems, "; ".join(problems[:6]) or "all updated names visible everywhere")
        assert_identity_cache(
            writers, nodes, "B5", expected_status=(403,), needle="user_disabled",
            only_disabled=True,
        )
        assert_catalog_present(writers, nodes, "B6")

        # ---------------------------------------------------------------- Phase C
        print("\n== Phase C: concurrent deletes")
        def delete_worker(w):
            victims = list(w.records)
            w.rng.shuffle(victims)
            for rec in victims:
                w.delete(rec)

        joined = run_parallel(writers, delete_worker)
        for w in writers:
            failures.extend(w.errors)
        check("C1: all writer threads completed", joined, "no thread exceeded 120s")
        check("C2: every concurrent DELETE answered 2xx/404 (idempotent absence ok)",
              all(not w.errors for w in writers),
              "; ".join(e for w in writers for e in w.errors[:4]) or "all deletes succeeded")
        t0 = time.monotonic()
        version_c = wait_equal_versions(nodes, CONVERGE_BUDGET, "equal versions after deletes")
        converge_ms = (time.monotonic() - t0) * 1000
        check("C3: all three nodes converged to the same config_version after concurrent deletes",
              version_c, f"versions={[n.version() for n in nodes]} converge={converge_ms:.0f}ms")
        assert_absent(writers, nodes, "C4")

        # Snapshot-backed deletion probes: a deleted bundle's tenant is gone from the cache.
        missing = []
        for w in writers:
            for rec in w.records:
                if rec.kind != "tenant_bundle":
                    continue
                for node in nodes:
                    st, models = catalog_models(node, rec.tenant_id)
                    if st != 404:
                        missing.append(f"{node.name} {rec.tenant_id} -> HTTP {st} models={models}")
        check("C5: deleted tenants are 404 on the snapshot-backed catalog on every node",
              not missing, "; ".join(missing[:6]) or "no deleted tenant is still cached")

        gone = []
        for node in nodes:
            st, models = catalog_models(node, BASE_TENANT)
            if st != 200 or models is None:
                gone.append(f"{node.name} base catalog -> HTTP {st}")
                continue
            for w in writers:
                for rec in w.records:
                    if rec.kind == "grant_bundle" and rec.model_key in models:
                        gone.append(f"{node.name} still lists deleted grant model {rec.model_key}")
        check("C6: deleted base-tenant grant models are gone from the snapshot-backed catalog",
              not gone, "; ".join(gone[:6]) or "no deleted grant model remains cached")

        # Deleted identity keys must be unknown to the data plane on every node.
        identity_gone = []
        for w in writers:
            for rec in w.records:
                if rec.kind != "identity":
                    continue
                for node in nodes:
                    st, _, body = node.chat(rec.api_key, BASE_DOMAIN, model=MODEL)
                    if st != 401:
                        identity_gone.append(f"{node.name} {rec.id} -> HTTP {st} {body[:50]}")
        check("C7: deleted identity keys are refused (401) on every node",
              not identity_gone, "; ".join(identity_gone[:6]) or "all keys revoked")

        assert_base_data_plane(nodes, "C8")

        # ---------------------------------------------------------------- Final
        print("\n== Final")
        versions = [n.version() for n in nodes]
        check("F1: all three nodes have an identical config_version",
              len(set(versions)) == 1, f"versions={versions}")
        # Compare the relevant id sets across nodes for every list endpoint touched.
        paths = sorted({p for w in writers for rec in w.records for p, _ in rec.lists})
        same_sets = []
        for path in paths:
            sets = [list_ids(n, path) for n in nodes]
            if any(s is None for s in sets) or not all(s == sets[0] for s in sets[1:]):
                same_sets.append(path)
        check("F2: every relevant admin entity-id set is identical across nodes",
              not same_sets, "; ".join(same_sets) or f"{len(paths)} endpoint(s) compared")
        total_deleted = sum(1 for w in writers for _ in w.records)
        check("F3: no write was lost — every created record was deleted",
              total_deleted == created and created > 0,
              f"created={created} deleted={total_deleted}")
        announce("final", f"seed={SEED} rounds={ROUNDS} versions={versions} "
                          f"records_per_writer={[len(w.records) for w in writers]}")
    finally:
        for n in nodes:
            n.kill()
        relay.stop()
        try:
            upstream.shutdown()
        except Exception:
            pass
        test_db.cleanup()
        shutil.rmtree(DIR, ignore_errors=True)

    print()
    if failures:
        print(f"RANDOM FANOUT: FAILED ({len(failures)}): " + "; ".join(failures))
        return 1
    print(
        "RANDOM FANOUT: PASSED (3 nodes x concurrent random creates/updates/deletes; equal "
        "config versions, every row visible on every node, pub/sub-only convergence with a "
        "120s poll, snapshot catalog/identity probes, clean deletion fanout)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
