# probe — XDP ingress 窗口聚合采集（Train 0）

## 概述

在 XDP ingress 观察流量：解析 Ethernet →（可选 802.1Q/802.1ad VLAN）→ IPv4 **或**
IPv6，必要时取 TCP/UDP 端口，然后在**内核 per-CPU 哈希表里按窗口聚合**，只把每个
`(window_id, AggKey)` 的**首包样本**（64 字节）经 ringbuf 送给用户态。用户态按固定
周期扫描、合并各 CPU 槽位、关闭已结束的窗口，最终写出两份 CSV：

- **样本 CSV**（`--out`，7 列）：每窗口每键最多一行，记录首包五元组。
- **聚合 CSV**（`--agg-out`，16 列）：每窗口每键一行，`count` 是该窗口内该键的**完整
  包数**。

程序的唯一判决是 `XDP_PASS`：本阶段是纯观察者，不做规则匹配、不丢包、不改包。

```
            NIC / veth ingress
                    │
                    ▼
    ┌───────────────────────────────────────────────┐
    │ probe-ebpf（XDP，恒 XDP_PASS）                 │
    │  parse Ethernet → [VLAN×2] → IPv4 / IPv6      │
    │  每包恰好一次 bpf_ktime_get_ns()               │
    │  window_id = ts_ns / CONFIG.window_ns          │
    │  AGG[window_id, AggKey]++（per-CPU 哈希）       │
    │  本 CPU 本窗口该键首包 → SAMPLES ringbuf（64B） │
    │  样本预留失败 → STATS[SAMPLE_FAILED]++          │
    └───────────┬───────────────────────┬───────────┘
                │ CONFIG（启动前写、只读） │ SAMPLES ringbuf 256 KiB
                ▼                       ▼
    ┌───────────────────────────────────────────────┐
    │ probe（用户态，tokio）                         │
    │  写 CONFIG → 回读 → attach XDP                 │
    │  每 --flush-ms 收样本、按 (window_id, key) 去重 │
    │  每 --scan-ms 扫 AGG：读槽位 → 合并 → 删除      │
    │  关闭条件：window_end + grace ≤ now            │
    │  → packets.csv（样本） + aggregates.csv（聚合） │
    └───────────────────────────────────────────────┘
```

## 目录

```
probe-common/src/lib.rs   # 唯一 ABI 定义：SampleEvent / AggKey / AggValue / ProbeConfig + 布局单测
probe-ebpf/src/main.rs    # XDP 数据面：解析、CONFIG 读取、per-CPU 聚合、首包样本、STATS
probe/src/main.rs         # CLI、CONFIG 下发/回读、attach、ringbuf 与 AGG 扫描循环、退出统计
probe/src/aggregate.rs    # 纯逻辑：窗口水位线、per-CPU 槽位合并、样本去重、时钟与格式化
probe/src/sink.rs         # Event / Sink / AggSink 抽象 + 两个 CSV 后端 + 工厂
```

## 数据面流程

1. **入口**：`probe(ctx)` 调用 `try_probe`，无论解析成功与否，返回值都只有
   `XDP_PASS`。没有 per-packet 日志、格式化或动态分配。
2. **解析**：Ethernet(14 B) → 最多两层 `0x8100`/`0x88A8` VLAN（每层 4 B）→ 按内层
   ethertype 分流：`0x0800 → IPv4`、`0x86DD → IPv6`。其它 ethertype、截断包、
   version/ihl 非法的包直接 `XDP_PASS`，不产生聚合、不产生样本。
3. **时间戳**：解析成功后**每包只调用一次** `bpf_ktime_get_ns()`。同一个 `ts_ns`
   同时用于 `window_id = ts_ns / window_ns` 和 `first_seen`/`last_seen`，因此样本里的
   `ts_ns` 与聚合时间戳来自同一时基。
