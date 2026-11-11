# XDP IPv4 源/目的地址采集 — 实现与验证报告

- 时间：2026-10-30 19:16 (Asia/Shanghai) / 容器日志为 UTC 11:16
- 环境：`aya-dev:ubuntu24.04-llvm23`，容器 `aya-dev`，`uname -r` = `6.6.87.2-microsoft-standard-WSL2`
- 结论：**PASS**。`cargo build --release` exit 0，XDP 在 veth 上以 driver 模式加载无 verifier 报错；
  双向 ICMP 与 VLAN ICMP 均写出正确的 CSV；`bpftool` 证明 attach 与 `run_cnt` 增长；
  ping 3/3 通过；SIGINT/SIGTERM 后数据已 flush；`--append` 不覆盖、不重复表头；清理后无残留 XDP/进程。

## 1. 改动

| 文件 | 改动 |
|---|---|
| `probe/probe-common/src/lib.rs` | 新增 `PacketEvent`（`#[repr(C)]`, 24 B），含 `ts_ns/src_ip/dst_ip/src_port/dst_port/protocol/reserved`；`#[cfg(feature="user")] unsafe impl aya::Pod`；布局单测 |
| `probe/probe-ebpf/src/main.rs` | Ethernet → 最多两层 802.1Q/802.1ad → IPv4 解析；边界检查；`EVENTS` ringbuf；`DROPS` per-CPU 计数；始终 `XDP_PASS`；去掉 per-packet `info!` |
| `probe/probe/src/main.rs` | 新 CLI `--out/--sink/--flush-ms/--append`；ringbuf 轮询 + 批量写；Ctrl-C/SIGTERM 最终排空；退出统计；显式 detach |
| `probe/probe/src/sink.rs` | `Event`、`Sink` trait、`CsvSink`、`create_sink` 工厂；CSV 表头与 7 列 |
| `probe/probe/Cargo.toml` | 给 tokio 增加 `time` feature（`tokio::time::interval`）；给 clap 增加 `help/usage/error-context`（`--help` 可用）；未新增 crate |
| `probe/README.md` | 用法、CSV 语义、`--append`、存储扩展步骤、限制 |
| `verification/run_e2e.sh` | 可重复执行的端到端验证脚本（本次新增） |

未修改 `Dockerfile`、`docker-compose.yml`，未新增 `csv` 等依赖（CSV 用 `std::io::Write` 直接写）。
事件 ABI 与存储后端解耦：新增 `DbSink` 只需改 `probe/src/sink.rs` 和 `SinkKind` 工厂分支。

### 事件 ABI（`probe/probe-common/src/lib.rs`）

| offset | 字段 | 类型 | 说明 |
|---:|---|---|---|
| 0 | `ts_ns` | `u64` | `bpf_ktime_get_ns()`，CLOCK_MONOTONIC |
| 8 | `src_ip` | `u32` | IPv4 `saddr` 原样（网络序），用户态 `from_be` 一次 |
| 12 | `dst_ip` | `u32` | IPv4 `daddr` 原样 |
| 16 | `src_port` | `u16` | 预留，L4 未解析时为 0 |
| 18 | `dst_port` | `u16` | 预留 |
| 20 | `protocol` | `u8` | 本阶段已填充（ICMP=1） |
| 21..24 | `reserved` | `[u8;3]` | 显式填充/未来标志位 |

`size_of = 24`, `align_of = 8`，单测 `event_layout_is_stable` 通过。

## 2. 构建与静态检查

```text
$ cargo build --release
exit=0
```

完整日志：`verification/build.log`。构建无 warning（首次构建曾有一个未使用 import 和
`Event` 字段未读的 warning，已分别删除/改为按字段写 CSV 消除）。

```text
$ cargo test -p probe-common --release
test tests::event_layout_is_stable ... ok
test result: ok. 1 passed; 0 failed
```

镜像未安装 clippy/rustfmt 组件（`cargo-clippy`/`cargo-fmt` 不可用），因此本次未运行；
build 本身无编译告警。

### 2.1 rust-analyzer: duplicate lang item `panic_impl`（E0152）

rust-analyzer 曾对 `probe-ebpf/src/main.rs` 报：

```text
found duplicate lang item `panic_impl`
the lang item is first defined in crate `std` (which `aya` depends on)
second definition in the local crate (`probe`)
```

原因不是 eBPF 代码有错，而是两套构建视图的差异：

- 真正的 eBPF 构建由 `build.rs` 单独调用 `cargo build -p probe-ebpf --target bpfel-unknown-none`
  （`-Z build-std=core`），此时没有 std，crate 里的 `#[panic_handler]` 是必需的。
