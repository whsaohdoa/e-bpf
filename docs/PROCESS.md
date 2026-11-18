# 流程规范（PROCESS）

本文件规定从切分支到合并、发布、收尾的协同时序：什么时候做什么、谁做、拿什么当证据。
命令怎么跑看 `CONTRIBUTING.md`，禁区与不变量看 `AGENTS.md`，本文不复制命令细节。

## 1. 适用范围与原则

适用于 `probe/` 与 `control-center/` 的全部改动，含纯文档改动。

| 原则 | 含义 | 检查点 | 证据来源 |
|---|---|---|---|
| 小步合并 | 一个 PR 一件事，一个工作日内可评审完 | `additions ≤ 400` 且 `changedFiles ≤ 20`（§4） | PR 页 Files changed 计数 |
| 计划与状态分离 | 计划只写“要做什么”，状态只写 issue 与 `docs/STATUS.md` | 计划文档 diff 不含“已完成/进行中” | PR diff 的 plan 文件 |
| 单一文档入口 | 找文档只查 `docs/README.md` 索引 | 新增文档必须在索引加一行 | `docs/README.md` 的 diff |
| 可逆操作才自动化 | CI 只校验；合并、打 tag、发布由人执行 | `.github/workflows/` 无自动 merge/push 步骤 | workflow 文件 |
| 门禁可复现 | 本地一条命令能复现 CI 判定 | 本地跑过 §4 自检第 3 项 | PR 模板勾选 + CI `gate` |

## 2. 分支命名

| 前缀 | 用途 | 示例 |
|---|---|---|
| `feat/` | 新功能、新模块 | `feat/cc-m4-T4.1` |
| `fix/` | 修缺陷 | `fix/csv-header-race` |
| `docs/` | 文档、模板、索引 | `docs/process-baseline` |
| `chore/` | 构建、依赖、CI、脚本 | `chore/boot-4.1.2-upgrade` |
| `test/` | 测试分层、用例、夹具 | `test/test-plan-s4` |
| `exp/` | 一次性实验，产物不进 main | `exp/native-xdp` |

- 可选任务 ID 后缀，格式 `<前缀>/<模块>-<里程碑>-<任务>`，如 `feat/cc-m4-T4.1`；无任务 ID 时用短横线描述。
  检查点：`git branch --show-current` 匹配 `^(feat|fix|docs|chore|test|exp)/[A-Za-z0-9][A-Za-z0-9.-]*$`；证据：PR 的 head 分支名。
- 从 `origin/main` 切出：`git switch -c <branch> origin/main`，不基于别人的功能分支（会连带别人的 diff）。
  检查点：`git merge-base --is-ancestor origin/main HEAD` 退码 0；证据：PR 的 base 为 `main`。
- 生命周期 ≤3 天；到第 5 个自然日必须拆成两个 PR（按 §4 大小门切）。
  检查点：`git log --date=short --format=%ad origin/main..HEAD | tail -1` 与今天相差 ≤3 天；证据：PR 首个提交日期。
- 远端长期分支只保留 `main`；禁止 `develop`、`release/*` 这类长期集成分支。
  检查点：`git branch -r` 只列出 `origin/main` 与在途功能分支；证据：`git branch -r` 输出。

## 3. 提交规范

格式：`type(scope): 摘要`，摘要中文 ≤50 字；正文写“为什么做”和“验收门命令 + 期望输出”。

| 规则 | 检查点 | 证据来源 |
|---|---|---|
| type 只用白名单：`feat` `fix` `docs` `chore` `test` `refactor` `perf` `build` `ci` | `git log -1 --format=%s` 匹配 `^(feat\|fix\|docs\|chore\|test\|refactor\|perf\|build\|ci)(\([a-z0-9-]+\))?: .+` | 提交标题 |
| 正文含验收门（命令 + 期望输出），照 `control-center/docs/plan-control-plane-impl.md:91` 的写法 | `git log -1 --format=%b` 有可执行命令与期望串 | 提交正文 |
| 关联任务 ID（`S1-7`、`T3.1`）或计划条目，写成 `docs/Train0-Plan-6x1.md:466` | `git log -1 --format=%B` 含任务 ID 或 `文件:行` | 提交正文 |
| 一个提交一件事 | `git show --stat HEAD` 的文件落在同一主题 | `git show --stat` |
| 历史混用不追溯，新提交不放宽（`add:`、`doc:`、`Chore:`、PR 标题式一律打回） | 评审时扫 PR 的 Commits 列表 | PR Commits 页 |

## 4. PR 流程

何时开：本地自测通过（改动对应的最小验证集 + fmt/clippy）即可开，先 Draft 后 Ready；不要求全部功能完成。

大小门：`additions ≤ 400` 且 `changedFiles ≤ 20`。超出时在 PR 模板“拆分理由”写明为什么不能拆，评审者明确同意才可合并。