4. **读取 CONFIG**：`CONFIG[0]` 是用户态 attach 之前写入的 `ProbeConfig`；数据面读取
   `window_ns` 与两个前缀长度。CONFIG 缺失/非法时使用安全回退：`window_ns == 0` →
   `DEFAULT_WINDOW_NS = 10 s`，前缀长度越界则 clamp，并累加
   `STATS[STATS_CONFIG_MISSING]`；**绝不因为配置问题丢包**。
5. **聚合**：构造 `AggKey`（源前缀按 CONFIG 里对应 family 的长度掩码，`direction`
   恒为 `DIRECTION_INGRESS = 1`，`action = pass`，`rule_id = 0`，`reason = 0`），
   在 per-CPU `AGG` 哈希表里 `count += 1` 并更新 `first_seen`/`last_seen`。
   `count` 从 0 变成 1 的那个包就是“本 CPU、本窗口、该键的首包”。
6. **样本**：仅首包尝试 `SAMPLES.reserve::<SampleEvent>()` 写一条恰好 64 B 的记录；
   ringbuf 满时累加 `STATS[STATS_SAMPLE_FAILED]`，并把该槽位的
   `AggValue.sample_dropped` 置 1。样本失败**不影响 count**，也不影响 `XDP_PASS`。
7. **统计**：`STATS` 是 per-CPU array，索引见下；用户态退出时按 CPU 求和。

`AGG` 满（插入失败）时累加 `STATS[STATS_AGG_OVERFLOW]` 并跳过该包的样本尝试，数据面
继续运行；本阶段不做 LRU/淘汰，容量由窗口时长与键基数决定。

## map 契约

| map | 类型 | 条目数 | key | value | 说明 |
|---|---|---:|---|---|---|
| `CONFIG` | `Array` | 1 | `u32` 索引 | `ProbeConfig`（32 B） | attach 前由用户态写入；数据面只读 |
| `AGG` | `PerCpuHashMap` | 131072 | `AggKey`（56 B） | `AggValue`（32 B） | 每 CPU 独立计数；窗口结束后由用户态读走并删除 |
| `SAMPLES` | `RingBuf` | 256 KiB | — | `SampleEvent`（64 B） | 每 CPU 每窗口每键最多一条首包样本 |
| `STATS` | `PerCpuArray` | 8 | `u32` 索引 | `u64` | per-CPU 计数器，索引见下表 |

**本阶段没有 `DROPS` map**：样本预留失败已并入 `STATS[STATS_SAMPLE_FAILED]`，退出统计
里的 `kernel_dropped` 就是它，字段名沿用旧实现以兼容验证脚本。

`STATS` 索引（`probe-common` 中的常量，4..7 预留）：

| 索引 | 常量 | 含义 |
|---:|---|---|
| 0 | `STATS_PACKETS_TOTAL` | 成功解析的包数（所有 CPU 求和 = `packets_total`） |
| 1 | `STATS_AGG_OVERFLOW` | `AGG` 插入失败次数 |
| 2 | `STATS_SAMPLE_FAILED` | 样本 ringbuf 预留失败次数（= 退出统计的 `kernel_dropped` / `sample_failed`） |
| 3 | `STATS_CONFIG_MISSING` | 使用回退配置的包数（不进入固定 stdout 行，供扩展） |

> **内存注意**：`AGG` 是 per-CPU 哈希，值区大小按 CPU 数放大。本环境（20 vCPU）
> 实测 `bpftool map show`：`AGG` memlock = **100,664,832 B（96.0 MiB，约 768 B/元素）**，
> `SAMPLES` 275,864 B、`CONFIG` 416 B、`STATS` 1,728 B，四个 map 合计约 **96.3 MiB**。
> 估算公式为 `key 56 B + value 32 B × CPU 数 + 内核元素头`，CPU 更少时近似线性下降；
> 数值来源与复现命令见 `verification/REPORT-aggregation.md` §4.3。

## ABI 布局

`probe-common/src/lib.rs` 是内核与用户态**唯一**的结构定义，`#[repr(C)]`、`Copy`、
无指针；`probe-common` 的 `cargo test` 用 `offset_of!` 逐字段钉住布局，任何漂移都会
让单测失败。

### `SampleEvent`（64 B，ringbuf 记录）

