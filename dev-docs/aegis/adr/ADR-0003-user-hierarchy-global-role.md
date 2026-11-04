# ADR-0003 — 四级用户体系、内部 API-Key 鉴权、Global Role 资源调度

- **Status**：`accepted` **全部已实施（2026-11-03：T1–T9 + T11 全绿，974 passed / 0 failed；2026-11-04：T10 已收口** — 四独立提交 `5809d0c`(A: events 解耦) / `52fabb1`(B: auth cache 删除) / `46bf3f1`(C: sub-tenant / key-prefix binding 删除 + `0015`) / `efeec42`(D: `auth_url` / `access_token_hash` 删除) + 文档/guards 收尾（plan `dev-docs/aegis/plans/2026-11-04-t10-legacy-cleanup.md`）;旧路径下不再编译、不再服务、不再可调用).
- **Date**：`2026-11-03`
- **Deciders**：用户（四级体系 + 内部鉴权 + 欠费 + 多令牌 + Global Role 五维/通配/并集 + 会话粘性 + 规则收束到单界面）；编排智能体综合 exp-2/exp-3/exp-4 现状测绘。
- **Source Evidence**：用户需求 1–9（见 plan §0）；本仓直读 —— `migrations/0014_user_hierarchy.sql`（只 CREATE）、`crates/hydra-core/src/{model.rs,identity.rs,affinity.rs,router.rs,role.rs}`、`crates/hydra-server/src/{proxy.rs,tenant_api/handlers.rs,admin/handlers.rs,db.rs,store.rs,cluster/arachne_keys.rs}`。
- **Owner Surface**：`dev-docs/aegis/` 工作区（与 ADR-0001/0002 同）；**不是** `docs/aegis/`.
- **Boundary**：advisory 记录，不授予完成权，不取代 `dev-docs/design.md` / `ops.md` / `tenant-api-integration.md` / `cluster.md` 项目权威文档。
- **配套**：实施计划 `dev-docs/aegis/plans/2026-11-03-user-hierarchy-global-role.md`（§9 实施结果 + 偏差）;基线同步目标 = `cluster.md`（TOC 5 + 新实体）/ `ops.md`（session-affinity + 内部 auth + 硬限）/ README（en + zh）/ `tenant-api-integration.md`.

---

## 1. 背景（Context）

| # | 事实（本次直读） | 证据 |
|---|---|---|
| C1 | 客户端 key 不落库:鉴权 = client api-key → `ConfigData.users_by_api_key_hash`（全行,含 disabled/expired/delinquent,由 `reindex_identity` 从全行同源 leader/副本） | `identity.rs:20-165`;`store.rs:221`;`db.rs:2180` |
| C2 | 旧外部鉴权 = `proxy` 未命中 → `AuthCache`(L1+Redis L2) → POST `tenant.auth_url`;`HttpAuthChecker` 读 `auth_url`.本决策使 it **dead-but-present（T10 2026-11-04 deleted it outright**） | `proxy.rs` (pre-existing hop);`http.rs` `AuthChecker`;`redis/auth_cache.rs` (removed by T10 Stage B) |
| C3 | 旧 routing 层 = key-prefix `provider_key_binding` + `sub_tenant`/`sub_tenant_route`（前缀路由,operator-binding 优先,fail-closed 交集） | `router.rs` `match_key_binding`/`match_sub_tenant*` (live until T10 Stage C deleted them) |
| C4 | 无 user / group / api_key 表;无 `*` 通配;无会话头读取;`ModelProvider.weight` 死,真权重 = `Provider.weight` | pre-batch 现状 |
| C5 | 控制面 `TOC_FORMAT` was 4;新实体改变 key 布局/decode ⇒ 单调 bump required | `arachne_keys.rs:5` now `TOC_FORMAT = 5` |

一句话:鉴权与 routing 规则 today 分散在外部 auth URL + key-prefix binding + sub-tenant route,没有 user/group dimension,没有 session affinity — a green-field rebuild collapses them to one model (see §2),with the deletion of the old layers deferred at the time of writing (done by T10 on 2026-11-04).

## 2. 决策（Decision）

