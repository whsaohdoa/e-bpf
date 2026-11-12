# XDP IPv6 源/目的地址采集验证报告（ABI v2）

- 时间：2026-10-31 21:18–21:23 (Asia/Shanghai)，容器日志为 UTC 13:18–13:23
- 环境：`aya-dev:ubuntu24.04-llvm23`，容器 `aya-dev`，`uname -r` =
  `6.6.87.2-microsoft-standard-WSL2`，BTF 存在，`iputils-ping` 支持 `-6`
- 结论：**PASS**。真实 veth + netns + `ping -6` 端到端验证通过；
  `cargo build --release`、`cargo test -p probe-common --release`、
  `cargo test -p probe --release`、`cargo check -p probe -p probe-ebpf` 全部 exit 0；
  IPv4 回归 PASS（25 项断言，0 失败）；IPv6 验证 PASS（34 项断言，0 失败）；
  无 verifier 错误、无 kernel drop、无 malformed、清理后无 XDP/进程残留。

## 1. 改了什么

### 1.1 ABI v2（`probe-common/src/lib.rs`）

v1 的 24 字节 IPv4 专用事件（`src_ip`/`dst_ip: u32`）被删除，改为单一 48 字节结构，
IPv4/IPv6 共用同一个 ringbuf：

```rust
pub const EVENT_ABI_VERSION: u8 = 2;
pub const FAMILY_IPV4: u8 = 4;
pub const FAMILY_IPV6: u8 = 6;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PacketEvent {
    pub ts_ns: u64,
    pub src_addr: [u8; 16],
    pub dst_addr: [u8; 16],
    pub src_port: u16,
    pub dst_port: u16,
    pub protocol: u8,
    pub family: u8,
    pub version: u8,
    pub reserved: u8,
}
```

| offset | 字段 | 类型 | 本阶段取值 |
|---:|---|---|---|
| 0 | `ts_ns` | `u64` | `bpf_ktime_get_ns()` |
| 8 | `src_addr` | `[u8;16]` | 网络序原始字节；IPv4 左对齐补 0 |
| 24 | `dst_addr` | `[u8;16]` | 同上 |
| 40 | `src_port` | `u16` | 0（L4 未解析） |
| 42 | `dst_port` | `u16` | 0 |
| 44 | `protocol` | `u8` | IPv4 protocol / IPv6 base header Next Header |
| 45 | `family` | `u8` | 4 或 6 |
| 46 | `version` | `u8` | 2 |
| 47 | `reserved` | `u8` | 0 |

`size_of == 48`、`align_of == 8`。`probe-common` 新增了逐字段 `offset_of!` 单测和
ABI 常量单测（`cargo test -p probe-common --release`：2 passed）。

这是**有意的破坏性升级**：旧用户态二进制不能读取新的 ringbuf 记录，升级必须同时
重新编译 eBPF 和用户态。用户态解码前校验 `version`/`family`，不匹配则跳过并计入
`malformed`，不会按 v1 布局误读。CSV 仍是 7 列，旧 IPv4 CSV 可以继续 `--append`。

### 1.2 eBPF（`probe-ebpf/src/main.rs`）

- 保留 Ethernet + 最多两层 802.1Q/802.1ad；按**内层** ethertype 分流：
  `0x0800 → IPv4`、`0x86DD → IPv6`，其它 `XDP_PASS` 不产生事件。
- IPv4：固定 20 字节头，`version/ihl` 校验与 options 边界检查保留；地址改为
  `[u8;4]`，不再用 `u32` + `from_be`；用固定长度 `copy_nonoverlapping(.., 4)`
  左对齐到 16 字节字段，其余 12 字节为 0。IPv4 输出与 v1 完全一致。
- IPv6：只解析固定 40 字节 base header。`ptr_at::<Ipv6Hdr>` 证明
  `[l3_offset, l3_offset+40)` 在 `[data, data_end)` 内（截断包 `XDP_PASS`）；
  首字节高 4 位必须为 6（不把 version/tc/flow 当 `u32` 处理）；
  `next_header` 直接作为 `protocol`；`saddr`/`daddr` 16 字节原样拷贝。
  不遍历扩展头，不读 payload。