| offset | 字段 | 类型 | 说明 |
|---:|---|---|---|
| 0 | `ts_ns` | `u64` | `bpf_ktime_get_ns()`（CLOCK_MONOTONIC，ns） |
| 8 | `window_id` | `u64` | `ts_ns / window_ns`，内核用 CONFIG 的 `window_ns` 计算 |
| 16 | `src_addr` | `[u8;16]` | 网络序原始字节；IPv4 左对齐、其余补 0 |
| 32 | `dst_addr` | `[u8;16]` | 同上 |
| 48 | `src_port` | `u16` | 网络序；ICMP/ICMPv6 或无 L4 头时为 0 |
| 50 | `dst_port` | `u16` | 网络序；同上 |
| 52 | `protocol` | `u8` | IPv4 protocol / IPv6 base header 的 Next Header |
| 53 | `family` | `u8` | 4 = IPv4，6 = IPv6 |
| 54 | `action` | `u8` | 本阶段恒 `ACTION_PASS = 0`（E6 预留） |
| 55 | `reason` | `u8` | 本阶段恒 0（E6 预留） |
| 56 | `rule_id` | `u32` | 本阶段恒 0（E6 预留） |
| 60 | `reserved` | `u32` | 恒 0 |

`size_of == 64`、`align_of == 8`。**结构中不存在版本字段**，也没有对应的版本常量。

### `AggKey`（56 B，per-CPU `AGG` 的 key）

| offset | 字段 | 类型 | 说明 |
|---:|---|---|---|
| 0 | `window_id` | `u64` | 窗口索引 |
| 8 | `rule_id` | `u32` | 本阶段恒 0 |
| 12 | `direction` | `u8` | `DIRECTION_INGRESS = 1` |
| 13 | `family` | `u8` | 4 / 6 |
| 14 | `src_prefix` | `[u8;16]` | 已按前缀长度掩码；IPv4 左对齐 |
| 30 | `dst_addr` | `[u8;16]` | 目的地址原始字节 |
| 46 | `dst_port` | `u16` | 网络序；0 表示 ICMP/无端口 |
| 48 | `protocol` | `u8` | IPv4 protocol / IPv6 Next Header |
| 49 | `action` | `u8` | 本阶段恒 0 = pass |
| 50 | `reason` | `u8` | 本阶段恒 0 |
| 51 | `pad` | `[u8;5]` | 显式补齐，恒 0 |

`size_of == 56`、`align_of == 8`。**字段顺序是有意调整过的**：把 `rule_id` 放在
`window_id` 之后，随后才是字节字段与 16 B 数组，可以在保持自然对齐的同时把记录压到
56 B；若按“逻辑顺序”（`window_id, direction, family, ...` 最后放 `rule_id`）补齐会
变成 64 B。`pad` 必须显式写 0——否则未初始化的填充字节会把同一个逻辑键拆成多个 map
条目。

### `AggValue`（32 B）与 `ProbeConfig`（32 B）

| 结构 | offset / 字段 | 类型 | 说明 |
|---|---|---|---|
| `AggValue` | 0 `count` | `u64` | 本 CPU 本键本窗口的包数 |
| | 8 `first_seen` | `u64` | 该 CPU 上最早 `ts_ns` |
| | 16 `last_seen` | `u64` | 该 CPU 上最晚 `ts_ns` |
| | 24 `sample_dropped` | `u8` | 该键的样本预留失败过 |
| | 25 `degraded` | `u8` | 预留（Train 0 恒 0） |
| | 26 `pad` | `[u8;6]` | 恒 0 |
| `ProbeConfig` | 0 `version` | `u32` | 配置布局校验值 `CONFIG_VERSION = 1`（**不是样本事件的版本号**） |
| | 8 `window_ns` | `u64` | 窗口宽度；0 触发内核回退 `DEFAULT_WINDOW_NS` |
| | 16 `grace_ns` | `u64` | 窗口结束后的宽限期（用户态扫描使用） |
| | 24 `ipv4_prefix_len` | `u8` | 0..=32 |
| | 25 `ipv6_prefix_len` | `u8` | 0..=128 |
| | 26 `default_action` | `u8` | 未匹配规则的默认动作（Train 0 恒 pass） |
| | 27 `pad` | `[u8;5]` | 恒 0 |

