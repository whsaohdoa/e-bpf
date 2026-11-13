# AGENTS.md — 在这个仓库干活的硬性约定

本文件只写“会踩坑的约束、禁区、去哪找”。命令细节一律在 `CONTRIBUTING.md`，别在这里复制。

## 1. 仓库边界

| 路径 | 角色 |
|---|---|
| `D:\repos\rust\eBPF` | 工作区外层，**不是** git 仓库；`verification/`（日志/CSV）、`docs/`（Train 0 计划）、`out/`、`.tmp/` 都在这层 |
| `D:\repos\rust\eBPF\probe` | **唯一 git 仓库根**，Cargo workspace 根，也是本文件所在目录 |
| `probe/` | 用户态 crate（CLI、attach、ringbuf 消费、CSV） |
| `control-center/` | **独立的 Spring + Postgres + SPA + k3d 子项目**，有自己的 README/Makefile/scripts，见 §6 |

git 事实**不要写死在这里**（分支名、远端分支、commit 都会变）——用命令查：

```bash
git status -sb                 # 当前分支 / ahead-behind
git log --oneline -5           # 最近提交
git branch -a                  # 本地与远端分支
git remote -v                  # origin = https://github.com/whsaohdoa/e-bpf.git
```

不变的事实：

- 提交风格：Conventional Commits（`feat:` / `fix:` / `refactor:` / `test:` / `docs:` / `ci:` / `chore:`），正文中英均可。
- 备份 `probe-backup-*.bundle`（git bundle，在工作区外层）**不要删**。
- 功能分支合并后即删；远端只保留长期分支。
- 本机 git 到 `github.com:443` 曾不通（schannel `SEC_E_NO_CREDENTIALS`），只有 `gh api` 可走；推不动时先确认是网络还是凭据，别反复强推。

## 2. 构建环境

- Windows 宿主**没有** `cargo`/`rustup`，所有 Rust 命令在容器 `aya-dev` 里跑（`docker compose exec aya-dev bash -lc 'cd /workspace/probe && …'`）。
- 宿主**有** `java`(17) / `mvn` / `node` / `npm` / `kubectl` / `k3d`，**没有** `make`。
- 路径映射：宿主工作区 ↔ 容器 `/workspace`，仓库在容器里是 `/workspace/probe`。

## 3. 命令速查（细节见 `CONTRIBUTING.md`）

```bash
cargo xtask test --layer l0|l1|l2|all   # 分层测试（默认 all：L0→L1→L2，毫秒~秒级）
cargo xtask bpf                          # 只构建 eBPF 对象（nightly + bpf-linker）
cargo xtask e2e [--case E-0N] [--jobs N]# L3 真机驱动（特权容器），E-01…E-07
cargo xtask e2e --legacy [--script NAME]# 遗留 bash：目前只剩 verification/run_perf.sh 性能对比
cargo xtask send --iface IF [--count N]  # 定帧发送器（与 e2e 共用 testkit）
cargo xtask report                       # 汇总上次 JUnit
cargo xtask soak --minutes N             # 占位，未实现
```

改动 → 最小验证对照表在 `CONTRIBUTING.md#3-测试与门禁`，不要在别处复制一份。

## 4. 不变量（改动前先确认没破坏）

- **语言**：源码注释英文；`README.md`、`CONTRIBUTING.md`、`verification/*.md`、提交说明用中文（可混英文术语）。
- **ABI**：唯一来源 `probe-common/src/lib.rs`。改布局 = 改 `README.md` 的 ABI 表 + 布局单测 + 兼容政策；`SampleEvent` 恒 64 B。
- **CSV**：样本 7 列、聚合 16 列，顺序固定；改动要同步 `README.md` 与 L1（`probe/tests/csv_schema.rs`）/L2（`pipeline.rs`）断言。
- **数据面**：无分配、无 per-packet 日志、解析失败也返回 `XDP_PASS`；CONFIG 缺失时用默认窗口并累加 STATS，绝不因此丢包；AGG 溢出只计数。
- **端口化**：内核交互只留在 `app.rs`/`ports.rs`；可测逻辑进 `pipeline`/`aggregate`，用假 `AggStore`、脚本化 ringbuf、注入时钟。
- **unsafe**：eBPF 侧 unsafe 必须有 `// SAFETY:` 说明。
- **panic handler**：`probe-ebpf` 的 `#[cfg(all(not(test), target_arch = "bpf"))]` 不能删（否则宿主 `cargo check` 报 E0152，见 `CONTRIBUTING.md`）。
- **对象嵌入**：`probe/build.rs` 嵌套构建 `probe-ebpf` 并嵌入二进制；改 `probe-common`/`probe-ebpf` 必须重新 `cargo build`；覆盖率用 `PROBE_EBPF_PREBUILT` 逃生口。

## 5. 不要做

- 不要在 Windows 宿主直接 `cargo build/test`（无工具链，且 BPF 构建需要 Linux）。
- 不要跑 `sudo cargo test`：特权只属于 e2e/attach。
- 不要把 `.env`、`target/`、CSV/日志、`out/`、工作区外层 `verification/` 的产物提交进仓库。
- 不要绕过 `cargo xtask` 手拼测试命令（分层与 JUnit 输出都依赖它）。
- 不要在 `probe-ebpf` 里用 `std`、动态分配或浮点。
- 不要删除 `probe-backup-*.bundle`、`.git`、`verification/run_perf.sh`。
- 不要在 `control-center/` 里假定宿主 JDK 可用（宿主是 17，项目要 21），也不要在宿主调 `make`（没有）。

## 6. control-center（第二个子系统）

- 入口文档：`control-center/README.md`（四个镜像的边界、拓扑端口、compose/k3d、Liquibase 双库、
  审计表 append-only、“已经有的 / 还没有的”清单）。**先读它再动手**，那里的“还没有的”是刻意声明的范围，不是 TODO 遗漏。
- 构建：`bash control-center/scripts/up.sh` / `smoke.sh` / `k3d-up.sh`，或 `make`（宿主无 make 时用 Git Bash）。
- CI：`.github/workflows/control-center.yml`（`mvn package` + `vite build` + compose/kustomize 静态校验），
  Rust 侧的 `ci.yml` 不会因为改这里而跑。
- 与 Rust 侧的关系：两者尚未接线，probe 不向控制中心上报（E8 未落地）。

## 7. 文档索引

| 想知道 | 读 |
|---|---|
| 外部用法、架构、map/ABI/CSV 契约、产品级限制 | `README.md` |
| 构建/容器/测试分层/门禁/真机 e2e/代码地图 | `CONTRIBUTING.md` |
| 测试分层设计与用例编号 | `verification/TEST-PLAN.md` |
| S1–S3 分层测试、S4 e2e、聚合验收证据 | `verification/REPORT-layered-tests.md`、`REPORT-e2e.md`、`REPORT-aggregation.md` |
| 控制中心边界、部署、迁移 | `control-center/README.md` |
| Train 0 计划（仓库外） | 工作区 `docs/Train0-Plan-6x1.md` |

工作区外层的 `AGENTS.md` 只是指向本文件的指针；内容以本文件为准。