- 事件写 ringbuf：`reserve::<PacketEvent>(0)` 成功则 `write + submit`，失败只递增
  当前 CPU 的 `DROPS[0]`；始终返回 `XDP_PASS`；无 per-packet 日志/格式化/分配。
- `#[cfg(all(not(test), target_arch = "bpf"))]` panic handler 原样保留，`cargo check`
  的 workspace 特征统一视图下无 E0152。

### 1.3 用户态（`probe/src/{main.rs,sink.rs}`）

- `Event.src_ip`/`dst_ip` 改为 `std::net::IpAddr`；`CsvSink` 用 `Display` 写出：
  IPv4 点分十进制，IPv6 标准压缩格式（`fd00:99::2`）。
- `Event::from_abi` 返回 `Option<Self>`：校验 `version == EVENT_ABI_VERSION`；
  `family == 4` → `Ipv4Addr::new(a,b,c,d)`（取前 4 字节）；`family == 6` →
  `Ipv6Addr::from(bytes)`；其它版本/族返回 `None`。
- `drain_ring` 仍检查 `item.len() >= size_of::<PacketEvent>()` 后 `read_unaligned`；
  解码失败的记录计入 `malformed`，统计行变为
  `stats: events_received=.. rows_written=.. kernel_dropped=.. malformed=..`。
- CSV 列、CLI 参数、`Sink` trait、SIGINT/SIGTERM 退出路径均未变。
- 新增用户态单测：IPv4/IPv6 解码、未知 version/family 拒绝、以及并发写 CSV 的
  append 回归测试（4 passed）。

### 1.4 验证中发现并修复的问题：并发写同一 CSV 会丢行

按任务要求同时启动两个 probe（veth0 无 `--append`、veth1 `--append`）写同一个 CSV 时，
第一版实现里 veth1 的行全部丢失：无 `--append` 的进程用独立 file offset 写，后来
flush 时覆盖了另一个进程 `O_APPEND` 追加的行。修复：`CsvSink::create` 两种模式都用
`O_APPEND` 打开，非 `--append` 时用 `set_len(0)` 显式截断（`std` 的 `OpenOptions`
不允许 `truncate + append` 同时设置）。修完后 phase AB 的 13 + 9 = 22 行全部保留，
并有单测 `truncating_sink_does_not_clobber_appended_rows` 防止回归。

### 1.5 依赖/镜像

- 未修改 `Dockerfile` / `docker-compose.yml`。
- 未新增第三方 crate（`std::net::{IpAddr, Ipv4Addr, Ipv6Addr}` 足够），`Cargo.toml`
  未改动。

## 2. 构建与静态检查

```text
$ cd /workspace/probe
$ cargo build --release
    Finished `release` profile [optimized] target(s) in 3.58s

$ cargo test -p probe-common --release
test tests::abi_constants_are_pinned ... ok
test tests::event_layout_is_stable ... ok
test result: ok. 2 passed; 0 failed

$ cargo test -p probe --release
test sink::tests::decodes_ipv4_from_left_aligned_bytes ... ok
test sink::tests::decodes_ipv6_from_full_16_bytes ... ok
test sink::tests::rejects_unknown_version_and_family ... ok
test sink::tests::truncating_sink_does_not_clobber_appended_rows ... ok
test result: ok. 4 passed; 0 failed

$ cargo check -p probe -p probe-ebpf
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.23s
```

三者 exit 0，无 E0152，无 verifier 报错（程序成功 attach 并被流量执行，见下）。

## 3. IPv4 回归

脚本 `verification/run_e2e.sh` 未改变断言强度，只做了两处测试环境修正：

1. IPv4 phase 对 veth0/veth1 及 VLAN 子接口写
   `/proc/sys/net/ipv6/conf/<if>/disable_ipv6 = 1`。IPv6 捕获开启后 veth 默认会
   产生 ND/MLD/ICMPv6，不隔离就会打破“恰好 3 行 / protocol 全为 1”的精确断言。
   这是测试隔离，不是功能限制。