### 兼容政策（本阶段硬性约定）

- **没有事件版本号，也不承诺跨构建兼容**。eBPF 对象在编译期通过
  `include_bytes_aligned!` 嵌进 `probe` 二进制，对象与 loader 永远来自同一次构建；
  任何布局改动都是破坏性改动，**必须整仓重新编译**。
- 不要用新 `probe` 配旧对象，也不要用旧 `probe` 读新对象。
- 只有将来 eBPF 对象需要**脱离 loader 独立分发**时，才重新引入版本字段，并从 0 开始
  计数。
- 用户态仍然保留防御：ringbuf 记录长度必须**恰好** `size_of::<SampleEvent>() == 64`
  （更短或更长都判为 `malformed`，绝不按错误布局解析）；`family` 必须是 4 或 6；
  样本的窗口归属用**写入 CONFIG 的同一个 `window_ns`** 复核。

## 解析与 L4 端口

```
Ethernet(14B) → [802.1Q/802.1ad ×2，各 4B] → ethertype ┬ 0x0800 → IPv4 ─┐
                                                       └ 0x86DD → IPv6 ─┤
                                          其它 / 截断 / 非 IP → XDP_PASS │
                                                                        ▼
                                                          AggKey + （首包时）SampleEvent
```

- **VLAN**：最多两层，按**内层** ethertype 判定 IP 版本；`0x8100 → 0x86DD`（IPv6
  over VLAN）同样支持。
- **IPv4**：校验 `version == 4`、`ihl >= 5`，并用 `data_end` 检查 `l3_offset + ihl*4`
  在包内（IPv4 options 不会越界）。地址按 4 个原始字节左对齐拷贝到 16 B 字段。
- **IPv6**：只解析固定 40 B base header，不遍历扩展头；首字节高 4 位必须为 6。
  `protocol` 取 base header 的 Next Header，`saddr`/`daddr` 原样 16 B 拷贝。
- **端口**：仅当协议是 TCP(6)/UDP(17) 且 L4 头在包内时读取两个端口；ICMP/ICMPv6 或
  其它协议恒为 0，用户态输出整数 `0`，不是空字段。IPv4 **非首分片**（`frag_off &
  0x1fff != 0`）没有 L4 头，端口也置 0，但该包仍计入聚合。
- 所有地址都是**网线上的原始字节**（网络序）：不做字节序转换，不使用 IPv4-mapped
  IPv6；用户态用 `Ipv4Addr::new` / `Ipv6Addr::from` 解码，端口用 `from_be` 还原。

## 聚合窗口与样本语义

- **窗口**：`window_id = ts_ns / window_ns`，窗口 `w` 覆盖
  `[w * window_ns, (w + 1) * window_ns)`。用户态把单调时钟换算成 wall-clock 毫秒后
  写出 `window_start_ms` / `window_end_ms`（`end` 为开区间）。
- **窗口相位由 `bpf_ktime_get_ns()` 决定**：`window_id` 是对内核单调时钟（开机的
  ktime）做整除，所以窗口边界**不是** wall-clock 的整秒/整十分边界，边界通常落在任意
  毫秒；wall-clock 只用于展示，不参与窗口划分。按时间断言的脚本应基于 ktime/相对
  关系（或先对齐到固定相位），不要假设 `window_start_ms % 1000 == 0`。
- **首个窗口可能开始于进程启动之前**：第一个包所属窗口在 probe 启动前就已开始是正常
  现象。`Clock` 会保留启动时刻与包时间戳之间的**负向偏移**，因此
  `window_start_ms` 可以早于进程启动时间，而 `window_end_ms - window_start_ms`
  **恒等于 `--window-ms`**（两者都按同一偏移换算）；只有早于 Unix epoch 的极端值才会
  clamp 到 0。
