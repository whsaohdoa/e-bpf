# ADR-0008 分支与合并策略：短分支 + squash + 合并删分支 + demo tag

- 状态：已接受
- 日期：2026-11-18（回填；同日分支清点后固化）
- 决策者：Train0 团队
- 影响面：流程（分支/合并/发布）、git 历史形态、发布可追溯性
## 背景（为什么现在必须有决定）

2026-11-18 清点本地仓库：13 个本地分支（另有 `.git/config` 里的在途分支）、6 个远端分支、
**0 个 tag**。其中多数分支的提交早已进入 main，但因为合并方式使分支 tip 不在 main 的
祖先链上，`git branch --merged main` 不列它们——看起来像"16 个分支都没合并"。
没有 tag 意味着"哪周交付了什么"无法回溯。这两件事都是合并策略的直接后果，
必须在下一个 PR 之前固定，否则分支与 tag 会继续按个人习惯漂移。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：从 origin/main 切短分支、只 squash 合并、合并即删分支、每次 demo 打周号 tag。

1. 分支从 `origin/main` 切出，命名匹配 `^(feat|fix|docs|chore|test|exp)/[A-Za-z0-9][A-Za-z0-9.-]*$`，
   且不基于别人的功能分支。
   检查点：`git branch --show-current` 与 PR head 分支名匹配该正则。
2. 合并只用 squash：`gh pr merge <N> --squash --delete-branch`；main 保持线性，
   不允许 merge commit，不允许直接 push main。
   检查点：`git log --oneline main` 无 merge 提交。
3. 合并后删分支：本地 `git branch -D`（squash 后 `-d` 可能误报未合并），远端随合并删除。
   检查点：`git branch` 与 `git branch -r` 不再列出该分支。
4. 是否已合并只看 PR 状态或 `git cherry main <branch>`，不得用 `git branch --merged` 判断，
   也不得因"分支还在"重开 PR。
   检查点：清点脚本使用 PR 状态/cherry。
5. 每次 demo 在 main 上打 annotated tag `v0.<周>-w<N>`（例 `v0.2-w2`），并 push；
   tag 指向该周最后一个 squash 提交。
   检查点：`git tag -l 'v0.*'` 出现当周 tag，且 `git describe --tags` 可定位。
6. 远端长期分支只保留 main，禁止 develop/release/* 长期分支。
   检查点：`git branch -r` 只列 main 与在途功能分支。
## 替代方案与被否原因（≥1 条，写清代价）

- **merge commit**：保留每个提交的作者与时间，但 main 历史分叉，回滚粒度变成"一次 merge"，
  且 `git log --oneline` 无法一眼读完；六周项目不需要保全部中间提交。
- **rebase merge**：历史线性且保留每个提交，但会改写作者提交、冲突逐个解决，
  PR 越大成本越高；多人并行时容易要求作者反复 rebase。
- **长期 release 分支**：多一套合并方向与 hotfix 规程，Train 0 只有一条交付线，
  成本不抵收益。
## 后果（正面/负面/需要配套的动作）

正面：main 线性、一个 PR 一个提交；回滚以 PR 为单位；tag 让"哪周交付"可追溯。

负面：squash 造成"假未合并"（`docs/PROCESS.md:137` 记录的 16 个本地分支就是证据），
必须靠 PR 状态判断；提交粒度的历史被压缩，排障时看不到 PR 内部演进。

配套动作：清理现有 13 个本地分支（`git branch -D`，先按 PR 状态确认已合并）；
补 tag 从 `v0.2-w2` 起（W1/W2 已过，需在 backlog 登记补打决定）；
`.git/config` 的 `pull.ff=only` 与 squash 策略一致，保留。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `docs/PROCESS.md:18-36`：分支命名、从 origin/main 切出、远端只留 main。
- `docs/PROCESS.md:76-79`：squash、合并删分支（含 `-D`）、demo tag、CHANGELOG。
- `docs/PROCESS.md:137`：16 个本地分支"假未合并"的成因与判断口径。
- `docs/PROCESS.md:143`：红线（直接 push main 即打回）。
- `.git/packed-refs:2-14`：13 个本地分支引用。
- `.git/packed-refs:15-20`：6 个远端分支引用；全文无 `refs/tags` 条目（0 tag）。
- `.git/config:12-20`：main 跟踪 origin、`pull.ff=only`、origin URL。
- `.git/HEAD:1`：当前分支 `docs/process-baseline`。
- `.git/logs/HEAD:39-40`、`:104-108`：分支合入 main 后旧分支仍存在的记录。
- 2026-11-18 盘点（lead 提供，仓库内不可复核）：PR #3 52 文件 +9029、PR #6 43 文件 +11165，
  是采用 squash 后单 PR 体量偏大的直接动因。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0006（周号与 Demo 日期，tag 命名依赖它）、ADR-0009（评审与 gate）、ADR-0011（CI）。
- `docs/PROCESS.md` §2/§5；计划 §3.4。