2. 设置 `kernel.bpf_stats_enabled=1`（本 WSL2 内核默认 0）。否则
   `bpftool prog show` 不输出 `run_cnt`，断言会变成 0 → 0 的假失败。

结果：

```text
$ cd /workspace/verification && ./run_e2e.sh
...
CHECK PASS: phase A ping 3/3 without loss
CHECK PASS: phase A probe reports 3 events
CHECK PASS: phase A CSV has 3 data rows (got 3)
CHECK PASS: phase A CSV source is 10.99.0.2
CHECK PASS: phase A run_cnt grew (0 -> 4)
...
CHECK PASS: phase B run_cnt grew (0 -> 4)
CHECK PASS: append kept previous rows (3 -> 6)
CHECK PASS: CSV protocol is ICMP (1) for all rows
CHECK PASS: VLAN CSV has 10.100.0.2 -> 10.100.0.1 (3 rows)
CHECK PASS: no XDP attachment left after cleanup
CHECK PASS: no probe process left
RESULT: PASS
```

25 项断言全部 PASS，0 失败；CSV 中 IPv4 地址与 ping 一致（`10.99.0.2 ↔ 10.99.0.1`、
VLAN `10.100.0.2 → 10.100.0.1`），`protocol=1`，`kernel_dropped=0 malformed=0`。
本次回归完整输出：`verification/ipv4-regression-run-stdout.log`。

> 注：重跑 `run_e2e.sh` 刷新了它自己的旧证据文件（`e2e.log`、`packets.csv`、
> `bpftool-net-*.log` 等）。这些文件现在记录的是**本次（ABI v2）**运行，时间戳为
> 2026-10-31；`REPORT-xdp-ipv4.md` 仍描述上一版结果，其断言与本次一致。新的完整
> 输出另存为 `ipv4-regression-run-stdout.log`，不会被后续 IPv6 脚本覆盖。

## 4. IPv6 端到端

脚本：`verification/run_e2e_ipv6.sh`（可重复执行，自动清理）。
拓扑：`veth0`（容器 netns，`fd00:99::1/64`）+ `veth1`（`aya-ns`，`fd00:99::2/64`），
两端各起一个 probe 写同一个 `verification/ipv6-packets.csv`，第二个用 `--append`。

### 4.1 双向 ping 与 CSV

```text
$ ip netns exec aya-ns ping -6 -c 3 -W 1 fd00:99::1
3 packets transmitted, 3 received, 0% packet loss

$ ping -6 -c 3 -W 1 fd00:99::2
3 packets transmitted, 3 received, 0% packet loss
```

CSV（节选，含两个方向与 `fe80::` 控制流量）：

```csv
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
1793452928195,veth0,fe80::cf7:5eff:feac:8587,ff02::2,,,58
1793452928885,veth0,fd00:99::2,ff02::1:ff00:1,,,58
1793452928885,veth0,fd00:99::2,fd00:99::1,,,58
1793452929891,veth0,fd00:99::2,fd00:99::1,,,58
1793452930893,veth0,fd00:99::2,fd00:99::1,,,58
1793452930917,veth0,fd00:99::2,fd00:99::1,,,58
1793452931939,veth0,fd00:99::2,fd00:99::1,,,58
1793452928884,veth1,fd00:99::1,fd00:99::2,,,58
1793452929890,veth1,fd00:99::1,fd00:99::2,,,58
1793452930891,veth1,fd00:99::1,fd00:99::2,,,58
1793452931938,veth1,fd00:99::1,fd00:99::2,,,58
1793452927715,veth0,::,ff02::16,,,0
```

- `veth0` ingress 出现 `fd00:99::2 → fd00:99::1` 6 行，`veth1` ingress 出现反向
  7 行（3 个请求 + 3 个回显应答，另 1 行是 NUD/NA 触发的 ICMPv6），全部
  `protocol=58`（ICMPv6）。
- 断言按 ULA 过滤（不按总行数）：phase AB 共 22 行，其中 13 行是双向 ULA 流量，
  其余 9 行是 ND/MLD 控制流量（6 行以 `fe80::` 为源，2 行 DAD/MLD 源为 `::`，
  1 行为发往 solicited-node 多播的 NS）。