- **关闭**：窗口 `w` 在 `(w + 1) * window_ns + grace_ns <= now` 时关闭。扫描每
  `--scan-ms` 进行一次，只处理已关闭窗口；`AggTable` 维护已上报水位线，即使某个空
  窗口没有流量也会推进水位线，避免旧窗口被“重新发现”后重复上报。
- **合并**：同一 `(window_id, AggKey)` 在多个 CPU 上各有一个槽位；`count` 求和，
  `first_seen` 取最小、`last_seen` 取最大，`sample_dropped`/`degraded` 取逻辑或。
  **只有 `count > 0` 的槽位参与时间戳计算**：值为全零的槽位（计数竞争落败）不会被
  当成“1970 年”污染 `first_seen`。
- **先收集后删除**：扫描先把槽位读入内存并合并，再对读过的 key 执行 `delete`；
  删除发生在上报之后，尽量缩小“已读未删”窗口。
- **样本去重**：首包样本在每 CPU 上各写一次，且两个源地址可能落在同一个掩码前缀键
  上，所以样本行可能重复。用户态按 `(window_id, AggKey)` 去重，**保留 `ts_ns` 最小
  的那条**；聚合键用同一个回读的 `ProbeConfig` 前缀长度重建
  （`AggKey::from_sample`），保证与内核键逐字节一致。
- **迟到数据**：`window_id` 已低于等于水位线的槽位/样本不再打开旧窗口，计入
  `late_dropped`，同时保证同一窗口不会写出第二行。
- **退出**：收到 SIGINT/SIGTERM 后：最后一次样本 drain → 扫描 `AGG` 全部剩余键 →
  **所有未上报窗口**（含已过 `end + grace` 但还没来得及被扫描上报的窗口）按
  `window_complete = 0` 写出 partial 行 → 写出剩余待写样本 → flush 两份 CSV → 读
  `STATS` → detach XDP → 打印统计。**SIGKILL 可能丢失尚未 flush 的 partial 数据**。
- **周期指标**：运行期间每 5 s 用 `info!` 打印一次 `metrics:` 行（累计
  `packets_total`、当前 `agg_keys`、`agg_overflow`、`sample_failed`、
  `late_dropped`，以及上次扫描耗时 `scan_ms` 与配置的 `scan_interval_ms`），
  便于在线观察；数据面本身不产生日志。

## CLI 参数

| 参数 | 默认值 | 说明 |
|---|---|---|
| `--iface <dev>` | `lo` | attach XDP 的网卡；一个进程只 attach 一个接口 |
| `--out <path>` | `/workspace/packets.csv` | 样本 CSV；父目录自动创建 |
| `--agg-out <path>` | `/workspace/aggregates.csv` | 聚合 CSV；父目录自动创建 |
| `--sink <csv>` | `csv` | 存储后端；`csv` 是当前唯一实现 |
| `--flush-ms <n>` | `500` | ringbuf 轮询 / 批量 flush 周期，必须 > 0 |
| `--append` | 关闭 | 追加而不是截断；两个 CSV 都只在一个进程启动时处理一次 |
| `--window-ms <n>` | `10000` | 聚合窗口宽度（毫秒）；下发 CONFIG 时换算为 ns |
| `--grace-ms <n>` | `2000` | 窗口结束后的宽限期（毫秒） |
| `--scan-ms <n>` | `1000` | 用户态扫描 `AGG` 的周期（毫秒） |
| `--src-prefix-v4 <n>` | `24` | IPv4 源前缀长度（0..=32） |
| `--src-prefix-v6 <n>` | `64` | IPv6 源前缀长度（0..=128） |

启动顺序是 **加载 eBPF 对象 → 写 `CONFIG` → 回读校验 → load/attach XDP**。回读值
（`window_ns`、两个前缀长度）是用户态后续扫描、窗口边界和 CSV 渲染的**唯一**来源，
不会使用第二份本地常量；`--window-ms`/`--grace-ms`/前缀参数只在下发 CONFIG 时使用一次。
回读不一致（写失败或值不符）时启动直接失败，**不会在 CONFIG 未生效的情况下 attach**。