| 必填项 | 检查点 | 证据来源 |
|---|---|---|
| 摘要 1–3 行 | 模板首段非空、不含占位符 | PR 描述 |
| 关联 issue 或计划条目 | 有 issue 链接或 `文件:行` | PR 描述 + issue |
| 自检清单逐条勾选 | 7 个复选框全为 `[x]` | PR 描述 |
| 风险与回滚 | 写出失败模式与回滚动作 | PR 描述 |
| 文档影响 | 列出改动的文档名（STATUS/PROCESS/ADR/契约）或无 | PR diff |

评审降配（单/双人团队的现实折中，替代 GitHub approve）：

- 不要求 `reviewDecision = APPROVED`；允许无 approve 合并。
- 但必须有 ≥1 条实质评审：提问、指出风险、要求补测试或文档都算；`LGTM`、表情、纯点赞不算。
- 作者自检清单必须逐条勾，不允许整段复制；评审者至少抽查 3 项（构建命令、CHANGELOG、文档影响）。
- `gate` 绿是硬条件；gate 红时评审意见不生效，先修再评。
  检查点：PR conversation 至少有 1 条来自非作者的评论且不是无正文；`gate` 结论为 success；证据：PR 时间线 + Actions 的 `gate` job。

## 5. 合并与发布

| 动作 | 规则 | 检查点 | 证据来源 |
|---|---|---|---|
| 合并方式 | 只用 squash：`gh pr merge <N> --squash --delete-branch` | main 保持线性，一个 PR 一个提交 | `git log --oneline main` |
| 删分支 | 合并后删本地分支；squash 后 `git branch -d` 可能报未合并，用 `git branch -D` | `git branch` 不再列出该分支 | `git branch` |
| 打 tag | 每次 demo 在 main 上打 `v0.<周>-w<N>`，如 `v0.2-w2`：`git tag -a v0.2-w2 -m "W2 demo" && git push origin v0.2-w2` | `git tag -l 'v0.*'` 出现当周 tag | GitHub Tags / Releases |
| CHANGELOG | 合并 PR 时同步加 `Unreleased` 条目；打 tag 的 PR 把 Unreleased 落成版本段 | PR diff 含 `CHANGELOG.md` | PR Files changed |
| 状态更新 | “合并即更新 `docs/STATUS.md`”，最迟次日 | STATUS 出现该 PR 号与合并日期 | `docs/STATUS.md` |

## 6. Issue 流程

- 进入 W 计划前必须有一个 issue；没有 issue 时用 `docs/backlog.md` 条目 + 回链 issue，二者至少其一。
  检查点：issue 的 body 含计划条目链接，或 backlog 行含 issue 号；证据：issue 页 / `docs/backlog.md`。
- issue 必填字段：

| 字段 | 写法 | 检查点 |
|---|---|---|
| 目标 | 一句话结果，不写过程 | 首段 ≤2 行 |
| 验收门 | 命令 + 期望输出（同 §3 正文格式） | 至少 1 条可执行命令 |
| 依赖 | 前置 issue / 分支 / 环境 | 无则写“无” |
| 规模 | S（≤0.5 人日）/ M（≤2）/ L（>2，需拆） | 有字母 + 人日 |
| 计划条目 | `文件:行` | 可点开定位 |

- 状态流转：`Todo` → `In Progress`（开 PR 时）→ `Review`（PR 待评审）→ `Done`（合并且 gate 绿）；关闭由作者做，PO 复核。
- 状态只写在 issue 与 `docs/STATUS.md` 两处，计划文档不写状态。
  检查点：计划文档 diff 无状态词；issue 的 label/状态与 STATUS 一致；证据：issue 页 + STATUS diff。

## 7. ADR 流程

必须写 ADR 的四种情形，满足其一即写：

| 情形 | 例子 | 检查点 |
|---|---|---|
| 不可逆 | 数据迁移、ABI 布局、CSV 列序 | PR 涉及 `probe-common/src/lib.rs`、Liquibase changelog |
| 影响契约 | proto、HTTP 接口、CSV、ABI | PR 改 `agent/proto/control.proto` 或 `README.md` 契约表 |
| 影响多人协作 | 目录边界、构建方式、评审规则 | PR 改 `CONTRIBUTING.md`、`docs/PROCESS.md` |
| 推翻既有决策 | 换存储、换框架、放弃某层测试 | PR 描述引用旧决策 |

- 编号：`docs/adr/NNNN-<slug>.md`，四位递增，不复用、不跳号。
  检查点：`ls docs/adr` 中编号连续；证据：目录列表。
- 状态机：`提议` → `接受` / `废弃`；被新决策取代写 `被取代（by NNNN）`。状态写在标题下第一行 `- 状态：…`。
  检查点：状态值在白名单内且只有一个；证据：ADR 文件首屏。
