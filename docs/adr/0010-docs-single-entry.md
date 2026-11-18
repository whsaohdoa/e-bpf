# ADR-0010 文档单入口：probe/docs/ 入库并作为唯一入口

- 状态：已接受
- 日期：2026-11-18（回填；同日外层副本清理 runbook 落地）
- 决策者：Train0 团队
- 影响面：文档（计划/契约/ADR/STATUS）、引用路径、外层工作区清理
## 背景（为什么现在必须有决定）

历史上文档存在两处：外层工作区 `D:\repos\rust\eBPF\docs\`（不进 git）与仓库 `probe/docs/`。
两者已经漂移：外层 `verification/TEST-PLAN.md` 379 行，仓库内同文件 428 行，差 49 行；
外层几乎每个 `.md` 都有一份副本。读者按外层文档操作会漏掉仓库里新增的用例与结论。
同时 `control-center/README.md` 一度声明"计划不在本仓库"，`README.md` 一度让读者去看
"工作区 docs"，指针互指，没人能确认哪份最新。

文档不进 git 的代价已经发生：不可评审、不可回溯、无法在 PR 里看 diff。必须定单一入口。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：`probe/docs/` 是唯一文档入口，文档随代码入库并在 PR 中评审，外层副本废弃。

1. 新文档一律写入 `probe/docs/`（含子目录 `adr/`、`runbooks/`）；外层 `docs/` 不再新增文件。
   检查点：外层 `docs/` 无新增文件；仓库内能 grep 到该文档。
2. 文档引用只写仓库内相对路径（`docs/...`、`control-center/...`）；出现"工作区 docs"字样即修。
   检查点：`grep -rn "工作区 docs"` 为空。
3. 文档冲突以仓库内版本为准；外层副本只作历史残留，不得据其判定。
   检查点：外层与仓库同文件行数/内容不一致时，以仓库为准（TEST-PLAN：379 vs 428）。
4. 文档变更与代码同流程：走 PR、过 gate、至少 1 条实质评审（ADR-0009）。
   检查点：文档 PR 的 diff 可在仓库内查看。
5. 运行产物（CSV、日志、`out/`）仍写在外层，不入库；但它们不是文档，不构成第二入口。
   检查点：`.gitignore` 不含 `docs/` 之外的文档目录。
6. 外层副本的删除按 `docs/runbooks/outer-workspace-cleanup.md` 执行；删除前外层目录视为只读。
   检查点：runbook 状态由"未执行"变为"已执行"并有备份记录。
## 替代方案与被否原因（≥1 条，写清代价）

- **文档留外层、靠人工同步**：已经漂移（TEST-PLAN 差 49 行），且不可评审、不可回溯；
  每次同步都是一次手工 diff，成本持续发生。
- **双写（外层 + 仓库）**：漂移是必然结果，且读者无法判断哪份权威；与 ADR-0007 的
  单一权威原则冲突。
- **放外部 Wiki**：离线不可读、不能与代码同 PR 评审，也无法用 file:line 做证据。
## 后果（正面/负面/需要配套的动作）

正面：文档可评审、可回溯；`file:line` 证据成立；读者只有一个入口。

负面：仓库体积增大（文档本身很小，可接受）；外层脚本若引用外层文档路径会失效，
需要一次性排查；迁移期两处并存，容易误读。

配套动作：修正残留指针（`README.md:423`、`control-center/README.md:7` 已改为仓库内）；
执行外层清理 runbook（当前状态"未执行"）；`docs/README.md` 的权威矩阵需与本 ADR 保持一致。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `docs/README.md:3-4`：`probe/docs/` 是唯一入口，外层 `docs/` 已废弃。
- `docs/README.md:12-24`：各文档唯一职责矩阵（含 adr/、runbooks/）。
- `control-center/README.md:7`：计划指向仓库内 `docs/Train0-Plan-6x1.md`。
- `README.md:423`：契约与 proto 均指向仓库内路径。
- `docs/runbooks/outer-workspace-cleanup.md:3-5`：清理范围为外层 docs/ 与 verification/，状态未执行。
- `docs/runbooks/outer-workspace-cleanup.md:18`：外层 verification 含运行产物，未确认前不删。
- `AGENTS.md:96`：文档只保留单一来源，不在外层或其它目录复制。
- 外层 `verification/TEST-PLAN.md` 379 行 vs 仓库内 `verification/TEST-PLAN.md` 428 行（2026-11-18 核对）。
- 2026-11-18 盘点（lead 提供，仓库内不可复核）：外层 `verification/` 5 份 .md 副本中 4 份字节相同。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0007（状态单一来源）、ADR-0009（文档也要评审）、ADR-0006（计划不写状态）。
- `docs/README.md` 权威矩阵；`docs/runbooks/outer-workspace-cleanup.md`。