不指定 `--append` 时，启动会截断两个 CSV 并重写表头；指定 `--append` 时保留旧内容，
仅当文件长度为 0 时写表头。两个后端都用 `O_APPEND` 打开文件，因此 veth 两端各起一个
probe 写同一份 CSV 时行不会互相覆盖；跨进程的行序不确定。

## CSV 格式与示例

### 样本 CSV（7 列，固定顺序）

```
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
```

- `ts_unix_ms`：用户态启动时采样一次 `SystemTime` 与 `CLOCK_MONOTONIC` 的偏移，把
  内核 `bpf_ktime_get_ns()` 换算成 Unix 毫秒；不是每包 syscall。
- `src_ip` / `dst_ip`：IPv4 点分十进制；IPv6 为 RFC 5952 压缩格式。
- `src_port` / `dst_port`：**始终是整数**，ICMP/ICMPv6 为 `0`（不是空字段、不是空
  字符串；解析器可以无条件按整数读取第 5/6 列）。
- `protocol`：IPv4 协议号（1 = ICMP，6 = TCP，17 = UDP）或 IPv6 base header 的
  Next Header（58 = ICMPv6）。

示例（数值仅示意）：

```csv
ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol
1793452928885,veth0,10.99.0.2,10.99.0.1,0,0,1
1793452928885,veth0,fd00:99::2,fd00:99::1,0,0,58
1793452929000,veth0,10.99.0.2,10.99.0.1,44321,8080,6
```

### 聚合 CSV（16 列，固定顺序）

```
window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,sample_dropped,degraded,window_complete
```

- `window_start_ms` / `window_end_ms`：窗口的 wall-clock 毫秒边界（`end` 开区间）。
- `direction`：本阶段恒为文本 `ingress`（未知方向会渲染成 `unknown`，便于暴露 bug）。
- `src_prefix`：CIDR 文本，例如 `10.99.0.0/24`、`fd00:99::/64`；前缀长度来自回读的
  `ProbeConfig`。
- `dst_addr`：目的地址（IPv4 点分十进制 / IPv6 压缩格式）。
- `action`：文本 `pass` / `block`；本阶段恒为 `pass`。
- `rule_id`：本阶段恒 `0`（E6 预留）。
- `count`：该窗口内该键的包数，跨 CPU 求和。
- `first_seen_ms` / `last_seen_ms`：窗口内最早/最晚包的时间，跨 CPU 取 min/max。
- `sample_dropped`：该键是否发生过样本预留失败（0/1，跨 CPU 取或）。
- `degraded`：预留标志，本阶段恒 `0`。
- `window_complete`：正常关闭写 `1`；进程退出时对未关闭窗口写 partial 行 `0`。

示例（数值仅示意）：

```csv
window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,sample_dropped,degraded,window_complete
1793452920000,1793452930000,veth0,ingress,10.99.0.0/24,10.99.0.1,0,1,pass,0,3,1793452928885,1793452929285,0,0,1
1793452920000,1793452930000,veth0,ingress,10.99.0.0/24,10.99.0.1,8080,6,pass,0,1500,1793452921000,1793452929900,0,0,1
```

## 退出统计

Ctrl-C（SIGINT）或 SIGTERM 会触发：最后一次 ringbuf drain → 扫描全部剩余 `AGG` 键 →
把**所有未上报窗口**写为 `window_complete = 0` 的 partial 行 → 写出剩余样本 →
flush 两份 CSV → 读 `STATS` → detach XDP。随后 stdout 打印**固定 token** 的统计行
（不经过 `log`，不受 `RUST_LOG` 影响）：

```
stats: packets_total=N sample_rows=K events_received=R rows_written=W agg_records=M kernel_dropped=D sample_failed=F agg_overflow=O malformed=X late_dropped=L samples_deduped=Y
```