1. **四级用户体系**:`tenant → user_group → user → api_key`,三 identity tables (`user_group`/`user`/`user_api_key`) per-row authoritative;FK + write-time same-tenant invariants (handler-checked, not FK-only). `hk_`+32B CSPRNG client keys,SHA-256 stored,plaintext shown once (`proxy` 401/403/402). `ht_`+32B tenant access tokens,now **multi-token** per tenant (independent table,admin-minted,plaintext once).
2. **Internal auth = single path**:client api-key hash → `users_by_api_key_hash`;unknown/disabled/expired ⇒ 401 `invalid_api_key`;user/group/tenant disabled ⇒ 403;`billing_status=delinquent` ⇒ 402 `user_delinquent` (body `{"error":{"message":"user_delinquent","type":"insufficient_quota","code":"user_delinquent"}}`). **No** outbound HTTP, no auth cache, no `auth/cache/invalidate` round-trip — revoke/delinquency flips next request (SLO ≤5 s via config publish + 1 s materialise).
3. **Global Role 五维通配并集**:`global_role(tenant,user_group,user,api_key,model)` each dim `*`=any → `global_role_provider(role,provider)`;matched rules' providers **unioned** (dedup,`provider_id` ascending),always ∩ providers online-serving that model (`models_by_key`);**wildcard-tenant role ⇒ also ∩ `tenant_providers`** (fail-closed),specific-tenant role ⇒ explicit grant (no intersect). No match ⇒ today's `tenant_providers ∩ models_by_key`.
4. **Routing weighting**:`weight` = `Provider.weight`. No session header ⇒ SWRR (unchanged). With session header (`X-Opencode-Session` → `Session-Id` → `x-conversation-id`,first non-empty trim ≤256B) ⇒ **session-sticky weighted rendezvous** on `(tenant,model,api_key_id,session)`:pick **minimum** `-ln(u)/weight` (ties `provider_id` ascending) ⇒ `P(i) ∝ weight_i` (NOT argmax). `HYDRA_SESSION_AFFINITY` default on;total parse (bad value ⇒ default on, no startup refuse). Failover from sticky head = remaining candidates `provider_id` ascending (stateless, not `swrr::order`).
5. **Rule consolidation to one admin surface**:Global Roles page owns all scheduling;tenant self-service API (`/tenant/{tid}/api/v1/`) owns ONLY own group/user/key/billing (never provider config).

## 3. 决策记录（D-1…，被否方案）

| # | 决策 | 依据 / 被否 |
|---|---|---|
| D-A | identity index carries **disabled AND delinquent** rows (deliberate enabled-only exception),derived from full rows by `reindex_identity` on leader + replica | anti-enumeration: revoked key is "known→401" not "unknown"; user state is business fact. 被否:enabled-only (replica would 404 a still-valid-but-revoked key inconsistently) |
| D-B | wildcard literal `*` (not NULL),monotonic id space | readable in UI,id never collide. 被否:NULL (ambiguous "any" vs missing) |
| D-C | `tenant='*'` roles ∩ `tenant_providers`;specific-tenant roles do NOT | one wildcard rule must not silently re-point every tenant. 被否:never intersect (privilege amplification) |
| D-D | session-sticky weighted rendezvous (minimum `-ln(u)/weight`),stateless,failover tail `provider_id` asc | stable + `P(i) ∝ weight` + no memory growth. 被否:`hash%N` (remaps almost all on set change);stateful `swrr::order` |
| D-E | keys store SHA-256 + prefix/last4,plaintext once (D-7 in plan),`ht_`/`hk_` same shape | matches existing seal policy. 被否:operator-chosen token口令 (weak口令 + hash boundary mismatch) |
| D-F | `TOC_FORMAT` 4→5,per-row entities carry disabled rows (NOT duplicated into `_fidelity`),seal with `seal_deterministic` | avoid 1 MiB fidelity cliff. 被否:reset to 1 (variant deletion reorders discriminants → old tree silently misread) |
| D-G | `0014` only CREATE,`0015` + code deletion land SAME batch (T10, done 2026-11-04 in `46bf3f1` + `efeec42`);T1–T9 keep old code compiling | green-field but drop-before-code breaks loader/proxy mid-batch (BL-2) |

## 4. ~~遗留 (T10-deferred)~~ — 已收口（2026-11-04, T10 complete）

At the time of writing this was deferred (`0015` not applied, `tenant.auth_url` NOT NULL still write-required, `auth/cache/invalidate`, admin `auth/test`, `auth/cache`, and the whole sub-tenant / sub-tenant-route / provider-key-binding stack compiled & served as dead code, `whoami` still echoing `auth_url`). **All of it is now CLOSED by T10 (2026-11-04)**: migration `0015_drop_legacy.sql` drops `provider_key_binding` / `sub_tenant_route` / `sub_tenant` and the `access_token_hash` / `auth_url` columns; `HttpAuthChecker` / `AuthCache` / `redis/auth_cache.rs` are deleted; the admin `auth/cache` route and the tenant-plane `auth/cache/invalidate` answer `404`; `TOC_FORMAT=5` refuses old trees by `UnknownEntityKind` (fail-stop) — green-field wipe ⇒ no live exposure.
