# 门禁核对：前两路 oracle 复审的 23 条 findings，逐条验证修复是否**真的落在计划文本里**。
# 可重跑：python3 .acceptance/findings-disposition.py
#
# ⚠ 这个脚本核对的是**计划正文里的字符串**，不是代码。它只能证明"计划写到了"，
#   不能证明"代码实现了"——把两者混为一谈正是本项目历史上的缺陷类别。要证明代码，
#   用下面 `code_checks` 那种可执行断言（判定子进程退出码），或直接跑
#   `.acceptance/phase-b-gate.sh`。
#   因此：markers 里**不要写 `file:line`**（行号随提交漂移，会变成永远为真的死标记；
#   历史上 `main.rs:909`、`content.rs:28` 就是这样失效并被当成证据的）。用符号名/字面量。
import subprocess, sys, re
P='dev-docs/aegis/plans/2026-09-17-tenant-api.md'
D='dev-docs/design-tenant-api.md'
s=open(P,encoding='utf-8').read()

# (id, 严重度, 修复的文本标记（全部必须出现）, 简短说明)
CHECKS = [
 ("P0-1","P0",["一个都不接","诚实的路由骨架","TDD Route: strict"],"T4 不接任何端点（消除假 TDD）"),
 ("P0-2","P0",["步骤 0","tenant_cache.rs","8 条 → 3 条" if "8 条 → 3 条" in s else "3 passed / 0 failed"],"旧路由的测试消费者先摘除"),
 ("P1-1","P1",["E0502","owned"],"借用安全：先取 owned path"),
 ("P1-2","P1",['starts_with("/tenant/")',"前缀"],"前缀门控（否则令牌被送去 auth_url）"),
 ("P1-3","P1",["--lib","内联","never_answers"],"T3 回归网真相 + 内联模块同搬"),
 ("P1-4","P1",["query: &str","params: &[(String, String)]","connect_timeout"],"CH 原语正确签名"),
 ("P1-5","P1",["SingleNode","Unavailable","list_nodes","alive"],"屏障接口三处重定义"),
 ("P1-6","P1",["with_allow_ttl_max","56 个调用点"],"不改 AuthCache 构造签名"),
 ("P1-7","P1",["设计用例覆盖矩阵","44/44"],"孤儿用例全部补齐"),
 ("P1-8","P1",["指标交付矩阵","12/12"],"指标全部有归属任务"),
 ("P1-9","P1",["usage_query::select","SelectError","二进制目标"],"选择逻辑提成库函数"),
 ("P1-10","P1",["358 行","基线例外",'expect("the cert store is built'],"grep 门禁限域 + 记录例外（符号名，不用行号）"),
 ("P1-11","P1",["build_config","因断言失败而红","loader"],"T2 红灯落在 loader"),
 ("P1-12","P1",["3 个字段","tenant_api","node_id"],"AppState 3 字段 + T6 含 main.rs"),
 ("P2-1","P1",["cluster/content.rs","i18n.js"],"反熵 grep 限域 + i18n 文案（符号名，不用行号）"),
 ("P2-2","P2",['cfg(feature = "proxy")','cfg(feature = "usage-clickhouse")'],"三个新模块的特性门控"),
 ("P2-3","P2",["serde(skip, default)","reindex_tenants","snapshot.rs"],"ConfigData 上 wire 的 serde 决策"),
 ("P2-4","P2",["12 处测试","14 处"],"构造点计数 13 → 12"),
 ("P2-5","P2",["14 套件","13 个测试文件"],"core 套件计数"),
 ("P2-6","P2",["跳过 C15","T24"],"用例编号说明"),
 ("P2-7","P2",["apply_batch_then_ack","checked","T16 也属阻塞项"],"C6 确定性 + checked + T16"),
 ("P2-8","P2",[],"T5/T9 补 Verification（用下面的 bash 块检查代替）"),
 ("P2-9","P2",["路径部分","8081"],"admin UI 只能展示路径"),
]

# 真·代码断言（判定退出码，不看文本）。计划里被当成"门禁例外"的那处生产 expect
# 必须真的存在——否则"例外"就是空的，而 P1-10 的限域也就无从谈起。
# (id, 说明, argv)
CODE_CHECKS = [
 ("C-1","计划引用的基线例外（main.rs 的 cert_store expect）确实存在",
  ["grep","-q",'expect("the cert store is built',"crates/hydra-server/src/main.rs"]),
 ("C-2","限域 grep 的四个目标路径都存在（两个已由 ADR-0002 T1.3 搬家，与计划正文同步）",
  ["bash","-c","for p in crates/hydra-core/src/tenant_api.rs crates/hydra-server/src/tenant_api crates/hydra-server/src/usage/backends/clickhouse/mod.rs crates/hydra-server/src/usage/backends/clickhouse/transport.rs; do test -e \"$p\" || exit 1; done"]),
 ("C-3","数据面租户路由表里确有 auth/cache/invalidate（非管理面路径）",
  ["grep","-q","api/v1/auth/cache/invalidate","crates/hydra-core/src/tenant_api.rs"]),
]
fail=[]
for cid,sev,markers,desc in CHECKS:
    miss=[m for m in markers if m not in s]
    if miss: fail.append((cid,sev,desc,miss))
    print(f"{cid:6} {sev}  {'✅' if not miss else '❌ 缺: '+str(miss)}  {desc}")

# P2-8 用结构检查代替文本检查：每个任务都要有可执行命令块。
# 只看**计划正文**：`## 实施记录` 里的 `### T7 — …` 记录标题与任务标题同形，
# 但记录本来就不需要命令块（命令在正文里）。不限定范围会把每条记录都误判成"缺命令块"。
body=s.split('## 实施记录')[0]
parts=re.split(r'^### (T\d+) — ',body,flags=re.M)
nobash=[parts[i] for i in range(1,len(parts),2) if '```bash' not in parts[i+1]]
print(f"{'P2-8':6} P2  {'✅' if not nobash else '❌ 缺命令块: '+str(nobash)}  10/10 任务含精确命令")
if nobash: fail.append(("P2-8","P2","每任务含命令","缺 "+str(nobash)))

print()
print("--- 可执行断言（判定退出码，不看文本） ---")
for cid,desc,argv in CODE_CHECKS:
    rc=subprocess.run(argv,capture_output=True,text=True).returncode
    ok = rc==0
    if not ok: fail.append((cid,"CODE",desc,f"exit {rc}"))
    print(f"{cid:6} CODE {'✅' if ok else '❌ exit '+str(rc)}  {desc}")

total=len(CHECKS)+1+len(CODE_CHECKS)
print()
print(f"处置核对：{total-len(fail)}/{total} 通过")
if fail:
    print("仍待处置："); [print("  ",f) for f in fail]
    sys.exit(1)
print("门禁：计划文本标记 + 可执行断言全部通过 ✅")
print("注意：本脚本不检查代码实现是否等价于文本声明；那部分由 .acceptance/phase-b-gate.sh 覆盖。")
