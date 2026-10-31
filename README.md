# probe — XDP IPv4 源/目的地址采集

在 XDP ingress 解析 Ethernet →（可选 802.1Q/802.1ad VLAN）→ IPv4，把
`src_ip` / `dst_ip` / `protocol` 通过 ringbuf 送到用户态，由用户态按
`--sink` 选择的后端持久化。当前实现对 CSV 后端；事件结构已经预留
`src_port` / `dst_port`，后续解析 L4 时内核 ↔ 用户态 ABI 不变。

```
NIC / veth ingress
        │
        ▼
probe-ebpf (XDP) ── reserve/submit ──▶ EVENTS ringbuf ──▶ probe (userspace)
        │                                                   │
        └── reserve 失败 ──▶ DROPS[0] (per-CPU)              └──▶ Sink trait ──▶ CsvSink ──▶ CSV
```

## 目录

```
probe-common/src/lib.rs   # PacketEvent ABI（#[repr(C)]、unsafe impl aya::Pod）
probe-ebpf/src/main.rs    # XDP 解析 + ringbuf + 丢弃计数
probe/src/main.rs         # CLI、attach、ringbuf 轮询、统计
probe/src/sink.rs         # Event + Sink trait + CsvSink + 工厂
```

## 构建与运行

```bash
cd /workspace/probe
cargo build --release

# 默认 /workspace/packets.csv，500 ms 批量 flush
sudo -E ./target/release/probe --iface veth0

# 指定输出、周期，并追加到已有文件
sudo -E ./target/release/probe \
    --iface veth0 \
    --sink csv \
    --out /workspace/packets.csv \
    --flush-ms 200 \
    --append
```

`cargo run` 也可用（`.cargo/config.toml` 里 `runner = "sudo -E"`），但验证时建议
先 `cargo build --release` 再直接执行二进制，避免 sudo 环境差异触发重建。

### rust-analyzer 的 E0152

`probe-ebpf` 的 panic handler 只在真正的 BPF 目标上定义：

```rust
#[cfg(all(not(test), target_arch = "bpf"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! { loop {} }
```

原因是 `probe` 启用了 `probe-common/user` → `aya` → `std`；当 rust-analyzer 或
`cargo check -p probe -p probe-ebpf` 以 host target + workspace 特征统一去分析
`probe-ebpf` 时，std 已经提供 `panic_impl`，再定义一次就会报
“found duplicate lang item `panic_impl`”。真实 eBPF 构建使用
`--target bpfel-unknown-none` + `-Z build-std=core`，没有 std，仍然需要该 handler。
`target_arch = "bpf"` 正好区分这两种视图，不改变生成的 eBPF 对象。

### 参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--iface <dev>` | `lo` | attach 的网卡；保留原参数 |
| `--out <path>` | `/workspace/packets.csv` | 输出文件；父目录会自动创建 |
| `--sink <csv>` | `csv` | 存储后端；`db` 为将来预留（新增后端见下） |
| `--flush-ms <n>` | `500` | 轮询 ringbuf / 批量 flush 的周期，必须 > 0 |
| `--append` | 关闭 | 追加而不是截断；文件非空时不重复写表头 |

不指定 `--append` 时，启动会截断 `--out` 并重写表头；指定 `--append` 时保留
旧内容，仅在文件长度 == 0 时写表头。重启语义因此是显式的，不会静默覆盖历史数据。

程序不会对每个包打日志。退出（Ctrl-C / SIGINT 或 SIGTERM）时会先排空 ringbuf、
写盘、flush，再 detach XDP，最后打印一行统计：

```
stats: events_received=6 rows_written=6 kernel_dropped=0
```

`RUST_LOG=info` 可看到 attach 等控制面日志；数据面完全不依赖 `env_logger`。

## CSV 格式

固定 7 列（顺序固定）：

```
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
```

- `ts_unix_ms`：用户态用启动时采样的 `CLOCK_MONOTONIC` / `SystemTime` 偏移，把
  内核 `bpf_ktime_get_ns()` 换算成 Unix 毫秒；不是每包 syscall。
- `src_ip` / `dst_ip`：IPv4 点分十进制。内核原样保存网络序，用户态只做一次
  `u32::from_be`，不会出现地址翻转。
