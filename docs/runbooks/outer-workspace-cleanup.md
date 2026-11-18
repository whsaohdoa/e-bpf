# 外层工作区重复副本清理

- 状态：**已执行**（2026-11-18），执行记录见 §9
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

**已执行，2026-11-18。** 执行记录：

- 执行人：dsh agent（lead）。宿主无 Git Bash（只有 WSL `bash.exe`），逐步操作用 pwsh + .NET 文件 API 完成；语义与本文命令一致（`sha256` 用 `Get-FileHash -Algorithm SHA256`）。
- 前置：`git -C probe status --short` 为空（分支 `docs/process-baseline`）；外层 `verification/` 最新写入 2026-11-17 21:24，无任务在写；D: 可用 67 GB。
- 比对（实测比基线多 2 份同名，已复盘）：同名 **7** 份、字节相同 **6** 份；多出的是基线采样后新纳入仓库的 `run_perf.sh`、`traffic_gen.rs`（均字节相同），属基线漂移。
  - `verification/TEST-PLAN.md`：外层 379 行 / 仓库 428 行（与基线一致，以仓库版为准）。
  - `docs/Control-Agent-API-v1.md`：外层 1013 行 / 仓库 1015 行，4 行差异（外层“端口预留”，仓库“六个方法已实现”）；外层 mtime 2026-11-17、仓库 2026-11-18 → 以仓库版为准。`Train0-Plan-6x1.md` 字节相同。
  - 外层独有：`.log` 93、`.csv` 29、遗留 `.sh` 3（`run_e2e.sh`、`run_e2e_ipv6.sh`、`run_aggregation.sh`，已被 `cargo xtask e2e` / `run_perf.sh` 取代，仅保留在备份），目录 3：`control/`（14）、`drawio-probe/`（54）、`perf/`（20）。
  - 未知 `.md` 判定：`perf/RESULTS-A9.md` 被 `verification/REPORT-aggregation.md` §6 引用，按“收入仓库”处理；与 `perf-*.txt`、`perf-summary.tsv`、`perf-capability.txt` 一起复制到仓库 `verification/perf/`，`control/` 目录同样复制到 `verification/control/`（两者均在 `verification/.gitignore` 内，不进 git）。
- 备份：`D:\repos\rust\eBPF\.tmp\backup-20261118\`（`docs/` 2 + `verification/` 220 = 222 文件，22.3 MB；逐文件 sha256 与源比对，0 不一致）。保留到 2026-11-25 之后。
- 删除：外层 `docs/`、`verification/` 已删除（`Test-Path` 均 False）。
- 引用同步（同一批改动）：`verification/run_control_demo.sh` 默认输出 `../verification/control` → `$REPO/verification/control`；`verification/run_perf.sh` 的 `GEN_SRC`、`PERF_DIR` 默认值由 `/workspace/verification/…` 改为 `/workspace/probe/verification/…`（否则性能脚本会把外层 `verification/` 复活）；`xtask/src/main.rs` 的 `e2e --legacy` 路径改为仓库内 `/workspace/probe/verification/`；`verification/.gitignore` 增 `control/`；`REPORT-control-plane.md`、`REPORT-aggregation.md` 去掉“工作区外层”表述；`docs/README.md`、`docs/runbooks/00-README.md`、`docs/adr/0010-docs-single-entry.md`、`docs/backlog.md`、`AGENTS.md`、`CONTRIBUTING.md`、`.github/ISSUE_TEMPLATE/bug.md`、`.github/PULL_REQUEST_TEMPLATE.md` 同步更新。
- 验证：`probe/verification/TEST-PLAN.md` 428 行；`REPORT-control-plane.md` 存在；外层两目录不存在；`git check-ignore` 命中 `verification/control/g1.csv`、`verification/perf/RESULTS-A9.md`；仓库内 `grep` 已无指向外层 docs/verification 的现行引用（仅剩历史记录句）；容器内 `cargo check -p xtask`、`bash -n verification/run_perf.sh` 通过；`git -C probe status --short` 只剩本批改动。
- 回滚：见 §8（备份目录 `.tmp/backup-20261118/`）。
