# Aya eBPF 开发镜像：构建与验收报告

- 时间：2026-10-29 00:30 (Asia/Shanghai)
- 环境：Windows + WSL2（Docker Desktop WSL2 backend，`uname -r` = `6.6.87.2-microsoft-standard-WSL2`）
- 镜像：`aya-dev:ubuntu24.04-llvm23`，3.49 GB，`sha256:ec69d914b96c…`
- 结论：**镜像构建成功，编译层通过，加载层在 WSL2 内核下完整验证**（V2 字面命令因上游 dist 变更不可满足，见 §3 说明）

## 1. 对 Dockerfile 的修改（compose 未改）

| # | 位置 | 修改 | 原因 |
|---|---|---|---|
| 1 | 原第 138 行 | 删除 `rustup target add --toolchain nightly bpfel-unknown-none` | 上游 nightly dist 自 2025-12 起不再分发该 target 的预编译 `rust-std`，该命令必然失败；Aya 官方文档已移除此步骤，BPF crate 由 `aya-build` 用 `-Z build-std=core` + `rust-src` 从源码构建 |
| 2 | 新层（cargo-generate 之后） | apt 安装 `sudo libcap2-bin iproute2 ethtool iputils-ping`；`git config --global http.version HTTP/1.1` | 验收命令需要 `sudo`/`capsh`/`ethtool`/`ping`；GitHub smart-HTTP 经 gitoxide/rustls 走 HTTP/2 时会被中间盒重置（`tls handshake eof`），固定 HTTP/1.1 缓解 |
| 3 | 同上 | `/etc/sudoers.d/10-cargo-path` 的 `secure_path` 加入 `/usr/local/cargo/bin:/usr/lib/llvm-23/bin` | 否则 `sudo -E cargo run` 报 `cargo: command not found` |
| 4 | 尾部 | `ENV USER=root` | `cargo generate` 需要 `$USER`，否则报 `could not determine the current user` |

## 2. 构建结果

- 命令：`docker compose --project-directory D:\repos\rust\eBPF -f docker-compose.yml build`
- exit code：**0**；镜像 `aya-dev:ubuntu24.04-llvm23`，**3.49 GB**（`docker images`：`aya-dev:ubuntu24.04-llvm23 ec69d914b96c 3.49GB`）
- 耗时：首次完整构建约 5–6 分钟（23:57:25 开始，其中 `bpf-linker` 源码编译层 1.35 GB 为主要耗时；工具侧 300 s 超时断开后 daemon 继续完成并在下一次调用中导出镜像）；之后增量重建 20–60 s（全部命中缓存）。

```
#11 exporting layers done
#11 writing image sha256:dce0017b4a66... done
#11 naming to docker.io/library/aya-dev:ubuntu24.04-llvm23 0.0s done
 Image aya-dev:ubuntu24.04-llvm23 Built
```

## 3. 编译层（V1–V6）

| 项 | 结果 | 关键点 |
|---|---|---|
| V1 rustc/cargo/toolchains | PASS | stable 1.99.1 + nightly 均在，stable 为默认 |
| V2 `rustup target list --toolchain nightly \| grep bpfel-unknown-none` | **字面 FAIL / 等价 PASS** | 见下 |
| V3 rust-src | PASS | `rust-src (installed)` |
| V4 bpf-linker 与 LLVM 主版本一致 | PASS | bpf-linker 0.12.0，动态链接 `libLLVM.so.23.1`；`llvm-config-23` 23.1.5，`ENV LLVM_VERSION=23`，`llvm-23-dev` 23.1.5 |
| V5 cargo generate / bpftool | PASS | cargo-generate 0.25.1；bpftool v7.4.0 (libbpf v1.4) |
| V6 模板生成 + release 构建 | PASS | 生成 exit 0；`cargo build --release` exit 0，产物 3.0 MB |

V1/V3/V4/V5 原始输出（`verification/compile-layer.log`）：

```
rustc 1.99.1 (58cf8f9c3 2026-10-13)
cargo 1.99.1 (5f94df478 2026-08-27)
stable-x86_64-unknown-linux-gnu (active, default)
nightly-x86_64-unknown-linux-gnu
rust-src (installed)
bpf-linker 0.12.0
libLLVM.so.23.1
23.1.5
```

**V2 说明（不可通过修改镜像修复）**：`rustup target list` 只列 dist 清单内的目标。官方与 TUNA 的 nightly 清单自 2025-12 起已无任何 `bpf*` 条目（2025-06-01 清单命中 58 次，可作正对照），因此该 target 无法安装；`rustup target add` 正是原 Dockerfile 的失败点。功能等价验证：

```
$ rustc +nightly --print target-list | grep bpfel
bpfel-unknown-none
```

