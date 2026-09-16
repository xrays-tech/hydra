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
| 2026-09-16 | plan | dev-docs/aegis/plans/2026-09-16-changelog-gap-remediation.md | changelog 缺口对齐修复计划（P0+P1+P2）—— **✅ 第 12 轮 oracle 复审 PASS（门禁通过）；累计 12 轮、阻塞项 45→0。** **计划已全部执行完毕**：Phase 0 ✅ / Phase A（Batch 1–4：T1+T6、T2、T3、T4）✅ / Phase B（Batch 5：T5、T8、T7）✅ / Phase C（Batch 6 + 12：T9.1 首字节超时、T9.2 转发超时语义、T9.3 租户写事务化、T9.4 排空期、T9.5 非 leader 横幅；T9.6 记为显式非目标；T9.7 由 T1 交付）✅ / Phase D（Batch 7–16：T10.5 冷启动选主不变量、T10.1 端口带、T10.4 e2e clearsFK、T10.2 真实证书链、T10.3 进程级监听器、T10.6 文档收口）✅。**唯一显式未收口项**：§7-1 的另一半（熔断探针语义）——计划明令留作独立的产品决策，不得用默认值不变的开关伪装已修，其已知盲区已记入 `dev-docs/ops.md`。开发后对抗式 oracle 复审：Phase A 一轮（T1+T6/T3/T4 PASS，T2 的 2 个阻塞已修），Phase B–D 一轮（见计划开发日志的对应小节）。最近一次统一门禁：fmt clean、两种特性组合 clippy 0 告警、`hydra-core` 15 套件、`--features server` **30 套件 0 failed**、三特性 **439 passed**、脚本门禁全绿、Playwright **14 passed**（真实二进制 + 真实 Chromium）。 |
