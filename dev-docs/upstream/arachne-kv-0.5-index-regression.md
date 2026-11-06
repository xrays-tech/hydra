# Upstream bug report: `multi_put_with_index` can return a non-comparable / regressing index after leader churn

> 这份文件是给 `arachne-kv` 0.5 的 bug report（英文正文可直接粘贴）。
> 复现基于 Hydra 的 3 节点 compose、4000 users + 4000 roles + 16000 API keys、
> kill leader/follower + network partition + quorum loss 的验收。
> 0.5 的 API 契约见 `dev-docs/arachne-kv-0.5-integration-guide.md`。

## Summary

After a real leader/follower/partition drill, a node that was restarted reports successful
`multi_put_with_index` writes at indexes **lower than the head index already materialized on the
other nodes**. The write's head value is therefore not the current head; the key phase reports HTTP
200 but followers never see that tree until a later unrelated write commits a much higher index.

This violates the 0.5 contract that indexes are log indexes, comparable across keys and nodes, and that
a forwarded write returns the leader's index.

## Observed evidence

Cluster: 3 nodes, `arachne-kv 0.5.0`, fresh v4 data dirs, `hydra` with TOC sharding.

### Other nodes have head index 557

```text
B: materializer head read head=m:f862d703c69159da... head_index=557
C: materializer head read head=m:f862d703c69159da... head_index=557
```

### A reports successful writes at indexes 274..307

```text
A: control-plane tree committed head=m:f97079f1e1b2852b... final_index=274
A: control-plane tree committed head=m:f7e7a48792f0ed58... final_index=284
A: control-plane tree committed head=m:9aef43f07503cde7... final_index=293
A: control-plane tree committed head=m:fcf00ade81d292b2... final_index=307
```

Those writes are the 4000-user/4000-role/16000-key bulk's final manifest+head batches. The HTTP call
returns 200. B/C remain on `m:f862...` / `557`; user/group counts converge, key counts stay at 1.

A later post-cliff admin write produces:

```text
A/B/C: materializer head read head=m:cada26b1b6f51998... head_index=2323
```

and only then all nodes reach `k16001`.

### The same workload without destructive churn passes

A capacity-only run (no kill/partition/quorum loss) reaches all 16001 keys in ~2 seconds. The failure
requires the post-churn/restart path.

## Expected

Per the 0.5 integration guide:

- `multi_put_with_index` returns the log index of the entry that actually applies the write;
- indexes are comparable across keys and nodes;
- a follower-forwarded write returns the leader's index;
- a head write whose index is lower than an already-applied head must not be accepted as a newer head.

At minimum, a stale local write must return an error (`NotLeader`, `Timeout`, `Unsupported`, or a
dedicated stale-generation error), not `Ok(index=274)` when the cluster head is 557.

## Suspected area

This looks like one of:

1. after restart/leader churn, the node's local `Handle` is using a stale local leader/session and
   returns a local index instead of a leader-committed index;
2. `multi_put_with_index` returns an index from a local/session path that is not the raft log index;
3. a stale leader is allowed to apply locally without a quorum and reports success.

The `Index` can be a session cache value only if it is guaranteed to be the same global log index; in
this run it is not.

## Reproduction

Hydra's acceptance script (not upstream code):

```bash
export USE_BULK=1
export SCALE_GROUPS=200 SCALE_USERS=4000 SCALE_ROLES=4000 SCALE_KEYS=16000
python3 .acceptance/tenant_scale_stability.py
```

The destructive phase is:

- kill leader, restart, wait healthy
- kill follower, restart
- restart Redis
- stop/start ClickHouse
- disconnect/reconnect node-b
- stop two nodes (quorum loss), issue a write (expect 503), restart both
- wait for post-destructive convergence
- bulk create 16000 keys on node A
- wait for all three nodes to reach 16001

Capacity-only variant:

```bash
export USE_BULK=1 CAPACITY_ONLY=1
export SCALE_GROUPS=200 SCALE_USERS=4000 SCALE_ROLES=4000 SCALE_KEYS=16000
python3 .acceptance/tenant_scale_stability.py
```

Result: PASS (~2s convergence). The destructive variant reproduces the index regression.

## What we need

- A 0.5.x fix so `final_index` is always the committed log index and is monotonic with the head index.
- A stale leader must fail the write rather than commit/return a lower local index.
- A regression test: 3 nodes, kill/restart leader, write from the old/follower node, assert:
  - returned index > previously materialized head index;
  - all nodes eventually read the same new head;
  - no silent local-only head.
- If this is a known limitation, document the exact error/retry contract so Hydra can reject `Ok(index)`
  that does not match the current head.

## Hydra-side safety while waiting

Hydra can add a read-back guard after `multi_put_with_index`:

```text
final_index = multi_put_with_index(...)
(head, head_index) = get_stale_with_index(ctl_head)
if head != expected_head || head_index < final_index:
    return 503 config_not_published (do not report the write as visible)
```

That turns a silent stale-local success into an honest failure, but it is not a substitute for the
upstream index/leader fix.