- rust-analyzer / `cargo check -p probe -p probe-ebpf` 按 **host target + workspace 特征统一**
  分析：`probe` 启用了 `probe-common/user` → `aya` → `std`，std 已经提供
  `panic_impl`，本地 crate 再定义一个就触发 E0152。

修复：把 panic handler 限定到真正的 BPF 目标（该目标的 `target_arch` 为 `bpf`）：

```rust
#[cfg(all(not(test), target_arch = "bpf"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! { loop {} }
```

验证：

```text
$ cargo check -p probe -p probe-ebpf     # 修复前 E0152，修复后 exit 0
$ cargo build --release                  # exit 0
```

host 侧检查日志：`verification/check-host.log`。

重新构建后的 eBPF 对象 `tag` 仍为 `4a517a161d3deb1b`、`xlated 760B`，与修复前一致，
说明该 cfg 只影响 host 分析视图，不改变数据面程序；attach 冒烟测试见
`attach-after-panic-fix.log`、`bpftool-prog-after-panic-fix.log`。

## 3. 端到端验证

脚本：`verification/run_e2e.sh`；汇总日志：`verification/e2e.log`。
拓扑：`veth0`（host netns，10.99.0.1/24）↔ `veth1`（`aya-ns` netns，10.99.0.2/24），
XDP 分别 attach 到两端；用 ping 制造确定性的双向 ICMP。脚本最后删除 veth/netns 并检查无残留。

### 3.1 挂载证据（bpftool）

```text
$ bpftool net list                      # phase A
xdp:
veth0(110) driver id 826

$ bpftool prog show id 826
826: xdp  name probe  tag 4a517a161d3deb1b  gpl run_time_ns 31732 run_cnt 9
        loaded_at 2026-10-30T11:16:26+0000  uid 0
        xlated 760B  jited 526B  memlock 4096B  map_ids 212,211

$ bpftool net list                      # phase B（在 aya-ns 内执行）
xdp:
veth1(109) driver id 831

$ bpftool prog show id 831
831: xdp  name probe  tag 4a517a161d3deb1b  gpl run_time_ns 14798 run_cnt 4
        loaded_at 2026-10-30T11:16:30+0000  uid 0
        xlated 760B  jited 526B  memlock 4096B  map_ids 217,216
```

- phase A `run_cnt`：2 → 9（attach 后到 ping 前已有 2 次入口包，ping 后增长 7）
- phase B `run_cnt`：无计数（0）→ 4
- 加载时无 verifier 错误；`xlated 760B` 说明程序被 kernel 接受。
- 原文件：`bpftool-net-A.log`、`bpftool-prog-A-before/after.log`、
  `bpftool-net-B.log`、`bpftool-prog-B-before/after.log`。
- 另有一次 `RUST_LOG=info` 的纯 attach 运行（无流量）证明加载/卸载路径，
  日志 `attach-info.log`：

  ```text
  [INFO  probe] attached XDP program to 'veth0' (link id XdpLinkId(FdLinkId(FdLinkId(13))))
  [INFO  probe] persisting to /workspace/verification/attach-only.csv (append=false)
  Waiting for Ctrl-C...
  [INFO  probe] Ctrl-C received, draining ring buffer
  stats: events_received=0 rows_written=0 kernel_dropped=0
  ```

  该次的 `attach-only.csv` 只有表头，`bpftool-net-attach-only.log` 中 `veth0(106) driver id 816`。

### 3.2 流量不被破坏

```text
# phase A: ip netns exec aya-ns ping -c 3 10.99.0.1
3 packets transmitted, 3 received, 0% packet loss
# phase B: ping -c 3 10.99.0.2
3 packets transmitted, 3 received, 0% packet loss
# phase C (VLAN): ip netns exec aya-ns ping -c 3 10.100.0.1
3 packets transmitted, 3 received, 0% packet loss
```

日志：`ping-ns-to-host.log`、`ping-host-to-ns.log`、`ping-vlan.log`。`XDP_PASS` 生效。

### 3.3 CSV 校验

phase A 写 3 行，phase B 用 `--append` 追加 3 行，最终 `verification/packets.csv`：

```csv
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
1793358987411,veth0,10.99.0.2,10.99.0.1,,,1
1793358988429,veth0,10.99.0.2,10.99.0.1,,,1
1793358989453,veth0,10.99.0.2,10.99.0.1,,,1
1793358991273,veth1,10.99.0.1,10.99.0.2,,,1
1793358992301,veth1,10.99.0.1,10.99.0.2,,,1
1793358993325,veth1,10.99.0.1,10.99.0.2,,,1
```

- A 侧（veth0 ingress）只看到 `10.99.0.2 → 10.99.0.1`，B 侧（veth1 ingress）只看到反向，
  与实际 ICMP 请求/响应方向一致。
