# Aegis Workspace Index

This index tracks files created under this project's `dev-docs/aegis/` workspace.
Entries are workspace records, not authoritative runtime decisions.

| Date | Kind | Path | Title |
| --- | --- | --- | --- |
| 2026-08-21 | plan | dev-docs/aegis/plans/2026-08-21-key-prefix-binding.md | api-key 前缀绑定路由闸门（Key Prefix Binding Gate） |
| 2026-08-26 | doc | dev-docs/cluster.md | 集群模式（Cluster Mode）：Redis 租约 HA + 快照控制面 + 共享限流/熔断/认证 L2（含 live 验收实测 §5.1） |
| 2026-08-26 | doc | dev-docs/design.md | 主设计文档 §20 集群模式章节（概述 + 与既有章节的索引） |
| 2026-08-26 | doc | dev-docs/ops.md | 运维手册 §13 集群模式运维（部署/演练/Redis 故障/已知限制） |
| 2026-08-26 | doc | dev-docs/deployment.md | 部署方案（单节点 / docker-compose 多节点 / K3s / K8s 多节点） |
| 2026-08-27 | plan | dev-docs/aegis/plans/2026-08-27-admin-ui-i18n.md | Admin-UI 多语言国际化（中/英/法/德 四语切换） |
| 2026-09-03 | doc | dev-docs/design-tenant-model-catalog.md | 租户模型目录接口（数据面本地聚合 GET /v1/models + 管理口只读聚合 GET /api/v1/tenants/{id}/models） |
| 2026-09-08 | plan | dev-docs/aegis/plans/2026-09-08-public-models-catalog.md | 数据面 GET /v1/models 公开目录（免认证读取）——修订租户模型目录鉴权边界（oracle v1 GATE: PASS） |
| 2026-09-09 | plan | dev-docs/aegis/plans/2026-09-09-auth-insufficient-balance-402.md | 认证拒绝原因透传：insufficient_balance ⇒ HTTP 402（Payment Required；拒绝语义 + 缓存策略 + admin 归类） |
| 2026-09-10 | plan | dev-docs/aegis/plans/2026-09-10-inbound-credential-forms.md | 入口 api-key 多形式解析（Bearer / 裸 Authorization / x-api-key / api-key / x-goog-api-key / query；优先级 + 冲突告警，零 schema 变更） |
| 2026-09-16 | doc | dev-docs/audit-2026-09-16-changelog-gap-analysis.md | changelog-2026-09-16 工作项 vs 当前代码交叉对比审核（27 个 commit 均不存在于本仓库；P0×4 / P1×6 / P2 缺口清单 + §7 待决策项现状） |
| 2026-09-16 | plan | dev-docs/aegis/plans/2026-09-16-changelog-gap-remediation.md | changelog 缺口对齐修复计划（P0+P1+P2；快照 wire v2 双向 fail-closed + 复制内容唯一所有者 + 注册表回收 + h2 authority + 会话持久化 + CI ui-e2e）—— **✅ 第 12 轮 oracle 复审 PASS（门禁通过）；累计 12 轮、阻塞项 45→0，自第 4 轮起无设计级缺陷。** 开发进度：**Phase 0（预重构，纯搬移）✅ + Phase A / Batch 1（T1+T6，同一提交单元）✅** —— G1（副本保留禁用行）、G3（provider_key 身份保真）、§7-7（token 哈希落地）三条 P0/P1 主线闭合，wire v2 双向 fail-closed 三条机械断言落地，`POST /reload` 幂等 + `?force=1`；6 个新增回归用例已按"改前失败"取证；门禁重跑：fmt/clippy 0 告警、`--features server` 26 套件 0 failed（库 96）、三特性 386 passed。**下一步：Batch 2（T2 注册表回收）** |
