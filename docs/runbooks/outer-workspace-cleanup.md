# 外层工作区重复副本清理

- 状态：**未执行**，需人工确认后执行
- 范围：`D:\repos\rust\eBPF\docs\`、`D:\repos\rust\eBPF\verification\`（均不在 git 内）
- 目标：`probe/docs/`、`probe/verification/` 成为唯一来源，外层不再保留同名副本

## 1. 前置条件

- 在 Git Bash 中操作，工作目录 `D:/repos/rust/eBPF`；`probe` 工作树干净（`git -C probe status --short` 为空或只剩本次已知改动）。
- 确认没有 e2e/perf 任务正在写外层 `verification/`。
- `.tmp/` 有足够空间存放两份目录副本（当前 `verification/` 约百来个文件）。
- 已读 `docs/STATUS.md`、`docs/PROCESS.md`（若已存在）。

## 2. 影响面

- 只删外层副本，`probe/` 内文件不动；外层目录不入库，删除不影响 git 历史。
- 删除后所有脚本/文档必须引用 `probe/docs/...`、`probe/verification/...`。
- 外层 `verification/` 还有日志/CSV 等运行产物，可能不被仓库跟踪；步骤 3 先列独有文件，未确认前不删。
- 可逆性：依赖步骤 4 的备份；备份在 `.tmp/`，不提交、保留至少 7 天。

## 3. 比对（只读）

### 3.1 同名文件与哈希

```bash
cd /d/repos/rust/eBPF
mkdir -p .tmp

comm -12 <(cd docs && ls -1 | sort) <(cd probe/docs && ls -1 | sort) > .tmp/docs-same.txt
while read -r f; do
  a=$(sha256sum "docs/$f" | cut -d' ' -f1)
  b=$(sha256sum "probe/docs/$f" | cut -d' ' -f1)
  [ "$a" = "$b" ] && echo "SAME docs/$f" || echo "DIFF docs/$f"
done < .tmp/docs-same.txt

comm -12 <(cd verification && ls -1 | sort) <(cd probe/verification && ls -1 | sort) > .tmp/verification-same.txt
while read -r f; do
  a=$(sha256sum "verification/$f" | cut -d' ' -f1)
  b=$(sha256sum "probe/verification/$f" | cut -d' ' -f1)
  [ "$a" = "$b" ] && echo "SAME verification/$f" || echo "DIFF verification/$f"
done < .tmp/verification-same.txt
```

已知基线（2026-11-18 采样，执行时以命令输出复核）：

- 外层 `verification/` 与 `probe/verification/` 有 **5 份同名**，其中 **4 份字节相同**。
- `verification/TEST-PLAN.md`：外层 **379 行**，仓库 **428 行**；以仓库版为准，差异是后续补充的分层与门禁内容。
- 外层 `verification/` 缺 `REPORT-control-plane.md`。
- 外层 `docs/` 与 `probe/docs/` 同名文件为 `Train0-Plan-6x1.md`、`Control-Agent-API-v1.md`，预期字节相同，用上面的循环确认。

若实测与基线不一致，以实测为准；同名数或相同数偏差超过 1 份时先停下复盘，不要继续删除。

### 3.2 差异复核

```bash
wc -l verification/TEST-PLAN.md probe/verification/TEST-PLAN.md
diff -u verification/TEST-PLAN.md probe/verification/TEST-PLAN.md | head -80
diff -rq docs probe/docs
diff -rq verification probe/verification | head -50
```

预期：`wc -l` 输出 379 与 428；`diff -rq` 只报 `TEST-PLAN.md` 和外层独有文件。

### 3.3 外层独有文件清单

```bash
comm -23 <(cd verification && ls -1 | sort) <(cd probe/verification && ls -1 | sort) > .tmp/outer-only-verification.txt
wc -l .tmp/outer-only-verification.txt
cat .tmp/outer-only-verification.txt
```

判定规则：

- `.log`、`.csv` 是运行产物，可重跑，不构成“独有内容”。
- 外层独有 `.sh`（预期为已被 `cargo xtask e2e` / `run_perf.sh` 取代的旧脚本）在备份中保留，并在执行记录里列名。
- 若出现未知 `.md`、`.rs` 或其它源文件，**停止删除**，先决定收入仓库还是明确废弃；不要直接 `rm -rf`。

## 4. 备份

```bash
DATE=$(date +%Y%m%d)
test ! -e ".tmp/backup-$DATE" || { echo "backup dir exists"; exit 1; }
mkdir -p ".tmp/backup-$DATE"
cp -a docs ".tmp/backup-$DATE/docs"
cp -a verification ".tmp/backup-$DATE/verification"
du -sh ".tmp/backup-$DATE"
find ".tmp/backup-$DATE" -type f | wc -l
```

记录 `DATE`、文件数和占用空间。备份就是回滚源，不要提前删除。

## 5. 删除外层副本

```bash
DATE=YYYYMMDD   # 与步骤 4 相同
test -d ".tmp/backup-$DATE/docs" && test -d ".tmp/backup-$DATE/verification" || { echo "backup missing"; exit 1; }
rm -rf docs verification
```

Windows 资源管理器或编辑器占用会导致 `rm` 失败；关掉占用进程后重试，不要改删仓库内文件。

## 6. 同步引用

本 runbook 只负责目录；引用改动随同一 PR 完成，至少覆盖：

```bash
grep -rn "工作区.*docs\|仓库外\|D:.*docs" probe --include='*.md' --include='*.rs' --include='*.proto'
```

预期不再命中“工作区 docs”这类表述；历史报告里描述当次产物位置的句子可保留，但不得指向已删除的路径。

## 7. 验证

```bash
test ! -e docs && test ! -e verification && echo "outer copies removed"
git -C probe status --short
wc -l probe/verification/TEST-PLAN.md
test -f probe/verification/REPORT-control-plane.md && echo "report present"
grep -rn "工作区.*docs\|仓库外" probe --include='*.md' | head
```

- 外层 `docs/`、`verification/` 不存在。
- 仓库版 `TEST-PLAN.md` 为 428 行，`REPORT-control-plane.md` 存在。
- 下次 PR 的 `gate` 通过（纯文档改动不触发 Rust job，`gate` 恒执行）。

## 8. 回滚

```bash
DATE=YYYYMMDD   # 与步骤 4 相同
cp -a ".tmp/backup-$DATE/docs" .
cp -a ".tmp/backup-$DATE/verification" .
```

只误删部分文件时，从备份按文件名恢复。外层没有 git 历史，备份是唯一回滚源。

## 9. 是否已执行

**未执行。** 执行后回填：日期、执行人、同名数/相同数/差异文件、外层独有清单、备份路径、删除结果、验证输出。