- 6 行数据、1 个表头；`src_port/dst_port` 为空字段，`protocol=1`（ICMP）。
- 时间戳与当前时间差 6.7 s（脚本内检查 < 300 s），说明 ktime→Unix 换算正确。
- ARP / IPv6 邻居帧没有进入 CSV（全部行 `protocol == 1`），验证了非 IPv4 的 `XDP_PASS` 分支。

VLAN（phase C）`verification/packets-vlan.csv`：

```csv
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
1793358995226,veth0,10.100.0.2,10.100.0.1,,,1
1793358996227,veth0,10.100.0.2,10.100.0.1,,,1
1793358997229,veth0,10.100.0.2,10.100.0.1,,,1
```

说明 802.1Q 分支确实按 `l3_offset += 4` 解析到了 IPv4，而不是把 VLAN 头当 IP。

### 3.4 退出、flush 与 `--append`

```text
$ cat probe-veth0.log
Waiting for Ctrl-C...
stats: events_received=3 rows_written=3 kernel_dropped=0

$ cat probe-veth1.log
Waiting for Ctrl-C...
stats: events_received=3 rows_written=3 kernel_dropped=0
```

- 发送 SIGINT 后进程先排空 ringbuf、`flush`，再输出统计，然后 detach；文件内容完整。
- 另测 SIGTERM：`--flush-ms 1000`、ping 3 个包后立即 `kill -TERM`，进程仍进入退出路径并
  把 3 行写盘（`sigterm.log` / `sigterm.csv`）：

  ```text
  [INFO  probe] SIGTERM received, draining ring buffer
  stats: events_received=3 rows_written=3 kernel_dropped=0
  ```

- phase A 无 `--append`（文件不存在：截断/新建）；phase B 使用 `--append`，3 → 6 行，
  表头仍只有 1 个，证明重启追加语义。
- `kernel_dropped=0`（本轮流量没有填满 256 KiB ringbuf；per-CPU 丢弃计数在退出时求和打印）。

### 3.5 清理

```text
$ bpftool net list
xdp:

tc:
...
$ pgrep -x probe       # 无输出
```

`verification/bpftool-net-final.log` 中 `xdp:` 为空；脚本检查 “no XDP attachment left” 与
“no probe process left” 均 PASS。

## 4. 关键实现点

- **边界检查**：`ptr_at<T>` 统一做 `start + offset + size > data_end` 检查；
  IPv4 还检查 `version == 4`、`ihl >= 5`，并按 `ihl*4` 验证可选头完整，为后续 L4
  (`l3_offset + ihl*4`) 留出正确偏移。
- **VLAN**：支持 0x8100 / 0x88A8，最多两层；非 IPv4（如 ARP）直接 `XDP_PASS`。
- **ringbuf**：`EVENTS.reserve::<PacketEvent>(0)`，`entry.write` + `submit`；
  `None` 时只递增 `DROPS` 并返回 `XDP_PASS`，绝不阻塞/panic。
- **字节序**：内核 `saddr/daddr` 原样写入；用户态 `Ipv4Addr::from(u32::from_be(...))` 仅一次，
  CSV 中地址与流量一致（未见翻转）。
- **扩展性**：端口字段已在 ABI 中；CSV 端口列在本阶段为空，L4 打开后无需改 schema；
  `Sink` 工厂让 DB 后端与 eBPF 完全解耦。
- **退出路径**：Ctrl-C/SIGTERM 后 `drain_ring` 循环排空所有已入队事件；`Sink::flush` +
  `CsvSink::Drop` 双重兜底；`program.detach(link_id)` 显式卸载，Ebpf Drop 再兜底。

## 5. 已知限制

- 仅 IPv4、仅 ingress；一个进程 attach 一个接口。要同时观察 veth 两端需运行两个进程
  （其中一端在 netns 内），本次 CSV 即用 `--append` 合并。
- L4 未解析：`src_port/dst_port` 恒为 0/空；`protocol` 已真实填充。
- ringbuf 256 KiB，用户态按 `--flush-ms` 轮询；极端突发会丢事件并计入 `DROPS`
  （per-CPU 槽位在退出时求和，属于诊断值；各 CPU 独立递增，无共享读改写）。
- CSV 追加依赖单写者或本地文件系统 `O_APPEND`；没有跨进程锁。
- 只 `flush(2)`，未对每个批次 `fsync`；崩溃时最多丢失用户态缓冲区内未 flush 的数据。
- WSL2 下 `lo` 只能 generic XDP；veth 实测 driver 模式正常。其他网卡若不支持 native，
  需要将 `XdpMode::default()` 改为 `XdpMode::Skb`。

## 6. 复现

```bash
cd /workspace/probe && cargo build --release
bash /workspace/verification/run_e2e.sh
```

脚本末尾输出 `RESULT: PASS`，退出码 0；所有日志/CSV 在 `/workspace/verification/`。