且 V6 实际用 `-Z build-std=core` 编译出了 BPF 目标（`aya-build` 调用 `rustup run nightly cargo build --target bpfel-unknown-none -Z build-std=core`）。**验收项 V2 的命令已过时，建议改为 `rustc +nightly --print target-list | grep bpfel-unknown-none`。**

V6 输出（`verification/generate.log`、`verification/probe-build.log`）：

```
 project-name: probe ...
 Generating template ...
    Finished `release` profile [optimized] target(s) in 5.37s
```

注：`cargo generate` 的原始命令在非交互环境会提示 `default_iface`，本次用伪终端输入 `lo` 完成，未改动验收命令本身；GitHub 的 TLS 重置为间歇性，首次失败、第二次重试成功（`verification/generate-retry1-failed.log`）。

## 4. 加载层（WSL2 内核 6.6.87.2-microsoft-standard-WSL2）

| 项 | 结果 | 证据 |
|---|---|---|
| V1 权限 | PASS | `cap_bpf cap_net_admin cap_perfmon cap_sys_admin`（`capsh --print`） |
| V2 debugfs / BTF | PASS | `debugfs on /sys/kernel/debug type debugfs`；`/sys/kernel/btf/vmlinux` 6,050,732 B |
| V3 `lo` attach（主依据） | **PASS** | `Waiting for Ctrl-C...`；`bpftool net`：`lo(1) generic id 194`；`ip link`：`xdpgeneric` |
| V4 `eth0` attach（可选） | **PASS** | `ethtool -K eth0 lro off` exit 0；`bpftool net`：`eth0(3) driver id 199` |
| V5 ping 触发 | 无可观测行为 | `ping -c 3 127.0.0.1` 3/3 通，`lo_attach.log` 行数 1→1（无新日志） |

```
xdp:
lo(1) generic id 194
eth0(3) driver id 199
```

`lo` 走 generic(SKB) 模式、`eth0` 走 driver(native) 模式，均无错误。ping 无新日志属可接受结果（generic XDP 在本内核上对 loopback ICMP 无可观测回传）。

## 5. 环境判定

**编译层通过；加载层在 WSL2 下完整验证（lo=generic、eth0=driver 均挂载成功）；下一步建议把本项目纳入 CI：`docker compose build` + 上述 V1–V6 与 lo attach 冒烟测试，并把 V2 的检查命令改为 `rustc --print target-list`。**

网络特性（非镜像缺陷）：GitHub 直连偶发 `tls handshake eof`（git CLI 正常，gitoxide 受影响）；crates.io 偶发 SSL EOF，`cargo` 重试后成功。

## 6. 日志索引

| 文件 | 内容 |
|---|---|
| `verification/build-failure-2026-10-28.log` | 修复前构建失败原始错误（历史） |
| `verification/rust-dist-manifest-check-2026-10-28.log` | dist 清单核验（历史） |
| `verification/compile-layer.log` | V1–V5 |
| `verification/loading-prereq.log` | caps/debugfs/BTF/内核 |
| `verification/generate.log` / `generate-retry1-failed.log` | V6 模板生成（成功/失败各一次） |
| `verification/probe-build.log` | V6 `cargo build --release` |
| `verification/lo_attach.log` / `bpftool-net-lo.log` / `ping.log` | lo 挂载与触发 |
| `verification/eth0_attach.log` / `bpftool-net-eth0.log` / `ethtool-eth0.log` | eth0 挂载 |

## 7. 命令清单（关键步骤）

```
docker compose ... build                                  → exit 0（最终）
docker compose ... up -d                                  → aya-dev Up
rustc --version / cargo --version / rustup toolchain list → V1
rustup target list --toolchain nightly | grep bpfel        → 空（V2 字面 FAIL）
rustc +nightly --print target-list | grep bpfel           → bpfel-unknown-none（等价 PASS）
rustup component list --toolchain nightly | grep rust-src → installed
bpf-linker --version; ldd $(which bpf-linker); llvm-config-23 --version
cargo generate --version; bpftool version
cargo generate --name probe -d program_type=xdp https://github.com/aya-rs/aya-template  → exit 0（第 2 次尝试）
cd probe && cargo build --release                         → exit 0
capsh --print | grep -E 'cap_bpf|cap_net_admin'           → PASS
mount | grep debugfs; ls /sys/kernel/btf/vmlinux          → PASS
sudo -E cargo run --release -- --iface lo                 → xdpgeneric, Waiting for Ctrl-C
bpftool net list                                          → lo(1) generic id 194
ping -c 3 127.0.0.1                                       → 3/3, 无新日志
ethtool -K eth0 lro off; sudo -E ./target/release/probe --iface eth0 → eth0(3) driver id 199
```