- IPv6 地址为标准压缩格式；没有 `::ffff:`，也没有把 `fe80::` 的字节误写成
  `254.128.x.x`（phase D 有专门的断言）。
- `protocol=0` 的行是 MLDv2 报告携带 Hop-by-Hop 扩展头导致的，正好说明本阶段
  `protocol` 只是 base header 的 Next Header。

### 4.2 bpftool：attach 与 run_cnt

```text
$ bpftool net list
xdp:
veth0(64) driver id 133

$ bpftool prog show id 133
133: xdp  name probe  tag 3732f64649b7d696  gpl run_time_ns 50891 run_cnt 5
        loaded_at 2026-10-31T13:22:06+0000  uid 0
        xlated 1456B  jited 902B  memlock 4096B  map_ids 101,100
        btf_id 144

$ bpftool prog show id 133     # ping 之后
133: xdp  name probe  tag 3732f64649b7d696  gpl run_time_ns 80293 run_cnt 13
        ...

$ ip netns exec aya-ns bpftool net list
xdp:
veth1(63) driver id 138

$ bpftool prog show id 138     # 2 -> 9
138: xdp  name probe  tag 3732f64649b7d696  gpl run_time_ns 30333 run_cnt 9
```

`run_cnt` 分别 5 → 13、2 → 9，证明两个接口都挂了程序且被真实流量执行。
`xlated 1456B` 说明内核接受、无 verifier 错误。

### 4.3 丢弃计数、信号与 --append

- 4 次 probe 运行的统计都是
  `kernel_dropped=0 malformed=0`（veth0 13 events、veth1 9、VLAN 11、mixed 9）。
- veth0 probe 用 **SIGINT**、netns 内 probe 用 **SIGTERM** 停止；两者都在退出前
  排空 ringbuf 并 flush，CSV 中 13 + 9 = 22 行全部落盘（与 `events_received` 之和
  相等），日志中都打印了 stats 行。
- `--append` 后 CSV 只有 1 个表头，veth0 行未被覆盖：22 行 = 13 + 9。
- 清理后 `bpftool net list` 的 `xdp:` 为空，`pgrep -x probe` 无输出。

## 5. IPv6 over VLAN 100 与混合地址

### 5.1 VLAN 100（0x8100 → 0x86DD）

在 veth0/veth1 上创建 `veth0.100`/`veth1.100`，配置 `fd00:100::1/64`、
`fd00:100::2/64`，从 netns `ping -6 fd00:100::1`：

```csv
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
1793452935012,veth0,fd00:100::2,ff02::1:ff00:1,,,58
1793452935012,veth0,fd00:100::2,fd00:100::1,,,58
1793452936034,veth0,fd00:100::2,fd00:100::1,,,58
1793452937058,veth0,fd00:100::2,fd00:100::1,,,58
```

ping 3/3，`fd00:100::2 → fd00:100::1` 恰好 3 行且 `protocol=58`，证明 VLAN 内层
ethertype 为 `0x86DD` 时也能正确解析。

### 5.2 一次运行同时包含 IPv4 与 IPv6

同一个 probe 进程先 `ping -6` 再 `ping -4`：

```csv
1793452938823,veth0,fd00:99::2,fd00:99::1,,,58
1793452940866,veth0,fd00:99::2,fd00:99::1,,,58
1793452940874,veth0,10.99.0.2,10.99.0.1,,,1
1793452941890,veth0,10.99.0.2,10.99.0.1,,,1
1793452942891,veth0,10.99.0.2,10.99.0.1,,,1
```

两种地址格式并存，列数仍为 7；所有 `protocol=58` 行地址均为 IPv6 格式，所有
`protocol=1` 行均为 IPv4 点分十进制。

## 6. 断言汇总

