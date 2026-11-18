# 任务台账（backlog）

## 0. 这份文件是什么

计划与状态分离：**计划**在 `docs/Train0-Plan-6x1.md` 与 `control-center/docs/plan-control-plane-impl.md`，
**状态**在 `docs/STATUS.md`，本文件是二者之间的台账——只记录“某个计划项今天走到哪了、证据在哪”。

- 任务 ID 沿用计划编号（`T0.1–T4.2`、`E1–E10`、`S1-x`），不另造一套。
- 状态只允许五种：`已完成`（必须带证据）、`进行中`、`未开始`、`阻塞`、`待核实`。
- 本台账**不代替 Issue**；GitHub Issues 启用后按 `.github/ISSUE_TEMPLATE/` 建条目，并在此回链 issue 号。
- 事实以 2026-11-18 的仓库内容为准；shell 不可用，凡涉及“跑过没有/绿不绿”一律写 `待核实`。

## 1. 已完成（有仓库内证据）

| ID | 来源文件:行 | 一句话内容 | 状态 | 证据 | 备注 |
|---|---|---|---|---|---|
| T0.1–T0.3 | `control-center/docs/plan-control-plane-impl.md:59` | gRPC/protobuf 依赖、proto 副本、proto 同源门 | 已完成 | `control-center/scripts/proto-check.sh` 存在；`control-center/README.md:115-116` 9090 有 gRPC 服务端 | 门禁通过性待核实 |
| T1.1–T1.2 | 同上 :60 | 字节向量测试与 `ConfigDocument` validator | 已完成 | `control-center/app/src/test/java/dev/train0/cc/control/` 下 `ProtoVectorTest.java`、`ConfigDocumentValidatorTest.java` | 用例数与计划期望值是否一致待核实 |
| T2.1–T2.3 | 同上 :61 | control-006…010、结构校验、`ConfigPublisher` | 已完成 | `control-center/README.md:129-131`；`control-center/scripts/db-verify.sql`；`control-center/app/src/test/java/dev/train0/cc/control/ConfigPublisherTest.java` | 迁移在空库/既有库的差异见计划 §7 |
| T3.1–T3.5 | 同上 :62 | 装配与 TLS、两个拦截器、六 RPC、TLS 负向、上报 RPC | 已完成 | `control-center/README.md:126-129`；`control-center/app/src/test/java/dev/train0/cc/control/` 下 `AgentIdentityTest.java`、`MutualTlsTest.java`、`AgentControlHappyPathTest.java` | `Watch` 仍 `UNIMPLEMENTED`，是有意范围（`Control-Agent-API-v1.md:929`） |
| T4.1–T4.2 | 同上 :63 | compose/k8s 清单、smoke 9090 角色门、G1 P95 测试 | 已完成 | `control-center/scripts/smoke.sh:96-106`；`control-center/app/src/test/java/dev/train0/cc/control/PollingLatencyTest.java:96-99`（打印 `g1_p95_ms`） | 实测 P95 数值与 CI 去 skip 待核实 |
| E8（Rust 半） | `docs/Control-Agent-API-v1.md:860-870` | agent 客户端、配置校验、状态机、演示脚本 | 已完成 | `verification/REPORT-control-plane.md`、`verification/run_control_demo.sh` | 控制面半未接线，见 §2 第 1 行 |
| S1–S4（测试迁移） | `verification/TEST-PLAN.md:382-388` | L0–L3 分层、`cargo xtask`、Rust e2e | 已完成 | `verification/REPORT-layered-tests.md`、`REPORT-parser.md`、`REPORT-e2e.md` | S5 未开工（同一段原文） |
| E5（审计底座） | `control-center/README.md:103-107` | 审计只追加、双库物理隔离 | 已完成 | `control-center/scripts/audit-check.sh` 断言 UPDATE/DELETE/TRUNCATE 三条 | E6 拒绝审计的底座 |
| 文档基线 P0/P1 | 本分支 `docs/process-baseline`（基于 `97ea0d8`） | STATUS、PROCESS、backlog、ADR 0001–0011、runbooks、PR/ISSUE 模板、CHANGELOG、文档单入口 | 已完成（待合并） | `docs/STATUS.md`、`docs/PROCESS.md`、`docs/backlog.md`、`docs/adr/`、`docs/runbooks/`、`.github/PULL_REQUEST_TEMPLATE.md`、`.github/ISSUE_TEMPLATE/`、`CHANGELOG.md` | 合并进 main 后才算生效 |

## 2. W3 候选（8–12 条，未经 Planning 不承诺）