| token | 含义 |
|---|---|
| `packets_total` | `STATS[STATS_PACKETS_TOTAL]` 跨 CPU 求和：成功解析的包数 |
| `sample_rows` | 写入样本 CSV 的数据行数 |
| `events_received` | 从 ringbuf 收到的样本记录数（去重前） |
| `rows_written` | 写入样本 CSV 的行数，**恒等于 `sample_rows`**（兼容旧脚本 token） |
| `agg_records` | 写入聚合 CSV 的数据行数（含 partial 行） |
| `kernel_dropped` | `Σ STATS[STATS_SAMPLE_FAILED]`：内核侧样本丢弃数（沿用旧字段名） |
| `sample_failed` | 同上，语义更明确的新名字 |
| `agg_overflow` | `Σ STATS[STATS_AGG_OVERFLOW]`：`AGG` 插入失败次数 |
| `malformed` | 无法解码的 ringbuf 记录数（长度 ≠ 64 或 family 非 4/6） |
| `late_dropped` | 迟到数据数：`AggTable` 与 `SampleDedup` 两处迟到丢弃之和 |
| `samples_deduped` | 被 `(window_id, key)` 去重吸收的重复样本数 |

示例（数值仅示意）：

```
stats: packets_total=1503 sample_rows=4 events_received=6 rows_written=4 agg_records=3 kernel_dropped=0 sample_failed=0 agg_overflow=0 malformed=0 late_dropped=0 samples_deduped=2
```

`RUST_LOG=info` 可看到 attach、CONFIG 回读、周期指标等控制面日志；数据面完全不依赖
`env_logger`。

## 构建与运行

```bash
cd /workspace/probe
cargo build --release

# 默认：样本 /workspace/packets.csv，聚合 /workspace/aggregates.csv，10s 窗口
sudo -E ./target/release/probe --iface veth0

# 完整参数示例
sudo -E ./target/release/probe \
    --iface veth0 \
    --sink csv \
    --out /workspace/packets.csv \
    --agg-out /workspace/aggregates.csv \
    --window-ms 10000 \
    --grace-ms 2000 \
    --scan-ms 1000 \
    --src-prefix-v4 24 \
    --src-prefix-v6 64 \
    --flush-ms 200 \
    --append
```

`cargo build --release` 会通过 `probe/build.rs` 构建 `probe-ebpf` 并把对象嵌进二进制，
所以改了 `probe-common`/`probe-ebpf` 之后**必须重新构建**，不能只替换二进制里的对象。
`cargo run` 也可用（`.cargo/config.toml` 里 `runner = "sudo -E"`），但验证时建议先
`cargo build --release` 再直接执行二进制，避免 sudo 环境差异触发重建。

### rust-analyzer 的 E0152

`probe-ebpf` 的 panic handler 只在真正的 BPF 目标上定义：

```rust
#[cfg(all(not(test), target_arch = "bpf"))]
#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! { loop {} }
```

原因是 `probe` 启用了 `probe-common/user` → `aya` → `std`；当 rust-analyzer 或
`cargo check -p probe -p probe-ebpf` 以 host target + workspace 特征统一去分析
`probe-ebpf` 时，std 已经提供 `panic_impl`，再定义一次就会报 “found duplicate lang
item `panic_impl`”。真实 eBPF 构建使用 `--target bpfel-unknown-none` +
`-Z build-std=core`，没有 std，仍然需要该 handler。`target_arch = "bpf"` 正好区分这
两种视图，不改变生成的 eBPF 对象。

## 验证方式

仓库根目录（挂载为容器内 `/workspace`）的 `verification/` 下有可重复执行的脚本，全部
使用真实 veth + netns，不做 mock：

| 脚本 | 覆盖内容 |
|---|---|
| `verification/run_e2e.sh` | IPv4 回归：attach、双向 ping、`run_cnt`、`--append`、VLAN、清理后无残留 |
| `verification/run_e2e_ipv6.sh` | IPv6 端到端：ULA、`protocol=58`、IPv6 over VLAN、混合地址 |
| `verification/run_aggregation.sh` | 聚合验收 A2–A7：3 个 ICMP 一行、前缀聚合、泛洪 Σcount、相邻窗口、SIGINT/SIGTERM partial、`late_dropped=0` |
| `verification/run_perf.sh` | 新旧实现性能对比（`MODE=old` / `MODE=new`）：发送数、样本行/秒、Σcount 误差、用户态 CPU%、`kernel_dropped` |