```text
$ cd /workspace/verification && ./run_e2e_ipv6.sh
CHECK PASS: IPv6 stack usable (ping -6 ::1)
CHECK PASS: phase A ping 3/3 without loss
CHECK PASS: phase B ping 3/3 without loss
CHECK PASS: veth0 ingress saw fd00:99::2 -> fd00:99::1 rows (>=3, got 6)
CHECK PASS: all veth0 ULA rows are ICMPv6 (58; 0 bad)
CHECK PASS: veth1 ingress saw fd00:99::1 -> fd00:99::2 rows (>=3, got 7)
CHECK PASS: all veth1 ULA rows are ICMPv6 (58; 0 bad)
CHECK PASS: probe A stats: no kernel drops
CHECK PASS: probe B stats: no malformed records
CHECK PASS: append kept exactly one header
CHECK PASS: append kept every row from both probes (22 rows)
CHECK PASS: CSV contains both interfaces
CHECK PASS: veth0 run_cnt grew (5 -> 13)
CHECK PASS: veth1 run_cnt grew (2 -> 9)
...
CHECK PASS: VLAN IPv6 protocol is ICMPv6 (58)
CHECK PASS: mixed CSV has 3 IPv4 ICMP rows
CHECK PASS: mixed CSV has 3 IPv6 ICMPv6 rows
CHECK PASS: no fe80 bytes decoded as IPv4 (254.128.x.x)
CHECK PASS: no XDP attachment left after cleanup
CHECK PASS: no probe process left
RESULT: PASS
```

34 项断言全部 PASS，0 失败。完整输出：`verification/ipv6-run-stdout.log`、
`verification/ipv6-e2e.log`。

## 7. 已知限制

- IPv6 只解析固定 40 字节 base header，**不遍历扩展头**。`protocol` 是 base header
  的 Next Header；遇到 Hop-by-Hop(0)/Routing(43)/Fragment(44)/DestOpts(60)/AH(51)
  时可能不是最终 L4 协议（本次证据中的 MLDv2 行显示 `0`）。
- 不解析 L4 端口，`src_port`/`dst_port` 仍为 0、CSV 留空；不读 payload。
- 仅 ingress，一个进程一个 `--iface`；看 veth 两端需要两个进程。
- IPv6 自动生成的 `fe80::`/`ff02::` ND/MLD 控制流量会进入 CSV，属正常现象；
  按 ULA 或具体地址过滤后再断言。
- 两进程并发写同一 CSV 时行不会互相覆盖（O_APPEND），但跨进程行序不确定。
- `/workspace` 是 bind mount，只保证 `flush(2)` 级别，不逐批 `fsync`。
- WSL2 下使用 `XdpMode::default()`；veth 走 driver 模式，`lo` 走 generic。

## 8. 证据文件

| 文件 | 内容 |
|---|---|
| `ipv6-run-stdout.log` / `ipv6-e2e.log` | IPv6 脚本完整输出（34 项断言） |
| `ipv6-packets.csv` | 双向 IPv6 + ND/MLD 噪声（22 行） |
| `ipv6-vlan.csv` | IPv6 over VLAN 100（11 行） |
| `ipv6-mixed.csv` | 同一次运行 IPv4+IPv6（9 行） |
| `ipv6-probe-{veth0,veth1,vlan,mixed}.log` | 各 probe 的 stats（drops/malformed 均为 0） |
| `ipv6-ping-*.log` | 5 次 ping 均 3/3、0% loss |
| `ipv6-bpftool-net-{a,b,c,final}.log` | attach 与清理后的 `xdp:` |
| `ipv6-bpftool-prog-{a,b}-{before,after}.log` | `run_cnt` 与 xlated/jited 信息 |
| `ipv4-regression-run-stdout.log` | 本次 IPv4 回归完整输出（25 项断言） |
| `run_e2e_ipv6.sh` | 新的 IPv6 端到端脚本（可重复执行） |

## 9. 复现

```bash
docker exec -it aya-dev bash
cd /workspace/probe
cargo build --release
cargo test -p probe-common --release
cargo test -p probe --release
cargo check -p probe -p probe-ebpf

cd /workspace/verification
./run_e2e.sh          # IPv4 回归
./run_e2e_ipv6.sh     # IPv6 端到端（含 VLAN 与混合地址）
```
