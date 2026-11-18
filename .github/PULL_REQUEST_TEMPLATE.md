# 摘要

<!-- 1–3 行：这个 PR 做了什么、为什么现在做。不要贴 diff，不要复制计划全文。 -->

## 关联

- Issue：
- 计划条目：`docs/Train0-Plan-6x1.md:行号`（如 `:466` S1-7）
- 关联 ADR：`docs/adr/NNNN-*.md`（无则删掉本行）

## 自检清单（作者逐条勾，评审者抽查 3 项）

- [ ] 构建通过：改 `probe-common`/`probe-ebpf` 时跑 `bash scripts/ci/run.sh cargo xtask bpf`；改 `control-center/**` 时跑 `bash control-center/scripts/up.sh`（宿主无 `make`）
- [ ] 必跑测试与 `CONTRIBUTING.md:94-100` 的改动表一致，并在下方写出实际跑了哪层：<!-- 例：--layer l2 -->
- [ ] 门禁本地复现：`bash scripts/ci/run.sh bash scripts/ci/fmt.sh`、`bash scripts/ci/run.sh cargo clippy --workspace --exclude probe-ebpf --all-targets -- -D warnings`、按改动范围取 `cargo xtask test --layer l0|l1|l2|all`
- [ ] 文档已更新，并列出改了哪份：STATUS / PROCESS / ADR / 契约（`README.md` ABI 表、`agent/proto/control.proto`、CSV 列序）/ 无
- [ ] `CHANGELOG.md` 的 `## [Unreleased]` 已加一条（含 PR 号）
- [ ] 无 `.env`、`target/`、CSV/日志、`out/`、`verification/` 下的运行产物
- [ ] 体量 `additions ≤ 400` 且 `changedFiles ≤ 20`；超出时已填下方“拆分理由”

## 拆分理由（仅当超出体量门）

<!-- 写清为什么不能拆：同一契约必须原子改、拆分会导致 gate 红、或纯文档基线。评审者明确同意后才合并。 -->

## 风险与回滚

<!-- 失败模式：影响哪层（L0–L3 / 控制面 / 部署）；回滚动作：revert 本 PR 后是否需要补数据迁移或重新 tag。 -->

## 演示 / 截图（可选）

<!-- 命令与期望输出，或界面截图路径；不写“见 PR 描述”。 -->

## 评审者关注点

<!-- 点名 2–3 处最需要看的文件或判断：契约是否同步、测试是否覆盖边界、文档是否漂移。 -->