| ID | 来源文件:行 | 一句话内容 | 状态 | 证据 | 备注 |
|---|---|---|---|---|---|
| E8 | `docs/Train0-Plan-6x1.md:59`；`control-center/README.md:157-163` | k3d agent 命名空间、`control-plane-tls` secret、netpol 接线 | 未开始 | 仅有 compose 下的 `dev-certs.sh` | 完成后“上报→Web 可见”才成立 |
| E6 | `docs/Train0-Plan-6x1.md:57`；`probe/README.md:541` | eBPF 规则数组匹配、优先级、默认策略 | 未开始 | `action` 恒 `pass`、`rule_id=0` | 规则形状已冻结，只差执行 |
| S1-3 | `docs/Train0-Plan-6x1.md:429` | 登录/会话/JWT、RBAC3、SoD | 未开始 | `control-center/README.md:120` | W2 计划项，已滑到 W3 |
| E1/E3/E5 | `control-center/README.md:133`；`docs/Train0-Plan-6x1.md:36` | 6 个负向用例自动化 | 未开始 | 仅有 `/api/machines` 一条形状断言 | 依赖 S1-3 |
| T4.2 复跑 | `control-center/docs/plan-control-plane-impl.md:340` | `smoke.sh` + `PollingLatencyTest` 实测并记录 P95 | 待核实 | 脚本与测试都在 | 需要 compose 环境 |
| E10 | `control-center/README.md:154-156` | Docker 环境 CI（四镜像 build + compose up + smoke） | 未开始 | CI 只做不装 Docker 的校验 | 建议先加 compose 档 |
| — | `probe/README.md:518` | fail-open 决策：接受并写明，还是排期改 fail-closed | 阻塞 | 数据面恒 `XDP_PASS` | 需 PO + 安全一起拍 |
| DoD-12.5 | `docs/Train0-Plan-6x1.md:695` | ADR 四份（主体模型、RBAC3、配置下发、聚合上报） | 已完成 | `docs/adr/0001`、`0003`、`0004`、`0005`（后两份状态为“提议”，待 D4/D5 拍板） | 可与实现并行补 |
| — | 仓库根（0 tag） | 首个 tag（CHANGELOG 已补） | 未开始 | `CHANGELOG.md` 已建；`git tag` 仍为空 | 建议 M4 验收后从 `v0.2-w2` 起，按 `docs/PROCESS.md` §5 |
| — | `.github/`（仅 3 个 workflow） | CODEOWNERS、main 分支保护 | 未开始 | PR/ISSUE 模板已补（本分支）；无 CODEOWNERS、无保护规则 | 按 `docs/runbooks/repo-hardening.md` 执行 |
| — | `probe/README.md:8,10,509,543` | 修订 5 处过时表述（见 `docs/README.md` §3） | 已完成 | 本分支已改 `README.md` 与契约 `:891` | 留档见 `docs/README.md` §3 |
| — | `docs/README.md` 开场 | 外层 `docs/` 清理与迁移收尾 | 已完成 | 外层 `docs/`、`verification/` 已于 2026-11-18 删除 | 备份 `.tmp/backup-20261118/`；记录见 `docs/runbooks/outer-workspace-cleanup.md` §9 |

## 3. 待拍板（D2–D10）

来源：`control-center/docs/plan-control-plane-impl.md` §11（`:364-378`）。不拍板即按“当前默认”执行。

| # | 问题 | 当前默认 | 若不定会影响什么 | 建议拍板时间 |
|---|---|---|---|---|
| D2 | `control.agent.group_id` 谁写 | 准入/签发证书时写入，控制面不读 mgmt 库 | Register 无组可回、GetConfig 只能空；越权读 mgmt 破坏两库分离 | W3 Planning 前（11-23） |
| D3 | Train 0 的发布入口 | 只留 `ConfigPublisher` + 测试 fixture，HTTP 发布 API 归 S1-3 | 要么现在开未鉴权发布接口（安全洞），要么 G1 无法按计划口径测 | W3 Planning 前 |
| D4 | report 拆三表、`key_hash` bytea(32) | 已按设计落地（`control-006…010`） | 退回 `payload jsonb` 将无法按 key 去重/游标查询 | W3 回顾确认即可 |
| D5 | `pending` agent 能否拉配置 | 可 Register/Heartbeat/GetCursor/ReportWindows；GetConfig 恒 `up_to_date` | “pending 拿到配置却不许执行”的中间态会让验收剧本对不上 | W3 Planning 前 |
| D6 | `machine_id` 冲突 | 保留 `agent.machine_conflict`，要求显式 `PROBE_AGENT_MACHINE_ID` | 静默覆盖会让 agent 在两台机间跳，游标错乱 | E8 接线前 |
| D7 | CI 是否跑真 PG 集成测试 | Train 0 不跑，用 `CC_IT_PG` 跳过 | DoD “CI 覆盖 integration”留缺口，需追加 0.5 人日 | M4 验收前 |
| D8 | 测试证书从哪来 | `scripts/dev-certs.sh`（容器内 openssl） | 不生成则 `MutualTlsTest` 永远 skip，mTLS 无门 | 已按默认执行，确认即可 |
| D9 | `Watch` 的错误码 | `UNIMPLEMENTED`，不给 `x-retry-after-ms` | 返回 `UNAVAILABLE` 会让 agent 重试风暴 | 已按默认执行，确认即可 |
| D10 | `proto-check.sh` 在宿主怎么跑 | 统一在容器/Git Bash 里跑 | 只写 `bash` 会在无 Git Bash 的机器上直接失败 | 已按默认执行，确认即可 |

契约 `docs/Control-Agent-API-v1.md` §15 的 Q1–Q7 已定案（`:919-929`），其中 Q1 写“本 md 留外层 `docs/`”已随迁移过时，按 `docs/STATUS.md` §1 的裁决处理。

## 4. 维护规则

- 任务从本表第 2 节移入第 1 节时，必须补“证据”列（提交/PR/文件行三选一以上）。
- 每周 Planning 后把第 2 节重排；未进入 Sprint 的候选不得写成承诺。
- 发现计划本身写错，改计划文档并在“来源文件:行”回链，不改本表结论。
