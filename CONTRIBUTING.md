# 贡献指南（probe）

面向在本仓库改代码的人和 AI agent：只回答“改哪里、跑什么、哪里会踩坑”。
外部使用说明在 `README.md`，硬性约定与禁区在 `AGENTS.md`。

## 1. 环境

| 事实 | 说明 |
|---|---|
| git 根 = 本目录 | 工作区外层（`D:\repos\rust\eBPF`）不是仓库；`verification/`、`docs/`、`out/` 在仓库外 |
| 宿主没有 Rust 工具链 | Windows 宿主没有 `cargo`/`rustup`，所有 Rust 命令都在容器里跑 |
| 开发容器 | `docker compose` 服务名 `aya-dev`，特权 + host 网络 + memlock 无限制，工作目录 `/workspace` |
| 路径映射 | 宿主工作区 ↔ 容器 `/workspace`，因此仓库在容器里是 `/workspace/probe` |

```bash
docker compose exec aya-dev bash -lc 'cd /workspace/probe && <cmd>'
```

`docker-compose.yml` 的默认挂载是 `probe/` 自身；当前开发容器把**外层目录**挂到 `/workspace`
（所以 `/workspace/probe` 就是仓库）。若按默认值启动，真机脚本依赖的 `/workspace/probe`、
`/workspace/verification` 不存在，需要用 `AYA_DEV_WORKSPACE_HOST` 指到外层。

## 2. 构建与运行

```bash
cargo build --release
sudo -E ./target/release/probe --iface veth0 \
    --sink csv --out /workspace/packets.csv --agg-out /workspace/aggregates.csv \
    --window-ms 10000 --grace-ms 2000 --scan-ms 1000 \
    --src-prefix-v4 24 --src-prefix-v6 64 --flush-ms 200 --append
```

- `probe/build.rs` 会嵌套构建 `probe-ebpf` 并把对象嵌进二进制：改了 `probe-common`/`probe-ebpf`
  之后**必须重新 `cargo build`**，不能只替换二进制里的对象。覆盖率场景用
  `PROBE_EBPF_PREBUILT` 逃生口跳过嵌套构建。
- 加载 XDP 需要特权，`cargo test`/`cargo run` 不需要。`.cargo/config.toml` 里已无全局
  `runner = "sudo -E"`（它曾让单测也依赖密码提示，并掩盖了“只有加载 eBPF 才需要特权”），
  特权只属于 e2e/attach。
- edition 2024；`rustfmt.toml` 开了 `group_imports=StdExternalCrate`、`imports_granularity=Crate`、
  `unstable_features`，只有 nightly rustfmt 生效。`scripts/ci/fmt.sh` 找不到 nightly 时回落
  stable 并打印三条 `unstable features ... only available in nightly` 警告——此时 import 布局
  其实没被检查，改过 import 要人工确认。
- 新依赖加到 `[workspace.dependencies]`，默认 `default-features = false`；不要往 `probe-ebpf`
  引入仅宿主可用的 crate。

### rust-analyzer 的 E0152

`probe-ebpf` 的 panic handler 只在真正的 BPF 目标上定义：

```rust
#[cfg(all(not(test), target_arch = "bpf"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! { loop {} }
```

原因是 `probe` 启用了 `probe-common/user` → `aya` → `std`；当 rust-analyzer 或
`cargo check -p probe -p probe-ebpf` 以 host target + workspace 特征统一去分析
`probe-ebpf` 时，std 已经提供 `panic_impl`，再定义一次就会报“found duplicate lang
item `panic_impl`”。真实 eBPF 构建使用 `--target bpfel-unknown-none` +
`-Z build-std=core`，没有 std，仍然需要该 handler。`target_arch = "bpf"` 正好区分这
两种视图，不改变生成的 eBPF 对象。这个 `#[cfg]` 不能删。

## 3. 测试与门禁

全部宿主侧测试不需要特权、不需要内核：

| 层 | 命令 | 内容 |
|---|---|---|
| L0 | `cargo xtask test --layer l0` | `aggregate` / `sink` / `ports` 的纯逻辑单测与 ABI 布局 |
| L1 | `cargo xtask test --layer l1` | CLI 契约：`probe/tests/csv_schema.rs`（列序、表头、退出码） |
| L2 | `cargo xtask test --layer l2` | 进程内管线 P-01…P-16：假 AGG map、可脚本化 ringbuf、内存 sink、注入时钟 |
| L3 | `cargo xtask e2e` | 特权真机驱动 `probe/tests/e2e.rs`，E-01…E-07 |

