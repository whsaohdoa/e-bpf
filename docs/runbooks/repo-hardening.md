# 仓库加固（GitHub 侧）

- 状态：**未执行**（本文件只给命令，不改任何设置）
- 目标仓库：`whsaohdoa/e-bpf`（`probe/` 的 origin），默认分支 `main`
- 权限：当前 gh 登录账号为 ADMIN；仍须逐条确认后再执行

## 1. 前置条件

```bash
cd /d/repos/rust/eBPF/probe
REPO=whsaohdoa/e-bpf

gh --version
gh auth status
gh repo view "$REPO" --json nameWithOwner,defaultBranchRef,hasIssuesEnabled,visibility
```

预期 `defaultBranchRef.name` 为 `main`，`visibility` 按实际记录。所有命令显式用 `-R` 或 `$REPO`，不依赖当前目录。

已读 `CONTRIBUTING.md:128-132`：`gate` **恒执行**，把 skipped 视作通过，分支保护只需要求它一个检查。

## 2. 影响面

- ruleset / 分支保护会改变所有 PR 的合并条件；配错会阻塞合并。
- 删除远端分支不可逆（除非预先记录 SHA）。
- Issues 开关、模板、CODEOWNERS 只影响协作界面，不影响代码和 CI。

## 3. 启用 Issues

事实：Issues 已启用，但零使用。

```bash
gh repo edit "$REPO" --enable-issues
gh repo view "$REPO" --json hasIssuesEnabled --jq .hasIssuesEnabled
gh issue list -R "$REPO" --limit 5
```

验证：第二条输出 `true`；第三条可执行（0 条也正常）。
回滚：`gh repo edit "$REPO" --enable-issues=false`（只隐藏入口，已有 issue 不删除）。

## 4. PR / Issue 模板

事实：模板已由他人创建；本 runbook 只写启用与检查，不新建文件。

```bash
gh api "repos/$REPO/contents/.github" --jq '.[].name'
```

预期含 `PULL_REQUEST_TEMPLATE.md`（或 `pull_request_template.md`）和 `ISSUE_TEMPLATE` 目录。缺失时停下交给模板作者，不在本 runbook 补建。

验证：新建测试 PR 时正文自动带模板，`gh pr view --json body` 可见。
回滚：没有设置变更；模板文件本身走 PR revert。

## 5. 只要求 gate 检查

理由：`.github/workflows/ci.yml` 的 `changes` 用 paths-filter 判断改动落在哪半边，`fast`、`bpf` 在无关改动时 skipped，只有 `gate` 恒执行并把 skipped 视作通过（`CONTRIBUTING.md:128-132`）。因此要求 `fast`/`bpf` 会让纯文档或单侧改动永久 pending。故只要求 `gate`，且不强制 approve：评审 ≥1 人是约定，不是合并闸门（另见 `CONTRIBUTING.md` 第 8 节与 `docs/PROCESS.md`）。

### 5.1 方案 A：ruleset（推荐）

```bash
gh api --method POST "repos/$REPO/rulesets" --input - <<'JSON'
{
  "name": "require-gate",
  "target": "branch",
  "enforcement": "active",
  "conditions": {
    "ref_name": {
      "include": ["refs/heads/main"],
      "exclude": []
    }
  },
  "rules": [
    {
      "type": "required_status_checks",
      "parameters": {
        "strict_required_status_checks_policy": false,
        "required_status_checks": [
          { "context": "gate" }
        ]
      }
    }
  ]
}
JSON
```

验证：

```bash
RULESET_ID=$(gh api "repos/$REPO/rulesets" --jq '.[] | select(.name=="require-gate") | .id')
gh api "repos/$REPO/rulesets/$RULESET_ID" --jq '{name,enforcement,rules}'
```

预期 `name=require-gate`、`enforcement=active`，`rules` 中 required_status_checks 的 `context` 为 `gate`。

回滚：

```bash
gh api --method DELETE "repos/$REPO/rulesets/$RULESET_ID"
```

### 5.2 方案 B：经典分支保护（与 A 二选一）

```bash
gh api --method PUT "repos/$REPO/branches/main/protection" --input - <<'JSON'
{
  "required_status_checks": { "strict": false, "contexts": ["gate"] },
  "enforce_admins": false,
  "required_pull_request_reviews": null,
  "restrictions": null,
  "allow_force_pushes": false,
  "allow_deletions": false
}
JSON
```

`"required_pull_request_reviews": null` 即不要求 approve；`strict=false` 不要求分支最新。

验证：

```bash
gh api "repos/$REPO/branches/main/protection" \
  --jq '{checks: .required_status_checks.contexts, reviews: .required_pull_request_reviews}'
```

预期 `checks=["gate"]`、`reviews=null`。

回滚：

```bash
gh api --method DELETE "repos/$REPO/branches/main/protection"
```

注意：A/B 同时启用会叠加限制，只选一个。

## 6. CODEOWNERS（可选）

目的：把 Rust 数据面、agent、control-center 的评审请求自动指向维护者；CODEOWNERS 本身不强制 approve。

建议内容（新建 `.github/CODEOWNERS`，通过 PR 提交，`@<owner>` 换成真实账号）：

```text
/probe-ebpf/      @<owner>
/probe-common/    @<owner>
/agent/           @<owner>
/control-center/  @<owner>
/docs/            @<owner>
```

不要写不存在的团队；启用前确认账号对仓库有写权限。

验证：

```bash
gh api "repos/$REPO/contents/.github/CODEOWNERS" --jq .path
gh pr view --json reviewRequests
```

预期对应 PR 的 `reviewRequests` 出现 owner。

回滚：删除 `.github/CODEOWNERS` 的 PR 合并即可；历史评审请求不受影响。
当前未启用的原因：仓库以单人维护为主，收益有限。

## 7. 删除已合并远端分支

前提：`main` 是唯一长期分支，功能分支合并即删（`AGENTS.md` §1）。

先 dry-run：

```bash
gh pr list --repo "$REPO" --state merged --limit 200 \
  --json headRefName,mergedAt \
  --jq '.[] | select(.headRefName != "main") | .headRefName' | sort -u | tee .tmp/merged-branches.txt
```

人工确认列表不含长期分支。记录 SHA 再删：

```bash
: > .tmp/deleted-branches.txt
while read -r b; do
  sha=$(git ls-remote origin "refs/heads/$b" | cut -f1)
  [ -n "$sha" ] || { echo "skip $b (no ref)"; continue; }
  echo "$b $sha" >> .tmp/deleted-branches.txt
  git push origin --delete "$b"
done < .tmp/merged-branches.txt
```

验证：

```bash
gh api "repos/$REPO/branches" --paginate --jq '.[].name'
test -s .tmp/deleted-branches.txt && cat .tmp/deleted-branches.txt
```

预期只剩 `main`（及明确保留的长期分支）。

回滚（按记录的 SHA 重建）：

```bash
while read -r b sha; do
  git push origin "$sha:refs/heads/$b"
done < .tmp/deleted-branches.txt
```

不要依赖 GitHub 删除后的短期 ref；以 `.tmp/deleted-branches.txt` 为准。

## 8. 合并审计

```bash
gh pr list --repo "$REPO" --state merged --limit 100 \
  --json number,title,mergedAt,headRefName,mergeCommit \
  --jq '.[] | [.number, .mergedAt, .headRefName, .title] | @tsv'
```

验证：条数与 GitHub PR 页面 merged 数一致；抽查 `mergeCommit.oid` 在 `git log` 中可见。
回滚：只读，无。

## 9. 是否已执行

全部**未执行**。执行后逐条回填：命令、时间、执行人、验证输出、是否回滚。
