# probe — XDP IPv4/IPv6 源/目的地址采集

在 XDP ingress 解析 Ethernet →（可选 802.1Q/802.1ad VLAN）→ IPv4 **或** IPv6，
把 `src_addr` / `dst_addr` / `protocol` 通过 ringbuf 送到用户态，由用户态按
`--sink` 选择的后端持久化。当前实现对 CSV 后端；事件结构已经预留
`src_port` / `dst_port`，后续解析 L4 时内核 ↔ 用户态 ABI 不变。

IPv4 与 IPv6 共用同一个 48 字节事件（ABI v2）和同一个 ringbuf。用户态按
`family` 解码：IPv4 输出点分十进制，IPv6 输出标准压缩格式（如 `fd00:99::2`）。

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
probe-common/src/lib.rs   # PacketEvent ABI v2（IPv4+IPv6，#[repr(C)]、aya::Pod）
probe-ebpf/src/main.rs    # XDP 解析（VLAN -> IPv4/IPv6）+ ringbuf + 丢弃计数
probe/src/main.rs         # CLI、attach、ringbuf 轮询、统计
probe/src/sink.rs         # Event（IpAddr）+ Sink trait + CsvSink + 工厂
```

## 事件 ABI v2（破坏性升级）

`probe-common/src/lib.rs` 中的 `PacketEvent` 由 eBPF 侧原样写入 ringbuf，用户态用
`read_unaligned` 读回。v2 用同一个结构承载 IPv4 与 IPv6：

| offset | 字段 | 类型 | 说明 |
|---:|---|---|---|
| 0 | `ts_ns` | `u64` | `bpf_ktime_get_ns()` |
| 8 | `src_addr` | `[u8;16]` | 网络序原始字节；IPv4 左对齐、其余补 0 |
| 24 | `dst_addr` | `[u8;16]` | 同上 |
| 40 | `src_port` | `u16` | 预留，L4 未解析时为 0 |
| 42 | `dst_port` | `u16` | 预留 |
| 44 | `protocol` | `u8` | IPv4 protocol / IPv6 Next Header |
| 45 | `family` | `u8` | 4 = IPv4，6 = IPv6 |
| 46 | `version` | `u8` | `EVENT_ABI_VERSION`（当前 2） |
| 47 | `reserved` | `u8` | 恒为 0 |

`size_of == 48`、`align_of == 8`；`probe-common` 的布局单测逐字段断言
`offset_of!`，防止 ABI 静默漂移。

地址语义：

- 一律保存**网线上的原始字节**，不做 `from_be`，也不做 IPv4-mapped `::ffff:`。
- IPv4：`family = 4`，`src_addr[0..4]` 是 IPv4 的 4 个字节，`src_addr[4..16]` 全 0；
  用户态用 `Ipv4Addr::new(a, b, c, d)`。
- IPv6：`family = 6`，完整 16 字节；用户态用 `Ipv6Addr::from(bytes)`。

**这是有意的破坏性升级。** v1 是 24 字节的 IPv4 专用结构（`src_ip` / `dst_ip: u32`），
已删除，没有双事件或双 ringbuf。**旧用户态二进制不能读取新的 ringbuf 记录**：
升级到 IPv6 版本必须同时重新编译 eBPF 对象和用户态。用户态会校验 `version` 与
`family`，不匹配的记录跳过并计入 `malformed`（退出统计里可见），不会按错误布局
强行解析。CSV 列没有变化（仍 7 列），所以旧 IPv4 CSV 仍可用 `--append` 继续写。

## 解析路径

```
Ethernet(14B) → [802.1Q/802.1ad ×2] → ethertype ─┬─ 0x0800 → IPv4(固定头) ─┐
                                                 └─ 0x86DD → IPv6(40B base)─┤
                                                        其它 → XDP_PASS     │
                                                                            ▼
                                                              PacketEvent → EVENTS ringbuf