聚合改造的完整证据与结论见 `verification/REPORT-aggregation.md`；早期逐包实现的报告
（`REPORT-xdp-ipv4.md` / `REPORT-xdp-ipv6.md`）只适用于旧版本，本版 CSV 语义已变化。

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

sudo -E ./target/release/probe --iface veth0 --window-ms 4000 --grace-ms 500 --scan-ms 200 &
ip netns exec aya-ns ping -c 3 10.99.0.1
sleep 5
kill -INT %1

cat /workspace/packets.csv      # 每个 (窗口, 键) 一条样本
cat /workspace/aggregates.csv   # count=3 的聚合行，window_complete=1
bpftool net list

ip link del veth0
ip netns del aya-ns
```

## 已知限制

- **无规则匹配（E6 未实现）**：`action` 恒为 `pass`、`rule_id = 0`、`reason = 0`，
  不读 `default_action`，不做丢弃。E8（上报/控制面）同样不在本阶段。
- **IPv6 扩展头不遍历**：`protocol` 取 base header 的 Next Header；遇到
  Hop-by-Hop(0)/Routing(43)/Fragment(44)/DestOpts(60)/AH(51) 时该列可能不是最终 L4
  协议（例如 MLDv2 显示 `0`），此时端口为 0。
- **仅 ingress**：一个进程只 attach 一个 `--iface`；要观察 veth 两端就运行两个进程，
  可用 `--append` 写同一文件；两进程都以 `O_APPEND` 写，行不会互相覆盖，但跨进程行序
  不确定。
- **仅 IP 帧**：ARP、非 IP 帧、截断/畸形包直接 `XDP_PASS`，不产生聚合或样本。
- **非首分片无端口**：IPv4 非首分片按 `protocol` 聚合，但 `dst_port` 为 0。
- **AGG 内存随 CPU 数放大**：per-CPU 哈希的值区是 `32 B × CPU 数`，20 vCPU 全量
  131072 键的估算在 95 MiB 量级；容器 CPU 数变化会显著改变内存占用。
- **ringbuf 满只丢样本、不丢 count**：`SAMPLES` 固定 256 KiB；用户态按 `--flush-ms`
  轮询。极高速突发可能填满 ringbuf，此时只丢首包样本（`kernel_dropped`/
  `sample_failed` 增长、对应聚合行 `sample_dropped=1`），**包数仍完整**。只有
  **每窗口新键基数很大**（首包样本多）时才会出现 `sample_failed > 0`；单键持续流量
  通常为 0（20 vCPU 实测：100k pps × 5 s 单键 `sample_failed=0`；多 key 压测 50k
  dst_port 时 `sample_failed=86,996`、A5 验收的 100k 键场景 `sample_failed=103,414`，
  两场景 `Σcount` 均精确）。
- **SIGKILL 可能丢 partial**：只有 SIGINT/SIGTERM 会走完整退出流程；进程被强杀时
  尚未 flush 的 partial 窗口数据和 ringbuf 内容会丢失。
- **单点时间基**：窗口边界与时间戳都来自 `CLOCK_MONOTONIC` 换算；用户态启动时采样
  一次 wall-clock 偏移，不逐包或逐窗口校时。
- `/workspace` 是 bind mount（Windows 9p/drvfs），只保证 `flush(2)` 级别，不额外
  `fsync` 每个批次。
- WSL2 下 attach 使用 `XdpMode::default()`；本环境 veth 走 driver 模式、`lo` 走
  generic。若目标网卡不支持 native XDP，需要把 `XdpMode` 改为 `Skb`（generic）；
  native 模式未验证。

## 许可

与模板一致：用户态 MIT OR Apache-2.0，eBPF 代码 GPL-2.0 OR MIT。详见
`LICENSE-APACHE`、`LICENSE-MIT`、`LICENSE-GPL2`。
