# Changelog

本文件遵循 [Keep a Changelog](https://keepachangelog.com/zh-CN/1.1.0/) 的组织方式，版本号声明遵循 [SemVer](https://semver.org/lang/zh-CN/)。
`0.x` 阶段不承诺向后兼容：ABI（`probe-common/src/lib.rs`）、CSV 列序（样本 7 列 / 聚合 16 列）与 proto（`agent/proto/control.proto`）的破坏性改动都记在 `Changed` 下并升 `<major>`。

## 维护规则

1. 每个合并 PR 必须在 `## [Unreleased]` 下加一条，放在 `Added` / `Changed` / `Fixed` / `Docs` 之一，行尾带 PR 号。
2. demo 时把整段 `Unreleased` 落成版本段 `v0.<周>-w<N>`（如 `v0.2-w2`），并打同名 tag（见 `docs/PROCESS.md` §5）。
3. 条目只写“改了什么 + 证据在哪”，不复制 PR 正文；跨 PR 的同类改动合并为一条。
4. demo tag 不是 SemVer 发布：首个正式发布从 `0.1.0` 起，届时把 `v0.<周>-w<N>` 段的条目归并进来。

## [Unreleased]

### Added

- 流程规范 `docs/PROCESS.md`：范围与原则、分支命名、提交、PR 与评审降配、合并发布、Issue、ADR、DoD、反例、责任矩阵、文档关系（本 PR）。
- 状态与索引文档：`docs/STATUS.md`（唯一状态看板）、`docs/backlog.md`（未排期池）、`docs/README.md`（文档索引）、`docs/adr/`（架构决策记录）、`docs/runbooks/`（操作手顺）（本 PR）。
- `.github/PULL_REQUEST_TEMPLATE.md`：自检清单 7 项、拆分理由、风险与回滚（本 PR）。
- `.github/ISSUE_TEMPLATE/task.md` 与 `bug.md`：任务必填验收门（命令 + 期望输出），缺陷必填复现与环境（本 PR）。

### Changed

- 评审规则从“需要 GitHub approve”降为“≥1 条实质评审 + 自检清单逐条勾”，`reviewDecision` 允许为空（本 PR，理由见 `docs/PROCESS.md` §4）。
- PR 大小门定为 `additions ≤ 400` 且 `changedFiles ≤ 20`，超出需写拆分理由（本 PR，依据 #3/#5/#6 的体量）。

### Docs

- 状态只写在 issue 与 `docs/STATUS.md`，计划文档（`docs/Train0-Plan-6x1.md`）只保留目标与验收门（本 PR）。
- 明确 `CONTRIBUTING.md` 管构建与门禁命令、`docs/PROCESS.md` 管协同时序（本 PR）。

## [0.0.0] - 2026-10-31..2026-11-18

仓库从未打过 tag，本段为按合并顺序（`main` 上 10 次 squash）回填；提交主题取自本地 reflog，若与 PR 页标题不一致以 PR 页为准。

- `#1` CI 从 Gitee 流水线移植到 GitHub Actions（`.github/workflows/ci.yml`、`scripts/ci/`）。
- `#2` 移除 Gitee Go 遗留配置，CI 变量名去掉 `CI_*` 别名（`.workflow/ci.yml` 已不存在于任何分支）。
- `#3` 落地 L0–L2 分层测试：二进制拆库 + 显式端口，新增 `cargo xtask test` 与快速 CI gate（证据：`verification/REPORT-layered-tests.md`）。
- `#4` 解析移入 `probe-common`，新增共享帧夹具与 L0/L1 用例，收紧 CSV 列序断言（证据：`probe/tests/csv_schema.rs`）。
- `#5` 控制中心骨架：Spring 双角色 + Postgres + SPA + k3d，CI 按路径拆成 Rust 与 control-center 两条（证据：`control-center/README.md`）。
- `#6` S4 真机 e2e：E-01…E-07 全部 Rust 化，删除三个 bash 断言脚本，只留 `run_perf.sh` 走 `--legacy`（证据：`verification/REPORT-e2e.md`）。
- `#7` agent 控制面：配置解码、六步校验、mTLS 客户端、进程内假控制面与上报状态机（证据：`verification/REPORT-control-plane.md`）。
- `#8` Spring Boot 升到 4.1.2（`control-center/app/pom.xml`）。
- `#9` 接入 protobuf/gRPC 构建骨架与 proto 同源门禁，固化配置文档字节向量与值域校验器（`control-center/scripts/proto-check.sh`、`control-center/app/src/main/java/dev/train0/cc/control/ConfigDocumentValidator.java`）。
- `#10` 控制面 mTLS gRPC 服务与发布路径：schema 扩到 `control-010` + `db-verify`、`ConfigPublisher` 与 8082 发布接口、agent 身份与 RPC、上报 ingest、k3d TLS 与 smoke 断言（证据：`control-center/docs/plan-control-plane-impl.md`）。