```

- VLAN 解析与之前一致：最多两层，内层 ethertype 决定 IPv4/IPv6；`0x8100 → 0x86DD`
  的 IPv6 over VLAN 路径同样支持。
- IPv4 继续校验 version/ihl，并确认 `l3_offset + ihl*4` 在包内（为将来 L4 预留）；
  地址按 4 个原始字节左对齐拷贝到 16 字节字段，其余补 0。
- IPv6 只解析固定 40 字节 base header（不读 payload）：先用 `ptr_at` 证明
  `[l3_offset, l3_offset+40)` 在 `[data, data_end)` 内，再检查首字节高 4 位 == 6，
  把 `next_header` 当作 `protocol`，`saddr`/`daddr` 16 字节原样拷贝；截断包直接
  `XDP_PASS`。
- 所有路径都返回 `XDP_PASS`；ringbuf `reserve` 失败只递增当前 CPU 的 `DROPS[0]`，
  不阻塞、不 panic。数据面无 per-packet 日志、无动态分配。

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
两种模式都以 `O_APPEND` 打开文件（非 `--append` 时先 `set_len(0)`），所以两个进程
（例如 veth 两端各一个）可以并发写同一个 CSV，不会互相覆盖行；每条 flush 是一次
`O_APPEND` 写，行不会被拆开，仅跨进程的行序不确定。

程序不会对每个包打日志。退出（Ctrl-C / SIGINT 或 SIGTERM）时会先排空 ringbuf、
写盘、flush，再 detach XDP，最后打印一行统计：

```
stats: events_received=6 rows_written=6 kernel_dropped=0 malformed=0
```

`malformed` 是 `version != EVENT_ABI_VERSION` 或 `family` 非 4/6 而被跳过的记录数；
正常情况下为 0。

`RUST_LOG=info` 可看到 attach 等控制面日志；数据面完全不依赖 `env_logger`。

## CSV 格式

固定 7 列（顺序固定）：

```
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
```

- `ts_unix_ms`：用户态用启动时采样的 `CLOCK_MONOTONIC` / `SystemTime` 偏移，把
  内核 `bpf_ktime_get_ns()` 换算成 Unix 毫秒；不是每包 syscall。
- `src_ip` / `dst_ip`：IPv4 点分十进制；IPv6 为标准压缩格式（RFC 5952，如
  `fd00:99::2`）。内核原样保存网线上的字节，用户态用 `Ipv4Addr::new` /
  `Ipv6Addr::from` 解码，不做字节序转换，也不会把 `fe80::` 误写成 IPv4。
- `src_port` / `dst_port`：本阶段未解析 L4，恒为空字段（两个连续逗号）。eBPF
  事件里字段已存在且为 0；将来填上端口后 CSV 自动出现数字，列位置不变。
- `protocol`：IPv4 协议号（1 = ICMP，6 = TCP，17 = UDP）或 IPv6 **base header 的
  Next Header**（58 = ICMPv6，6 = TCP，17 = UDP）。IPv6 扩展头本阶段不遍历：
  遇到 Hop-by-Hop(0)/Routing(43)/Fragment(44)/DestOpts(60)/AH(51) 时，该列可能是
  扩展头编号而不是最终 L4 协议（例如 MLDv2 报告会显示 `0`）。

示例（IPv6 双向 ICMPv6、一条 `fe80::` 控制流量、一条 IPv4）：

```csv
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
1793452928885,veth0,fd00:99::2,fd00:99::1,,,58
1793452929891,veth0,fd00:99::2,fd00:99::1,,,58
1793452930893,veth0,fd00:99::2,fd00:99::1,,,58
1793452928884,veth1,fd00:99::1,fd00:99::2,,,58
1793452929890,veth1,fd00:99::1,fd00:99::2,,,58
1793452930891,veth1,fd00:99::1,fd00:99::2,,,58
1793452928195,veth0,fe80::cf7:5eff:feac:8587,ff02::16,,,0
1793452940874,veth0,10.99.0.2,10.99.0.1,,,1
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
   `src_ip` / `dst_ip`（`std::net::IpAddr`，同时覆盖 IPv4/IPv6）、`src_port` /
   `dst_port` / `protocol`，L4 打开后直接可用。

## 验证

仓库根目录有两个可重复执行的端到端脚本，都使用真实 veth + netns，不做 mock：

- `verification/run_e2e.sh`：IPv4 回归。创建 netns + veth，两端分别 attach，
  双向 ping，并检查 CSV、`bpftool net list`、`bpftool prog show` 的 `run_cnt`、
  `--append` 语义、VLAN 解析和清理结果。该脚本在 IPv4 phase 用
  `net.ipv6.conf.<if>.disable_ipv6=1` 关掉 veth 上的 IPv6，以保证“恰好 3 行”的
  精确断言；这是测试隔离，不是功能限制。
- `verification/run_e2e_ipv6.sh`：IPv6 端到端。veth 两端写同一个 CSV（第二个
  `--append`），双向 `ping -6`，校验 `fd00:99::2 → fd00:99::1` 与其反向行、
  `protocol=58`、`--append` 不重复表头、`bpftool` attach 与 `run_cnt`、
  SIGINT/SIGTERM flush、IPv6 over VLAN 100、以及一次运行里 IPv4/IPv6 地址并存。
  所有断言都按 ULA 前缀过滤，容忍 `fe80::`/`ff02::` 的 ND/MLD 控制流量。

本次验证日志/CSV 在 `verification/`（IPv6 证据以 `ipv6-` 开头），报告见
`verification/REPORT-xdp-ipv4.md` 和 `verification/REPORT-xdp-ipv6.md`。

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

IPv6 版本（ULA 前缀，避免 link-local scope 问题）：

```bash
ip netns add aya-ns
ip link add veth0 type veth peer name veth1
ip link set veth1 netns aya-ns
ip -6 addr add fd00:99::1/64 dev veth0 nodad && ip link set veth0 up
ip netns exec aya-ns ip link set lo up
ip netns exec aya-ns ip -6 addr add fd00:99::2/64 dev veth1 nodad
ip netns exec aya-ns ip link set veth1 up

sudo -E ./target/release/probe --iface veth0 --out /workspace/packets-ipv6.csv --flush-ms 100 &
ip netns exec aya-ns ping -6 -c 3 fd00:99::1
sleep 0.5
kill -INT %1

grep 'fd00:99::2,fd00:99::1' /workspace/packets-ipv6.csv   # protocol 列应为 58
bpftool net list

ip link del veth0
ip netns del aya-ns
```

## 已知限制

- **IPv6 扩展头不遍历**：只读固定 40 字节 base header，`protocol` 取 Next Header。
  遇到 Hop-by-Hop(0)/Routing(43)/Fragment(44)/DestOpts(60)/AH(51) 时该列可能不是
  最终 L4 协议（例如 MLDv2 报告显示 `0`，实际载荷是 ICMPv6）；需要最终协议时应
  在用户态或后续版本里按扩展头链继续解析。
- 仅采集 IP 帧：ARP、非 IP 帧直接 `XDP_PASS`，不产生事件。
- 仅 ingress；一个进程只 attach 一个 `--iface`。要同时看 veth 两端，运行两个
  进程（一个在 netns 内），可用 `--append` 写同一文件。两个进程都以 `O_APPEND`
  写，行不会被覆盖；仅跨进程的行序不确定。
- IPv6 开启后 veth/物理网卡会自动产生 `fe80::`、`ff02::` 的 ND/MLD/ICMPv6 包，
  这些行是正常的控制流量。用 ULA 或具体地址过滤后再断言，不要用总行数做精确断言。
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
