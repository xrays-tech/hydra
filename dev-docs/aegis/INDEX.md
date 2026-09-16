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
| 2026-09-16 | plan | dev-docs/aegis/plans/2026-09-16-changelog-gap-remediation.md | changelog 缺口对齐修复计划（P0+P1+P2）—— **✅ 第 12 轮 oracle 复审 PASS（门禁通过）；累计 12 轮、阻塞项 45→0。** **计划已全部执行完毕**：Phase 0 ✅ / Phase A（Batch 1–4）✅ / Phase B（Batch 5：T5、T8、T7）✅ / Phase C（Batch 6+12：T9.1–T9.5；T9.6 显式非目标；T9.7 由 T1 交付）✅ / Phase D（Batch 7–16：T10.1–T10.6）✅。**开发后对抗式复审两轮，findings 全部处置**：第一轮（Phase A）T1+T6/T3/T4 PASS、T2 FAIL 的 2 个阻塞已修；第二轮（Phase B/C/D）T5 PASS、T7+T8/T9.x/T10.x 的 **4 个阻塞已修**（`forward` 的连接阶段误判为"结果未知"、`ops.md` 一条永不触发的告警、三处 `--features db` 配方不能编译、CI 缺失败诊断上传），另一路"逐条重算声明"审查者证伪的 3 条断言亦已更正，并修掉一个**使门禁不确定的端口竞态**（`tests/metrics.rs` 改为换新端口有界重试）。**唯一显式未收口项**：§7-1 的另一半（熔断探针语义）——计划明令留作独立产品决策，其已知盲区已记入 `dev-docs/ops.md`。最终门禁（均带 `RUSTFLAGS=-D warnings`）：fmt clean、两种特性组合 clippy **0 告警**、两种 release build、`hydra-core` 15 套件、`--features server` **连跑 3 次每次 367 passed / 0 failed**、三特性 **441 passed / 2 ignored / 0 failed**、三条脚本门禁、Playwright **15 passed**（真实二进制 + 真实 Chromium）。运维仓库侧仍有三项外部依赖（告警规则文件、k8s manifest 的 `terminationGracePeriodSeconds`、admin 面 HTTPS）。 |