- 模板与状态机以 `docs/adr/README.md` 为准（本文不复述）：至少含背景、决策（可判定条款）、备选方案与被否原因、后果（含回滚）、证据（`file:line`）、关联 issue/PR。引用外部事实（PR 数据、分支保护）时在证据里标注“仓库内不可复核”。
  检查点：六个小节标题齐全；证据：ADR 文件。
- 评审：≥1 条实质评审（同 §4），并在合并 PR 里链接 ADR 路径。
  检查点：ADR 的关联 PR 有评审评论；证据：PR 时间线。

## 8. DoD 收尾

- 每个 Sprint 最后一个工作日跑 DoD 清单（`docs/Train0-Plan-6x1.md` §12，`:665-698`），逐条判定。
- 结论与证据**只**回填 `docs/STATUS.md`（唯一状态看板），形式是一张 DoD 判定表：逐条引用计划 §12 的条款编号与原文，给出“通过/不通过/未判定”与证据链接。
- **计划文档不改写、不勾复选框**：`docs/Train0-Plan-6x1.md` 自 v1.0 起保持冻结，任何“进度/负责人/日期/进行中”都不得写进计划；DoD 判定属于状态，因此只进 STATUS。
  检查点：STATUS 有本周 DoD 判定表；`git diff -- docs/Train0-Plan-6x1.md` 为空；证据：两处 diff。
- 每次 demo 出三个数字，写进 STATUS：

| 数字 | 取法 | 检查点 |
|---|---|---|
| 合并 PR 数 | `gh pr list --state merged --search "merged:>=<周一起始日>" --json number` 计数 | STATUS 有整数 |
| 平均 PR 大小 | 同上，取 `additions + deletions` 求平均 | STATUS 有整数行数 |
| 红 CI 时长 | 当周失败 run 的 `updated_at - created_at` 之和 | STATUS 有分钟数 |

## 9. 反例与红线

| 现象（已发生） | 为什么 | 规则 | 证据 |
|---|---|---|---|
| 16 个本地分支“假未合并” | squash 后分支 tip 不在 main 祖先链，`git branch --merged main` 不列它们 | 判断是否已合并只看 PR 状态或 `git cherry main <branch>`；不据此重开 PR | 2026-11-18 分支清点 |
| 文档在库外漂移 | 计划/契约在工作区 `docs/` 与 `probe/docs/` 各一份 | 仓库内 `docs/` 为唯一正本，外层副本只读 | 两份 `Train0-Plan-6x1.md` |
| PR 体量失控 | 无大小门，功能攒着一起提 | §4 大小门 + 拆分理由必填 | #3 52 文件 +9029、#6 43 文件 +11165 |
| 状态三处矛盾 | issue、STATUS、计划各写一份 | §6 状态只写两处 | 计划文档状态词 |
| 计划 0 复选框 | DoD 判定没有落点，评审无从核对 | §8 判定表写进 `docs/STATUS.md`，计划保持零改写 | 计划 §12 自 v1.0 未变 |

红线（一条即打回）：直接 push `main`、绕过 `gate`、提交 `.env`/`target/`/CSV/`out/`、无 issue 开工、评审只有 `LGTM`。

## 10. 责任矩阵

| 动作 | 谁 | 何时 | 证据 |
|---|---|---|---|
| 切分支、写验收门 | 作者 | 开工前 | 分支名 + issue 验收门 |
| 开 PR、勾自检 | 作者 | 本地自测后 | PR 描述 |
| 实质评审 | 另一名成员（无第二人时由 PO 兼） | 开 PR 后 1 个工作日内 | PR 评论 |
| 合并、删分支 | 作者（gate 绿后） | 评审完成当天 | PR merged 状态 |
| 打 tag、更新 CHANGELOG 版本段 | PO/SM | 每次 demo | tag 列表 + CHANGELOG |
| DoD 判定与回填 | PO/SM | Sprint 最后一个工作日 | `docs/STATUS.md` 的 DoD 判定表 |
| ADR 归档 | 决策提出者 | 决策当次 PR | `docs/adr/NNNN-*.md` |

## 11. 与其它文档的关系

| 文档 | 管什么 | 不写什么 |
|---|---|---|
| `docs/PROCESS.md`（本文） | 协同时序、评审、发布、DoD | 具体构建命令 |
| `CONTRIBUTING.md` | 构建、测试分层、改动→必跑表（`:94-100`）、gate 语义（`:130-132`） | 评审与发布时序 |
| `AGENTS.md` | 禁区、不变量、省 token | 流程步骤 |
| `docs/README.md` | 文档索引 | 规范正文 |
| `docs/STATUS.md` | 唯一状态看板 | 计划与验收门 |
| `docs/backlog.md` | 未排期池 | 进行中状态 |

- 协同/流程类冲突以本文件为准，并在 `docs/adr/` 记一条 ADR 说明改了什么、为什么。
- 命令与门禁细节只认 `CONTRIBUTING.md`（CI 按它实现，见 `.github/workflows/ci.yml`），本文不复制。