- `src_port` / `dst_port`：本阶段未解析 L4，恒为空字段（两个连续逗号）。eBPF
  事件里字段已存在且为 0；将来填上端口后 CSV 自动出现数字，列位置不变。
- `protocol`：IPv4 协议号（1 = ICMP，6 = TCP，17 = UDP），本阶段已填充。

示例（ping 的 ICMP，两个方向 + VLAN 各一次）：

```csv
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
1793358693268,veth0,10.99.0.2,10.99.0.1,,,1
1793358694297,veth0,10.99.0.2,10.99.0.1,,,1
1793358695321,veth0,10.99.0.2,10.99.0.1,,,1
1793358697134,veth1,10.99.0.1,10.99.0.2,,,1
1793358698136,veth1,10.99.0.1,10.99.0.2,,,1
1793358699160,veth1,10.99.0.1,10.99.0.2,,,1
```

## 存储后端如何扩展（例如 DB）

`probe/src/sink.rs` 中是唯一的存储抽象：

```rust
pub trait Sink: Send {
    fn write_batch(&mut self, events: &[Event]) -> anyhow::Result<()>;
    fn flush(&mut self) -> anyhow::Result<()>;
}
```

新增 DB 后端的步骤：

1. 新建 `DbSink` 并实现 `Sink`（连接池、批量 insert 等都在这里）。
2. 在 `SinkKind` 加 `Db` 变体，在 `create_sink` 加一个 match 分支和
   `DbSink::connect(...)`。
3. 不需要改 `probe-ebpf`、`probe-common` 或 `PacketEvent`；`Event` 已经带有
   `src_port` / `dst_port` / `protocol`，L4 打开后直接可用。

## 验证

仓库根目录的 `verification/run_e2e.sh` 是可重复执行的端到端脚本：创建
netns + veth，两端分别 attach，双向 ping，并检查 CSV、`bpftool net list`、
`bpftool prog show` 的 `run_cnt`、`--append` 语义、VLAN 解析和清理结果。
本次验证日志在 `verification/`，报告见
`verification/REPORT-xdp-ipv4.md`。

快速手工验证：

```bash
cd /workspace/probe
cargo build --release

ip netns add aya-ns
ip link add veth0 type veth peer name veth1
ip link set veth1 netns aya-ns
ip addr add 10.99.0.1/24 dev veth0 && ip link set veth0 up
ip netns exec aya-ns ip link set lo up
ip netns exec aya-ns ip addr add 10.99.0.2/24 dev veth1
ip netns exec aya-ns ip link set veth1 up

sudo -E ./target/release/probe --iface veth0 --out /workspace/packets.csv --flush-ms 100 &
ip netns exec aya-ns ping -c 3 10.99.0.1
sleep 0.5
kill -INT %1

bpftool net list
cat /workspace/packets.csv

ip link del veth0
ip netns del aya-ns
```

## 已知限制

- 仅 IPv4；IPv6、ARP、非 IP 帧直接 `XDP_PASS`，不产生事件。
- 仅 ingress；一个进程只 attach 一个 `--iface`。要同时看 veth 两端，运行两个
  进程（一个在 netns 内），可用 `--append` 写同一文件；并发写没有额外锁，
  依赖本地文件系统的 `O_APPEND` 原子性。
- L4 端口尚未解析（CSV 留空、事件中为 0）；`protocol` 已填充。
- ringbuf 固定 256 KiB。用户态按 `--flush-ms` 轮询；极高速突发可能填满
  ringbuf，此时内核侧丢包并累加 `DROPS`，不会阻塞数据面。退出统计中的
  `kernel_dropped` 是所有 CPU 槽位之和。
- `/workspace` 是 bind mount（Windows 9p/drvfs），flush 后宿主机可见；这里只
  保证 `flush(2)` 级别，不额外 `fsync` 每个批次。
- WSL2 下 attach 使用 `XdpMode::default()`；本环境 veth 走 driver 模式，`lo`
  走 generic。若目标网卡不支持 native XDP，需要把 `XdpMode` 改为 `Skb`
  （generic）。

## 许可

与模板一致：用户态 MIT OR Apache-2.0，eBPF 代码 GPL-2.0 OR MIT。详见
`LICENSE-*` 文件。
