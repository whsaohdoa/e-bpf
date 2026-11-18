# ADR-0007 状态口径单一来源：docs/STATUS.md 权威

- 状态：已接受
- 日期：2026-11-18（回填；同日落地 STATUS.md 与仲裁表）
- 决策者：Train0 团队（PO 维护口径）
- 影响面：文档（STATUS/计划/README/契约）、评审流程、合并后动作
## 背景（为什么现在必须有决定）

2026-11-18 盘点发现同一件事有三套说法，且都能在仓库里找到"证据"：

1. gRPC 服务端是否实现：`README.md` 写"还没有 gRPC 服务端"，`control-center/README.md`
   写"六个方法已实现，仅 Watch UNIMPLEMENTED"。
2. 计划在哪：`control-center/README.md` 曾写"计划不在本仓库"，实际 `docs/Train0-Plan-6x1.md`
   就在仓库内且 717 行。
3. 同一份控制面 README 把 gRPC 服务端同时列进"已有"和"还没有"（粗细粒度不同，读者无法分辨）。

根因不是某个人写错，而是"计划、README、测试计划"都被当成了进度看板。只要没有唯一权威，
每次合并都会再产生一处矛盾，评审也无法判定谁错。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：`docs/STATUS.md` 是进度的唯一权威，计划只写计划，其它文档只能引用不能断言。

1. 进度、阻塞、最近门禁只写在 `docs/STATUS.md`；其它文档出现"已完成/还没有"时必须
   给出来源（STATUS 或 `control-center/README.md` §6）。
   检查点：新增"已完成"字样但没有出处，评审打回。
2. 计划文件只写计划：不勾复选框、不写进度、不贴提交号。
   检查点：`docs/Train0-Plan-6x1.md` 全文 0 个复选框。
3. 三条矛盾的裁决口径固定如下，后续同类冲突照此办理：
   - 能力是否实现：以 `control-center/README.md` §6（已有/还没有）为准；
     `README.md` 关于"没有 gRPC 服务端"的几处表述与契约 `:891` 曾是待修漂移（2026-11-18 文档基线已改正，留档见 `docs/README.md` §3），修正前的旧表述不得再引用。
   - 计划位置：仓库内 `docs/Train0-Plan-6x1.md` 是唯一版本，外层工作区副本废弃（ADR-0010）。
   - 同一能力不得同时出现在"已有"和"还没有"：以 §6 的六方法清单为准，
     粗粒度句必须限定为"仅服务端骨架，Watch UNIMPLEMENTED"。
   检查点：`docs/STATUS.md` §1 仲裁表逐行覆盖上述三类。
4. 更新触发：每次合并到 main、每次 demo 后 24h 内，由 PR 作者更新 STATUS，reviewer 检查。
   检查点：合并 PR 的 diff 含 `docs/STATUS.md`（纯文档/CI 变更除外）。
5. 新冲突先补进 STATUS §1 仲裁表，再改文案；不得只改一边。
   检查点：仲裁表行数只增不减（被推翻时保留并标注失效）。
## 替代方案与被否原因（≥1 条，写清代价）

- **把状态写进计划并每周更新**：已失败——计划 0 复选框，进度反而散落在
  `verification/TEST-PLAN.md` §9 的带日期段落里，两边都不是权威。
- **用 GitHub Project/Milestone 当唯一来源**：外部不可 grep、离线不可读，
  与本仓库"文档入库、可评审"的约定冲突（ADR-0010）。
- **各 README 各自维护现状**：就是当前的三处矛盾，成本已经发生，不能继续。
## 后果（正面/负面/需要配套的动作）

正面：任何"到底做没做"只需看一处；评审有客观依据；计划恢复"计划"性质。

负面：STATUS.md 成为合并热点，冲突概率上升；每次合并多一步文档动作，小团队容易漏。

配套动作：`README.md` 的 gRPC 表述与契约 `:891` 已在本分支改正（留档 `docs/README.md` §3）；
`control-center/README.md` §6 的"已有/还没有"表述需要收敛为不重叠的清单；
STATUS 的更新责任写进 `docs/PROCESS.md` 的合并清单。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `docs/STATUS.md:3`："本文件是当前进度的唯一权威"；计划只写计划。
- `docs/STATUS.md:4`：更新触发（合并 main、demo 后 24h）。
- `docs/STATUS.md:7-11`：三条矛盾的仲裁表（gRPC 服务端、E6/E8、计划能否当进度）。
- `docs/STATUS.md:39-42`：最急待拍板 3 条（D2/D3/D5）。
- `docs/README.md:12`：STATUS 的职责与"不写计划/不复制命令/不写设计理由"。
- `docs/README.md:15`：ADR 不可变、只能新增并 supersede。
- `docs/README.md:16`：计划不写状态、不回填复选框、不贴提交号。
- `docs/README.md` §3：近期漂移修正留档（README.md:8,10,509,543-544、契约 :891）。
- `control-center/README.md:113`：§6 的逐项状态以 STATUS 为唯一权威。
- `control-center/README.md:115-118`：已有的粗粒度表述（含 gRPC 服务端）。
- `control-center/README.md:128-131`：六方法已实现、Watch 未实现。
- `verification/TEST-PLAN.md:382-392`：进度以带日期的散段记录（不应再作为权威）。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0006（计划冻结与不写状态）、ADR-0010（文档单入口）、ADR-0011（门禁结论也归 STATUS）。
- 计划 §12.5 DoD；`docs/backlog.md` 的漂移修正条目。
