# ADR-0009 评审降配：≥1 条实质 review，不强制 GitHub approve

- 状态：已接受
- 日期：2026-11-18（回填；同日评审纪律写入 PROCESS.md）
- 决策者：Train0 团队
- 影响面：流程（评审/合并）、CI 门禁依赖、知识与风险分布
## 背景（为什么现在必须有决定）

团队 4–5 人，同一模块常常只有作者最熟。若强制 GitHub approve，现实结果是：
要么作者找一个人点"Approve"但没人真看，要么 PR 卡住等不到人。
另一方面，完全不评审又会让 `README.md` 与实现脱节这类问题无人拦截——本仓库已经出现过
"文档说没有 gRPC 服务端、代码已经实现"的漂移（ADR-0007）。
所以需要一个低成本、可判定的折中：保留"有人真看"的实质，放弃"必须点 Approve"的形式。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：每个 PR 至少 1 条来自非作者的实质评审，不要求 approve，gate 必须绿。

1. 每个 PR 至少 1 条来自非作者的评审评论，且必须指出具体行/风险/缺的验证；
   只有"LGTM"不算，视为未评审。
   检查点：PR 时间线有非作者、有正文、指向具体改动的评论。
2. 不要求 `reviewDecision = APPROVED`；允许 0 approve 合并。
   检查点：合并 PR 不因缺 approve 被阻塞。
3. `gate` 绿是硬条件；gate 红时评审意见不生效，先修再评。
   检查点：Actions 的 gate job 为 success（skipped 由 gate 翻译为绿）。
4. 作者自查清单必须在 PR 描述逐条勾选：构建/测试/文档影响/不变量（ABI、CSV、fail-closed 等）。
   检查点：PR 模板四项全部勾选。
5. 升级为强制 approve 的触发条件（满足任一条即新写 ADR 取代本文件）：
   活跃提交者 ≥3 人；或出现一次需要回滚 main 的事故；或同一类缺陷连续两次逃过评审。
   检查点：`docs/STATUS.md` 记录活跃提交者数与回滚事件。
## 替代方案与被否原因（≥1 条，写清代价）

- **强制 1 个 GitHub approve**：形式合规但容易变成橡皮图章；4–5 人团队里作者常是唯一
  熟悉该模块的人，reviewer 没有上下文，approve 不增加信息量，只增加等待。
- **双人 review**：人力不允许，且会让小改动排队；在六周 Train 里直接拖慢交付。
- **完全不评审、只靠 CI**：CI 只验证能测的东西（编译、分层测试、schema 静态校验），
  文档漂移、契约语义、安全边界都测不出来；本仓库的三处状态矛盾就是例子。
## 后果（正面/负面/需要配套的动作）

正面：合并快；评审聚焦在"具体行"而不是"点按钮"；gate 提供统一的客观底线。

负面：橡皮图章风险仍在（一条评论也可能敷衍）；知识集中没有缓解；
评审质量无法自动度量，只能靠事故复盘倒查。

配套动作：PR 模板必须包含自查清单与"文档影响"项；`docs/PROCESS.md` 的红线要执行
（评审只有 LGTM 即打回）；每次回滚事故必须在 STATUS 记录并复核本 ADR 的升级条件。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `docs/PROCESS.md:64-70`：评审降配的四条与检查点（不要求 approve、≥1 条实质评论、gate 硬条件）。
- `docs/PROCESS.md:143`：红线（绕过 gate、评审只有 LGTM 即打回）。
- `docs/PROCESS.md:117-118`：ADR 也要 ≥1 条实质评审。
- `CONTRIBUTING.md:128-132`：CI 分工；分支保护只需 gate 一个检查。
- `.github/workflows/ci.yml:204-211`：gate 恒执行（`if: always()`）。
- `.github/workflows/ci.yml:213-229`：gate 汇总并把 skipped 视为绿。
- `.github/workflows/control-center.yml:110-114`：control-center 的 gate 同语义。
- 2026-11-18 盘点（lead 提供，仓库内不可复核）：10 个 PR 的 `reviewDecision` 全为空、
  main 无分支保护。该数据不在仓库内，故本 ADR 的判定依据只用上述文件，不用盘点数字。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0008（合并策略）、ADR-0011（门禁分层）、ADR-0007（状态口径）。
- `docs/PROCESS.md` §4/§9；`CONTRIBUTING.md` §3。