`cargo xtask test`（不带 `--layer`）按 L0→L1→L2 跑全部；优先用 `cargo nextest`
（`--profile ci` 额外写 JUnit），未安装时自动回落 `cargo test`。`cargo xtask report`
汇总上一次 JUnit。`cargo xtask soak` 仍是占位。

改完代码至少跑到对应行：

```bash
bash scripts/ci/run.sh bash scripts/ci/fmt.sh                                   # nightly rustfmt
bash scripts/ci/run.sh cargo clippy --workspace --exclude probe-ebpf --all-targets -- -D warnings
bash scripts/ci/run.sh cargo xtask test --layer all
bash scripts/ci/run.sh cargo test --doc --workspace
bash scripts/ci/run.sh bash scripts/ci/coverage.sh                             # 需要覆盖率时
```

| 改动范围 | 必跑 |
|---|---|
| `aggregate.rs` / `sink.rs` / `ports.rs` 纯逻辑 | `--layer l0` |
| `pipeline.rs` / CSV 语义 / 窗口关闭 | `--layer l2` |
| CLI 参数、退出码、列序契约 | `--layer l1` |
| `probe-common` ABI 或 `probe-ebpf` | `cargo xtask bpf` + `--layer l0` |
| attach / 内核路径 / 性能 | `cargo xtask e2e`（特权容器）；性能对比用 `cargo xtask e2e --legacy` |

L3 用例（`cargo xtask e2e [--case E-0N] [--jobs N]`，默认 `-j 4`，全绿约 10–20 秒）：

| 用例 | 覆盖内容 |
|---|---|
| E-01 | attach 生命周期：`bpftool net` 先无后有、`run_cnt` 增长、SIGTERM 后无残留 |
| E-02 | 100 帧落进单个 200 ms 窗口：`Σcount=100`、`packets_total=100`、无迟到/畸形 |
| E-03 | 16 条固定帧矩阵逐条对照 `tests/fixtures/frames/index.tsv`（9 收 7 拒） |
| E-04 | SIGTERM 打断未关闭窗口：写出 `window_complete=0` 的 partial 行，统计完整 |
| E-05 | `CONFIG` 版本不匹配：`STATS[CONFIG_MISSING]` 累加，仍 `XDP_PASS`、计数不丢 |
| E-06 | `--features small-maps`（AGG=1024）+ 1100 个键：`agg_overflow>0` 且键数守恒 |
| E-07 | 两个 probe 分挂 veth 两端共写一份 CSV（`--append`）：一条 header、两个 iface、无错行 |

发帧器由 `cargo xtask send --iface IF [--count N]` 暴露，与用例共用 `testkit` 的 AF_PACKET 实现。
每个用例自建唯一命名的 veth 对与 `target/e2e/<case>/` 产物目录，`Drop` 负责清理；断言全在
Rust 里，没有固定 `sleep`（需要对齐窗口边界时用 `clock_gettime(CLOCK_MONOTONIC)`）。

已知坑：

- **陈旧 `target/`**：`cargo xtask test` 报 `cannot start cargo: No such file or directory (os error 2)`
  是 `xtask` 二进制里烙进了旧的 `CARGO_MANIFEST_DIR`（曾为 `/host/probe`），`workspace_root()`
  切目录失败。修法：`touch xtask/src/main.rs && cargo build -p xtask` 后重跑。
- **nextest 可选**：容器未装 `cargo-nextest` 时没有分层 JUnit，`cargo xtask report` 为空；
  需要报告先 `cargo install cargo-nextest --locked`。
- **覆盖率不是门禁**：`scripts/ci/coverage.sh` 目前只产 `target/lcov.info` 并上传，
  `COVERAGE_MIN_LINES` 未设，`main.rs`/`app.rs` 被排除。

CI 怎么分工（`.github/workflows/ci.yml`）：`changes` 用 paths-filter 判断这次改动落在哪一半仓库，
`fast`（fmt/clippy/L0–L2/doc/coverage）只在 Rust 相关路径变化时跑，`bpf` 的范围更窄
（`probe-ebpf/**`、`probe-common/**`、`Cargo.*`、`rust-toolchain*`、`xtask/**`），`gate` **恒执行**，
把 skipped 视作通过——分支保护只需要求 `gate` 一个检查。改 `control-center/**` 走
`control-center.yml`，不触发 Rust 侧；`workflow_dispatch` 视为「全部改过」，所有 job 都跑。

## 4. 代码地图

