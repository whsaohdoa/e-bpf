# 文档入口（docs/）

`probe/docs/` 是本仓库**唯一**的文档入口。外层工作区 `D:\repos\rust\eBPF\docs\` 已废弃，
迁移与清理见 `docs/runbooks/outer-workspace-cleanup.md`（由并行任务落地）。新文档一律写进仓库，不再写到外层工作区。

## 1. 权威矩阵

每份文档只有一个职责；同一事实只允许有一个权威来源，其余位置只能引用或摘要。

| 文档 | 覆盖范围 | 唯一职责 | 更新触发 | 不得做的事 |
|---|---|---|---|---|
| `docs/STATUS.md` | 当前进度、阻塞、最近门禁 | 进度的唯一权威 | 每次合并到 main、每次 demo 后 24h 内 | 不写计划、不复制命令、不写设计理由 |
| `docs/backlog.md` | 任务台账、W3 候选、待拍板 | 计划与状态之间的台账 | 每周 Planning；状态变化当天 | 不代替 Issue、不改计划正文 |
| `docs/PROCESS.md` | 分支、评审、合并、发布纪律 | 协作流程的唯一来源 | 流程变更、回顾产出 | 不写状态、不写构建命令 |
| `docs/adr/` | 架构决策记录 | 决策的不可变记录 | 每出现一个新决策 | 不改旧 ADR；只能新增并用 supersede 关联 |
| `docs/Train0-Plan-6x1.md` | 六周计划、G1–G5、验收剧本、DoD | 计划与验收口径 | PO 批准的范围/日期变更 | 不写状态、不回填复选框、不贴提交号 |
| `docs/Control-Agent-API-v1.md` | agent ↔ 控制面线协议、配置文档、错误码 | 契约的唯一来源 | proto 字段/语义变更 | 不与 `agent/proto/control.proto` 双向漂移 |
| `CONTRIBUTING.md` | 构建、容器、测试分层、门禁、e2e | 命令的唯一来源 | 命令、门禁、环境变更 | 不写架构与状态 |
| `AGENTS.md` | 硬性约束、禁区、文档索引 | agent/新人的入口与红线 | 约束变化 | 不复制命令细节、不写 git 事实 |
| `README.md` | 外部用法、架构、ABI/map/CSV、产品级限制 | 产品面的唯一来源 | 行为、ABI、CSV 变更 | 不写内部计划与进度 |
| `control-center/README.md` | 控制面边界、部署、数据库、已有/还没有 | 控制面现状的权威（“不假装完成”） | 控制面行为变化 | 不写 Rust 侧状态、不写计划排期 |
| `control-center/docs/plan-control-plane-impl.md` | M0–M4 切片、T0.1–T4.2、勘误 | 控制面实施计划与验收门 | 任务口径、门禁命令变更 | 不加状态列、不写当前里程碑 |
| `verification/TEST-PLAN.md` | 测试分层、用例编号 U/C/P/E/S、覆盖率门 | 用例编号的唯一来源 | 用例增删、分层调整 | 不再写进度（进度归 STATUS） |
| `docs/runbooks/` | 环境清理、迁移、排障的操作手顺 | 可重复执行的操作步骤 | 环境或步骤变化 | 不写设计理由与决策过程 |

## 2. 我该改哪份文档

| 改什么 | 改哪里 | 必须同时做 |
|---|---|---|
| ABI / map 布局 / `SampleEvent` | `README.md` 的 ABI 表 | 布局单测 + 兼容政策 |
| CSV 列序或列数 | `README.md` + `verification/TEST-PLAN.md` | L1 `tests/csv_schema.rs`、L2 `pipeline.rs` 断言 |
| 里程碑/任务状态 | `docs/STATUS.md` | 同步 `docs/backlog.md` 的状态与证据列 |
| 计划、范围、排期 | `docs/Train0-Plan-6x1.md`、`control-center/docs/plan-control-plane-impl.md` | 在 `docs/backlog.md` 登记来源行 |
| 协作纪律（分支/评审/合并） | `docs/PROCESS.md` | `CONTRIBUTING.md` 如有命令变化一并改 |
| 架构决策 | 新增 `docs/adr/NNNN-*.md` | 在 `docs/STATUS.md` 或 backlog 回链 |
| 控制面契约 | `docs/Control-Agent-API-v1.md` | `agent/proto/control.proto` 同源校验 |

## 3. 近期漂移修正（2026-11-18 文档基线）

以下过时表述已在本分支改正，留档供 review 核对；新增漂移请追加到本表并同步 `docs/STATUS.md` §1。

| 位置 | 原表述 | 现口径 | 状态 |
|---|---|---|---|
| `README.md:8,10` | “业务接口未实现”“还没有 gRPC 服务端” | 六方法已实现、仅 `Watch` 为 `UNIMPLEMENTED`、E8 未接线 | 已修 |
| `README.md:8` | “Spring Boot 3.5” | `control-center/app/pom.xml:10`：4.1.2 | 已修 |
| `README.md:509` | “七个方法都没写” | 六个已实现；仅 `Watch` 为 `UNIMPLEMENTED` | 已修 |
| `README.md:543-545` | 写控制面还没有 gRPC 服务端 | 两端各自已实现，缺的是 E8 端到端接线 | 已修 |
| `docs/Control-Agent-API-v1.md:891` | “两个环境今天都没有进程监听 9090” | `control` 角色已在 9090 提供 `AgentControl`（mTLS） | 已修 |

## 4. 口径与所有权

- `docs/PROCESS.md`、`docs/adr/`、`docs/runbooks/` 由并行任务创建，本文件只约定其职责与路径。
- 状态冲突一律按 `docs/STATUS.md` §1 的口径仲裁裁决；本文件不重复裁决内容。