```
probe-common/src/lib.rs   # 唯一 ABI：SampleEvent / AggKey / AggValue / ProbeConfig + 布局单测（no_std）
probe-ebpf/src/main.rs    # XDP 数据面：parse → CONFIG → per-CPU AGG → 首包样本 → STATS
probe/src/main.rs         # 8 行薄壳，调 probe::app::main()
probe/src/app.rs          # CLI 校验、加载/attach、tokio 主循环（不可进程内测）
probe/src/pipeline.rs     # Session：drain → 关窗 → 写 CSV → 统计（L2 主要被测对象）
probe/src/aggregate.rs    # 纯逻辑：窗口水位线、per-CPU 槽位合并、样本去重、时钟格式化
probe/src/ports.rs        # AggStore / SampleSource / TimeSource 端口 + aya 适配
probe/src/sink.rs         # Event / Sink / AggSink + 两个 CSV 后端 + 工厂
probe/tests/csv_schema.rs # L1：CLI/CSV 列序契约
probe/tests/pipeline.rs   # L2：进程内管线用例
probe/tests/e2e.rs        # L3：特权真机用例（L0–L2 不跑）
testkit/                  # e2e 共用件：AF_PACKET 发帧、veth/命名空间辅助、CSV 断言
xtask/                    # `cargo xtask`：test / bpf / e2e / send / report / soak / coverage
scripts/ci/               # run.sh（CI 环境变量）、fmt.sh、install-toolchain.sh、coverage.sh
verification/             # TEST-PLAN.md、各阶段 REPORT-*.md、run_perf.sh（CSV/日志忽略）
```

端口化边界：内核交互只留在 `app.rs`/`ports.rs`，可测逻辑进 `pipeline`/`aggregate`，
用假 `AggStore`、脚本化 ringbuf、注入时钟。改 `probe-common` 的布局 = 同步改
`README.md` 的 ABI 表 + 布局单测 + 兼容政策；CSV 列序改动 = 同步改 README 与 L1/L2 断言。

## 5. 本机环境相关的注意事项

- `/workspace` 是 bind mount（Windows 9p/drvfs），只保证 `flush(2)` 级别，不额外 `fsync` 每个批次。
- WSL2 下 attach 用 `XdpMode::default()`：本环境 veth 走 driver 模式、`lo` 走 generic。
  目标网卡不支持 native XDP 时要把 `XdpMode` 改为 `Skb`；native 模式未验证。
- AGG 内存随 CPU 数放大（每键 `32 B × CPU 数`）；容器 CPU 数变化会显著改变占用，
  20 vCPU 全量 131072 键约 95 MiB。

## 6. 控制中心（control-center）

`control-center/` 是独立的 Spring + Postgres + SPA + k3d 子项目，入口是它自己的 README，
约定与“还没有的”清单都在那里。构建入口：

```bash
bash control-center/scripts/up.sh        # 构建 + 起 compose 栈 + 等 readiness
bash control-center/scripts/smoke.sh     # 代理、双角色、种子数据
bash control-center/Makefile             # make up/smoke/p95/audit-check/migrate/psql/k3d-up
```

- **宿主 JDK 是 17，项目要 21**（`app/pom.xml:21`）：直接在宿主 `mvn package` 会因 target 21 失败，
  用 `make test`（内部 `docker run maven:3.9-eclipse-temurin-21`）或 `scripts/*.sh`。
- 宿主没有 `make`，Windows 下用 Git Bash 直接调 `scripts/*.sh`。
- 该目录有自己的 workflow `.github/workflows/control-center.yml`：只改 `control-center/**` 时跑
  `mvn -B -ntp -DskipTests package`、`npm ci && npm run build`、`docker compose config -q`、
  `kubectl kustomize deploy/k3d/k8s` 与 `bash -n scripts/*.sh`；四个镜像的 `docker build` 与
  k3d 起集群**不在** CI 里跑，仍在本地 `scripts/*.sh`。

## 7. 文档索引

| 想知道 | 读 |
|---|---|
| 外部用法、架构、map/ABI/CSV 契约、产品级限制 | `README.md` |
| 测试分层设计与用例编号 | `verification/TEST-PLAN.md` |
| S1–S3 分层测试落地结果 | `verification/REPORT-layered-tests.md` |
| S4 真机 e2e 证据 | `verification/REPORT-e2e.md` |
| 聚合验收证据（A2–A7） | `verification/REPORT-aggregation.md` |
| 控制中心边界、部署、迁移 | `control-center/README.md` |
| CI 与工具链 | `.github/workflows/ci.yml`、`control-center.yml`、`image.yml`、`scripts/ci/*.sh` |
