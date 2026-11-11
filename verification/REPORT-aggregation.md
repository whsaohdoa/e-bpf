# XDP 窗口聚合改造报告（Train 0）

- 时间：2026-11-10（Asia/Shanghai）；A9/e2e 实测 2026-11-09
- 环境：容器 `aya-dev`，`uname -r` = `6.6.87.2-microsoft-standard-WSL2`，
  `nproc` = 20，`CLK_TCK` = 100
- 状态：**已回填实测**。A1–A11 全部已填；A2–A7 于 2026-11-09T18:57 跑完
  （`RESULT: PASS`，0 FAIL / 0 SKIP）。
- 关联代码：`probe/probe-common/src/lib.rs`、`probe/probe-ebpf/src/main.rs`、
  `probe/probe/src/{main.rs,aggregate.rs,sink.rs}`
- 关联脚本：`verification/run_aggregation.sh`、`verification/run_perf.sh`、
  `verification/run_e2e.sh`、`verification/run_e2e_ipv6.sh`

## 1. 背景与目标

改造前的实现是“逐包上报”：XDP 每解析一个包就往 ringbuf 写一条逐包事件，用户态逐条写
CSV。该方案在高包率场景下有两个结构性问题：

1. **ringbuf 成为瓶颈**：ringbuf 满就丢事件，丢的是“明细”，用户态无法从输出恢复真实
   包数；`kernel_dropped` 只能报告“丢了多少条记录”，不能报告“丢了多少个包”。
2. **写放大**：每包一行 CSV，10 万 pps 就是每秒 10 万行落盘，I/O 与用户态 CPU 都不可
   持续；且无法直接回答“某窗口某前缀命中了多少包”。

**目标**（Train 0）：把计数搬到内核的 per-CPU 聚合表，让**包数不因用户态慢而丢失**；
ringbuf 只传“每窗口每键的首包样本”，用于保留可读的五元组示例；用户态按固定窗口扫描、
跨 CPU 合并、输出聚合与样本两份 CSV。约束是：纯观察（恒 `XDP_PASS`）、无规则匹配
（E6 预留）、无跨构建 ABI 兼容承诺。

## 2. 改造前/后对比

| 维度 | 改造前（逐包） | 改造后（聚合 + 样本） |
|---|---|---|
| 数据面每包动作 | 解析 → 写一条逐包事件到 ringbuf | 解析 → per-CPU `AGG[key]++`；仅本 CPU/本窗口/该键首包写 1 条样本 |
| 事件结构 | 逐包事件（含 `ts_ns`、地址、端口、protocol、family） | `SampleEvent`（64 B，含 `window_id`）；`AggKey`/`AggValue` 在 map 中 |
| ringbuf | 每包一条，满即丢包 | 每键每窗口每 CPU 至多一条，满只丢样本 |
| 内核丢弃语义 | `DROPS[0]` = ringbuf reserve 失败次数 | `STATS[SAMPLE_FAILED]` = 样本预留失败；**count 不受影响** |
| map | ringbuf + `DROPS` per-CPU array | `CONFIG`/`AGG`/`SAMPLES`/`STATS`（无 `DROPS`） |
| 用户态主要工作 | 逐条解码 + 逐行写 | 周期扫 `AGG`、跨 CPU 合并、窗口关闭、样本去重、写两份 CSV |
| 输出 | 1 份样本 CSV（每包一行） | 样本 CSV（每窗口每键 1 行）+ 聚合 CSV（每窗口每键 1 行，含 `count`） |
| 丢包对结果的影响 | 丢明细 = 丢计数 | 丢样本 ≠ 丢计数；`sample_dropped` 标记可见 |
| 速率瓶颈 | ringbuf / 用户态写盘 | 窗口内 key 基数、扫描与合并开销 |

改造前后都保留“IPv4/IPv6 共用一条数据通路、地址为网络序原始字节、只做 ingress 观察”
这些既有约束。

```
改造前：
  XDP ──1 条/包──▶ ringbuf(256 KiB) ──▶ userspace ──1 行/包──▶ packets.csv
                                          （慢 → ringbuf 满 → 丢包计数）

改造后：
  XDP ──AGG[window,key]++（per-CPU，计数不丢）
      └─首包/窗口/键/CPU──▶ ringbuf(256 KiB) ──▶ userspace 去重 ──▶ packets.csv
  XDP ──STATS(per-CPU) ──▶ userspace 求和 ──▶ stats 行
  userspace ──周期 scan: 读 AGG → 合并 → 删除──▶ aggregates.csv
```

## 3. 数据面设计

关键不变量：

1. **恒 `XDP_PASS`**：所有分支（解析失败、map 满、样本预留失败、配置缺失）都返回
   `XDP_PASS`；观察者不改转发行为。
2. **每包恰好一次 `bpf_ktime_get_ns()`**：同一个 `ts_ns` 同时决定
   `window_id = ts_ns / window_ns` 与 `first_seen`/`last_seen`，不存在“窗口归属用一个
   时钟、时间戳用另一个时钟”的偏差。
3. **每 CPU 独立聚合**：`AGG` 是 `PerCpuHashMap`，每个 CPU 独占自己的 value 槽位，
   数据面无跨 CPU 竞争、无锁。
4. **首包样本判定**：`aggregate()` 在 `count` 从 0 变 1 时返回 `true`，只有这个包尝试
   写样本；后续包只更新 `count`/`last_seen`。因此每窗口每键每 CPU 至多 1 条样本。
5. **失败不阻塞**：`AGG` 插入失败 → `STATS[AGG_OVERFLOW]++` 并跳过样本；ringbuf
   reserve 失败 → `STATS[SAMPLE_FAILED]++` 并把该槽位 `sample_dropped = 1`。
6. **配置回退**：`CONFIG` 缺失、`window_ns == 0`、前缀长度越界时使用安全默认
   （10 s、clamp 前缀），并累加 `STATS[CONFIG_MISSING]`；不会除零、不会丢包。

处理顺序（`try_probe`）：

```
parse_packet:
  Ethernet(14B) → 最多 2 层 VLAN(0x8100/0x88A8, 4B/层) → ethertype
    0x0800 → IPv4: version==4, ihl>=5, l3+ihl*4 在包内；
                    非首分片(frag_off&0x1fff!=0) 端口=0，否则按 TCP/UDP 取端口
    0x86DD → IPv6: 固定 40B base header，version 高 4 位==6；
                    protocol = base header Next Header；不遍历扩展头
    其它/截断 → 直接 PASS（不计数、不聚合）

成功解析:
  ts = bpf_ktime_get_ns()
  STATS[PACKETS_TOTAL]++
  cfg = CONFIG[0]，应用回退
  window_id = ts / cfg.window_ns
  key = AggKey{ window_id, rule_id=0, direction=1, family, src_prefix=mask(src,prefix), dst, dst_port, protocol, action=pass, reason=0, pad=0 }
  if aggregate(key, ts):          # 本 CPU/窗口/键首包
      emit_sample(SampleEvent{ ts, window_id, 原始地址/端口, protocol, family, action, reason, rule_id=0, reserved=0 })
  return XDP_PASS
```

map 容量与服务端内存的关系：`AGG` 131072 条是**每 CPU 一份 value**，容器 CPU 数越多
内存越大（详见 §4.3）；`SAMPLES` 256 KiB 固定；`STATS` 8 × 8 B × CPU 数；`CONFIG`
仅 32 B。

## 4. ABI 与 map

### 4.1 `SampleEvent`（64 B）

| offset | 字段 | 类型 | 说明 |
|---:|---|---|---|
| 0 | `ts_ns` | `u64` | 唯一一次 `bpf_ktime_get_ns()` |
| 8 | `window_id` | `u64` | `ts_ns / window_ns`（内核用 CONFIG 值） |
| 16 | `src_addr` | `[u8;16]` | 网络序；IPv4 左对齐补 0 |
| 32 | `dst_addr` | `[u8;16]` | 同上 |
| 48 | `src_port` | `u16` | 网络序；ICMP/无 L4 头 = 0 |
| 50 | `dst_port` | `u16` | 同上 |
| 52 | `protocol` | `u8` | IPv4 protocol / IPv6 base Next Header |
| 53 | `family` | `u8` | 4/6 |
| 54 | `action` | `u8` | Train 0 恒 0 = pass |
| 55 | `reason` | `u8` | Train 0 恒 0 |
| 56 | `rule_id` | `u32` | Train 0 恒 0 |
| 60 | `reserved` | `u32` | 恒 0 |

`size_of = 64`、`align_of = 8`；单测逐字段 `offset_of!` 钉住。**无版本字段、无版本
常量**。

### 4.2 `AggKey`（56 B）/ `AggValue`（32 B）/ `ProbeConfig`（32 B）

| 结构 | 布局 |
|---|---|
| `AggKey` | 0 `window_id` u64；8 `rule_id` u32；12 `direction` u8；13 `family` u8；14 `src_prefix[16]`；30 `dst_addr[16]`；46 `dst_port` u16；48 `protocol` u8；49 `action` u8；50 `reason` u8；51 `pad[5]`；`size_of = 56` |
| `AggValue` | 0 `count` u64；8 `first_seen` u64；16 `last_seen` u64；24 `sample_dropped` u8；25 `degraded` u8；26 `pad[6]`；`size_of = 32` |
| `ProbeConfig` | 0 `version` u32；8 `window_ns` u64；16 `grace_ns` u64；24 `ipv4_prefix_len` u8；25 `ipv6_prefix_len` u8；26 `default_action` u8；27 `pad[5]`；`size_of = 32` |

**56 B 的由来（有意调整字段顺序）**：为把记录压到 56 B，`rule_id` 放在 `window_id`
之后，再接 `direction`/`family` 与 16 B 数组，最后是显式 `pad[5]`。若按题面的“自然
顺序”（`window_id` 后先放 `direction`/`family`，`rule_id` 放到后面），中间会插入 2 B
对齐填充，最终对齐到 64 B。代价是字段顺序不再是“阅读顺序”，收益是每个 per-CPU 元素
少 8 B（131072 条 × 20 CPU 时约省 20 MiB 量级）。所有字段（含 pad）显式初始化，避免
未初始化填充导致同一逻辑键分裂成多个 map 条目。

### 4.3 map 契约与内存

| map | 类型 | 条目 | key/value 大小 | 用途 |
|---|---|---:|---|---|
| `CONFIG` | Array | 1 | 32 B | attach 前写入；数据面只读 |
| `AGG` | PerCpuHashMap | 131072 | key 56 B / value 32 B | 窗口内 per-CPU 计数 |
| `SAMPLES` | RingBuf | 256 KiB | 记录 64 B | 首包样本 |
| `STATS` | PerCpuArray | 8 | u64 | 计数器（索引 0..3 已用，4..7 预留） |

**内存：实测 + 公式**。per-CPU 哈希的值区是 `32 B × N_cpu`。本环境 20 vCPU，在 probe
运行中执行 `bpftool map show`（20 CPU、`AGG` 131072 条）实测：

| map | max_entries | key/value | memlock（实测） |
|---|---:|---|---:|
| `AGG` | 131072 | 56 B / 32 B | **100,664,832 B（96.0 MiB，约 768 B/元素）** |
| `SAMPLES` | 262144（B） | — | 275,864 B（≈269 KiB） |
| `CONFIG` | 1 | 4 B / 32 B | 416 B |
| `STATS` | 8 | 4 B / 8 B | 1,728 B |
| 合计 | — | — | **约 100,942,840 B（96.3 MiB）** |

复现命令（probe 运行期间；`<pid>` 为 probe 进程）：

```bash
grep -E '^Vm(Lck|RSS|Size):' /proc/<pid>/status     # 本内核用 memcg 记账，VmLck 为 0
bpftool map show | grep -A2 -E 'name (AGG|SAMPLES|CONFIG|STATS)'
```

实测 768 B/元素 = `key 56 B + value 32 B × 20 CPU（640 B）+ 内核元素头/对齐约 72 B`，
与 `56 + 640 + 元素头` 的 744–760 B 预期量级一致（预分配行为按 `max_entries`）。CPU 数
更少时 value 区近似线性下降（8 CPU 约 33 MiB、4 CPU 约 19 MiB，按同一公式推算，未逐一
实测）。注意本 WSL2 内核（6.6.87）用 memcg 记账，`/proc/<pid>/status` 的 `VmLck` 为 0，
**内存口径以 `bpftool map show` 的 memlock 为准**。

> 注：若后续要控制内存，可考虑 `BPF_F_NO_PREALLOC`（按实际键数分配）或把 `AGG`
> 改成 LRU/分片，均属设计变更，不在本阶段。

### 4.4 ABI 兼容政策

- 本阶段**没有事件版本号，也没有跨构建兼容承诺**：eBPF 对象由 `include_bytes_aligned!`
  嵌入 `probe`，对象与 loader 永远来自同一次构建。任何布局变化都是破坏性变更，必须整
  仓重编，**禁止新 loader 配旧对象或反之**。
- 只有将来对象需要脱离 loader 独立分发时，才重新引入版本字段，并从 0 起算。
- 用户态保留三道防御：① ringbuf 记录长度必须**恰好** 64（短/长均 `malformed`，不按
  错误布局读取）；② `family` 必须为 4/6；③ 窗口归属用写入 CONFIG 的**同一个**
  `window_ns` 复核，不使用第二份本地常量。

## 5. 用户态：扫描、合并、去重与迟到

启动阶段：`--window-ms`/`--grace-ms`/`--src-prefix-v4`/`--src-prefix-v6` 换算后写入
`CONFIG`，**回读校验**，再 load/attach XDP；之后所有窗口边界、前缀渲染都使用回读值。

周期循环（tokio `select!`）：

1. **样本通道（每 `--flush-ms`）**：drain `SAMPLES` ringbuf → 校验长度/family →
   `AggKey::from_sample(回读 config)` 重建键 → `SampleDedup::ingest`。同一
   `(window_id, key)` 只保留 `ts_ns` 最小的一条，重复计入 `samples_deduped`；已上报
   窗口的样本计入 `late_dropped`，不写 CSV。
2. **扫描通道（每 `--scan-ms`）**：用 `now_mono` 计算
   `watermark = (now - grace) / window_ns - 1`（`window_ns == 0` 或尚无窗口关闭时为
   `None`）。先**收集**：遍历 `AGG` 中 `window_id <= watermark` 的 key，读 value →
   `AggTable::ingest_slot` 合并（`count` 求和、`first_seen` 取 min、`last_seen` 取
   max、标志取或；**只有 `count > 0` 的槽位参与时间戳**）→ **再删除**该 key。
3. **上报**：`AggTable::take_closed(watermark)` 输出关闭窗口的行
   （`window_complete = 1`），按 `(window_id, key)` 排序后写聚合 CSV；水位线推进到
   watermark（空窗口也推进，避免旧窗口被重复上报）。
4. **迟到**：`window_id <= 已上报水位线` 的槽位/样本不再入表，计入 `late_dropped`；
   同一窗口绝不会写出第二行。
5. **退出**：SIGINT/SIGTERM → 最后一次 drain 样本 → 关闭可关闭窗口 → 对仍打开的窗口
   `take_pending()` 写 `window_complete = 0` 的 partial 行 → flush 两份 CSV → detach
   XDP → 打印固定 token 的 stats 行。SIGKILL 不保证 partial 落盘。
6. **周期指标**：每 5 s `info!` 输出一次样本/聚合/丢弃等增量指标；数据面无日志。

CSV 契约：

- 样本 7 列：`ts_unix_ms,iface,src_ip,dst_ip,src_port,dst_port,protocol`（端口恒为整数，
  ICMP 为 0）。
- 聚合 16 列：`window_start_ms,window_end_ms,iface,direction,src_prefix,dst_addr,dst_port,protocol,action,rule_id,count,first_seen_ms,last_seen_ms,sample_dropped,degraded,window_complete`；
  `direction` 为 `ingress`，`action` 为 `pass`/`block` 文本，`src_prefix` 为 CIDR。
- 退出 stats 固定 token：
  `packets_total sample_rows events_received rows_written agg_records kernel_dropped sample_failed agg_overflow malformed late_dropped samples_deduped`；
  其中 `kernel_dropped = Σ STATS[SAMPLE_FAILED]`（保留旧字段名），`rows_written`
  恒等于 `sample_rows`（兼容旧脚本）。

## 6. 验收 A1–A11 证据

> 编号与检查项按 Lead 任务书；A2–A7 对应 `verification/run_aggregation.sh`，A9 对应
> `verification/run_perf.sh`，其余为按当前设计给出的建议项，若与正式验收清单定义不同，
> 以 Lead 为准。

| 编号 | 检查项 | 命令 | 结果与证据 |
|---|---|---|---|
| A1 | 构建、单测与静态检查（布局单测钉住 64/56/32 B） | `cd /workspace/probe && cargo build --release && cargo test` | **PASS**：build exit 0；`cargo test` 共 **27 passed**（probe 21 + probe-common 6，0 failed） |
| A2 | 3 个 ICMP echo → 聚合 1 行 `count=3`、样本 1 行、`window_complete=1` | `cd /workspace/verification && ./run_aggregation.sh`（phase A2） | **PASS**：`Σcount=3`、聚合 1 行、`window_complete=1`、样本 1 行、窗口跨度 4000 ms、`sample_rows=1 agg_records=1` |
| A3 | 两个源地址同属 /24 → 1 行 `src_prefix=10.99.0.0/24`、`count=6`、样本 1 行 | 同上（phase A3） | **PASS**：`Σcount=6`、聚合 1 行 `src_prefix=10.99.0.0/24`、`window_complete=1`、样本 1 行 |
| A4 | `ping -f -c 1000` 泛洪 → `Σcount=1000`，每键每窗口样本行有上界 | 同上（phase A4） | **PASS**：flood ping 1000/1000、`Σcount=1000`、聚合 1 行、样本 1 行、`packets_total=1000` |
| A5 | >100k pps UDP 5 s → `Σcount` 误差 ≤1%，`agg_overflow` 不增长 | 同上（phase A5） | **PASS**：发送 800,000（实测 160,023 pps）、`Σcount=800,000`（误差 0.00%）、100,000 聚合行、样本 44,946 行、`agg_overflow=0`；`sample_failed=103,414` 且 `sample_dropped` 行 81,144（预期降级，计数不受影响） |
| A6 | 相邻两个窗口 → 2 行、`Σcount=总数`、窗口边界相接、无重复窗口 | 同上（phase A6） | **PASS**：2 行、`Σcount=6`、两行 `window_complete=1`、窗口 `1794250606756..1794250610756` 与 `1794250610756..1794250614756` 首尾相接、无重复窗口、样本 2 行 |
| A7 | 窗口未关闭时 SIGINT/SIGTERM → partial 行 `window_complete=0` + stats token 完整 | 同上（phase A7） | **PASS**：SIGINT 与 SIGTERM 各 `Σcount=3`、聚合 1 行 `window_complete=0`、样本 1 行、stats token 齐全、无 `late_dropped`/`malformed` |
| A8 | IPv4/IPv6 端到端回归：attach、`run_cnt`、`--append`、VLAN、`protocol=58`、无残留 | `./run_e2e.sh`；`./run_e2e_ipv6.sh` | **PASS**：IPv4 最终运行（19:01:46）`RESULT: PASS`，**58 条 CHECK PASS / 0 FAIL**（`e2e.log`、`/tmp/final-e2e.out`、聚合 CSV `packets-agg.csv`）；IPv6 `RESULT: PASS`，**89 条 CHECK PASS / 0 FAIL**（`ipv6-e2e.log`、`/tmp/v-ipv6-3.out`） |
| A9 | 新旧实现性能对比 | `MODE=old bash /workspace/verification/run_perf.sh`；`MODE=new bash ...` | **PASS**：见 §7；原始数据 `verification/perf/perf-summary.tsv`、`perf-{old,new,a5}-summary.txt` |
| A10 | ABI/map 契约：布局单测 + 20 vCPU 下 `AGG` 实测内存 | `cargo test`；运行中 `bpftool map show` | **PASS**：布局测试通过（A1）；`AGG` memlock 实测 **100,664,832 B ≈ 96.0 MiB**（§4.3） |
| A11 | 清理与文档一致性：退出后无 XDP attach/无 probe 进程残留；README 与代码/脚本契约一致 | `bpftool net list`；`pgrep -x probe`；文档逐项核对 | **PASS**：IPv4（19:01:46）与 IPv6（18:54:46）最终 e2e 均 `RESULT: PASS`，`no XDP attachment left after cleanup` / `no probe process left` / `netns+veth removed` 全 PASS；跑完 `bpftool net list` 的 `xdp:` 为空、`pgrep -x probe` 无输出；README 已按 `probe-common`/eBPF/CLI/双 CSV/stats token 契约逐项核对 |

### 6.1 已回填摘要

- **A1**：`cargo build --release` 通过；`cargo test` 27 passed（probe 21 + probe-common
  6），其中 `probe-common` 的 `offset_of!` 单测把 `SampleEvent` 64 B / `AggKey` 56 B /
  `AggValue` 32 B / `ProbeConfig` 32 B 的每个字段偏移钉死。
- **A8**：IPv4 回归中 phase A 为 `count=3 / window_complete=1`，phase B 因窗口未关闭
  即被 SIGTERM 停止而写出 `count=3 / window_complete=0` 的 partial 行，VLAN phase 为
  `count=3`；IPv6 回归 phase AB 在 veth0/veth1 各见 `count=6`（每个 probe 的 ingress
  同时看到 request + reply；ICMPv6 端口为 0，同属一个 key），合计 12；VLAN phase
  `count=3`；phase D 混合流量 v4=`count=3`、v6=`count=3`；每个 ULA key 恰好 1 条样本
  行。证据：`verification/{e2e.log,ipv6-e2e.log}`、
  `verification/{packets-agg.csv,ipv6-packets-agg.csv,ipv6-vlan-agg.csv,ipv6-mixed-agg.csv}`、
  `/tmp/v-e2e-final.out`、`/tmp/v-ipv6-3.out`。
- **A2–A7**：2026-11-09T18:57:03 `RESULT: PASS`，0 FAIL / 0 SKIP。要点：A2
  `Σcount=3`、1 行、`window_complete=1`；A3 两个源地址同 /24 折叠成 1 行
  `10.99.0.0/24`、`Σcount=6`；A4 flood 1000/1000、`Σcount=1000`；A5 发送
  800,000、`Σcount=800,000`（0.00%）、100,000 聚合行、44,946 样本行、`agg_overflow=0`
  （`sample_failed=103,414` 属多 key 首包采样超限的预期降级）；A6 两个相邻窗口
  `Σcount=6`、边界相接、样本 2 行；A7 SIGINT/SIGTERM 各写出 `window_complete=0` 的
  1 条 partial 行、`Σcount=3`，stats token 完整。证据：`verification/agg-e2e.log`、
  `/tmp/v-agg-2.out`、`verification/agg-a{2,3,4,5,6,7-INT,7-TERM}.csv` 及对应
  `*-samples.csv`。
- **A9**：见 §7（100k pps × 5 s，old/new 同一流量；另含多 key 压测场景）。
- **A10**：布局单测见 A1；内存实测见 §4.3（`AGG` ≈96.0 MiB，四 map 合计 ≈96.3 MiB）。
- **A11**：IPv4（19:01:46）与 IPv6（18:54:46）最终 e2e 均 `RESULT: PASS`，两脚本的
  `no XDP attachment left after cleanup` / `no probe process left` / `netns+veth removed`
  全部 PASS；收尾时 `bpftool net list` 的 `xdp:` 段为空、`pgrep -x probe` 无输出。
  说明：IPv6 唯一一次与 A9 perf 并发的旧运行（18:41 左右）曾留下 `perf0(183)`，该次
  数据已废弃，最终证据以 `/tmp/v-ipv6-3.out` 的独占运行为准。脚本中
  `awk '/^xdp:/{...} /^[a-z]/{...}'` 的段解析对“以字母开头的接口行”有盲区（建议
  后续改为 `xdp:` 段到空行），但本轮最终运行已用独立 `bpftool net list` 确认无残留。
  文档侧：README 已与 `probe-common`/eBPF/CLI/双 CSV/stats token 契约逐项核对，无旧
  ABI 措辞。

### 6.2 验收结论与证据索引

- 结论：**A1–A11 全部 PASS**（A2–A7 `RESULT: PASS`，0 FAIL / 0 SKIP；
  A8 IPv4/IPv6 两套 e2e `RESULT: PASS`）。
- 聚合验收证据：`verification/agg-e2e.log`（完整 CHECK 列表）、`/tmp/v-agg-2.out`、
  `verification/agg-a2{,3,4,5,6,7-INT,7-TERM}.csv` 与对应 `*-samples.csv`、
  `agg-e2e.log` 末行 `RESULT: PASS`。
- 回归证据：`verification/e2e.log`、`ipv6-e2e.log`、`packets-agg.csv`、
  `ipv6-packets-agg.csv`、`ipv6-vlan-agg.csv`、`ipv6-mixed-agg.csv`；
  `/tmp/v-e2e-final.out`、`/tmp/v-ipv6-3.out`。
- 性能证据：`verification/perf/`（`perf-summary.tsv`、`perf-{old,new,a5}-summary.txt`、
  `perf-a5-agg.csv`、`RESULTS-A9.md`）。
- 内存证据：运行期 `bpftool map show`（§4.3）。

## 7. 性能对比

测量方法：`verification/run_perf.sh` 在独立 `perf-ns`/`perf0` 拓扑上以固定 pps、固定
时长打 UDP 流，分别跑旧二进制（`MODE=old`，`PROBE_OLD=/tmp/probe-old`）与新实现
（`MODE=new`）；用户态 CPU% 来自 `/proc/<pid>/stat` 的 utime+stime，`Σcount` 误差 =
（发送数 − 聚合 `Σcount`）/ 发送数。原始数据见 `verification/perf/perf-<mode>-summary.txt`
与 `verification/perf/perf-summary.tsv`。

固定条件：`PPS=100000`、`DUR=5`、`PAYLOAD=64`、UDP 目的端口 9999、`FLUSH_MS=500`；
新实现另加 `--window-ms 10000 --grace-ms 2000 --scan-ms 1000`。旧/新串行执行，无并发；
`perf0`/`perf1` 两端 `disable_ipv6=1`。发送端 `sent` 为权威到达数（recv 只作参考）。

| 实现 | 发送（包） | 写入行每秒 | Σcount 误差 | 用户态 CPU% | kernel_dropped |
|---|---:|---|---:|---:|---:|
| 旧（逐包 48 B 事件） | 500,000（100k pps，4999 ms） | 51,552 sample 行 / **10,302 行/秒** | **89.69%**（CSV 行 51,552 vs 500,000） | **0.20%**（schedstat 0.185%） | **448,448** |
| 新（per-CPU 聚合 + 首包样本，单 key） | 500,000（100k pps，4999 ms） | 1 条 sample + 1 条 agg 行（**样本 0.2 行/秒**，退出时一次性写聚合行） | **0.00%**（`Σcount=500,000`，`agg_rows=1`） | **0.00%**（低于 1 tick；schedstat 0.042%） | **0**（`sample_failed=0`） |
| 新（多 key 压测，50,000 个 dst_port） | 500,000（100k pps，4999 ms） | 50,000 agg 行 + 9,980 sample 行 / **1,450 行/秒** | **0.00%**（`Σcount=500,000`） | **2.20%**（schedstat 2.265%） | **0**（`sample_failed=86,996`） |

读数说明：

1. **旧实现的 10,302 行/秒是逐包写盘**，但只收到 51,552 / 500,000 个事件——
   `kernel_dropped=448,448`，计数误差 89.69%；其用户态 CPU 极低（0.20%）说明瓶颈是
   ringbuf 容量与 500 ms flush 节拍，不是 CPU。
2. **新实现单 key 的“写入行/秒 = 0”不是性能为 0**：每窗口只有 1 个 key，正常扫描不
   产生任何行，退出（SIGINT）时才 flush 出 1 条聚合行 + 1 条样本；关键是
   `Σcount=500,000` 精确、`kernel_dropped=0`。多 key 场景（A5，50k keys/窗口）才会
   有持续的样本/聚合行写入（1,450 行/秒），此时 `sample_failed=86,996`（样本槽位不足）
   但 `Σcount` 仍精确。
3. 用户态 CPU% 取 `/proc/<pid>/stat` 的 utime+stime（21 线程合计；`CLK_TCK=100`），
   并用 `/proc/<pid>/schedstat` 交叉核对；**BPF/XDP softirq 不在该进程内**，不计入
   “用户态 CPU%”，数据面成本另见 `system_cpu_pct` 与 `kernel_dropped`。
4. 旧实现基线复跑 9 次：CSV 51,520–51,668 行（10,292–10,321 行/秒），
   `kernel_dropped=448,332–448,486`，误差 89.67%–89.70%，非常稳定；新实现复跑 4 次，
   `Σcount` 恒 500,000（0.00%），`kernel_dropped=0`。
5. 运行期开销旁证（A5 50k keys）：周期扫描 `scan_ms=18 ms`（`--scan-ms 1000`），
   SIGINT → 进程退出（含 flush 50k 聚合行）**233 ms**，`agg_overflow=0`。
6. 原始数据：`verification/perf/perf-summary.tsv`、`perf-old-summary.txt`、
   `perf-new-summary.txt`、`perf-a5-summary.txt`、`perf-a5-agg.csv`、
   `verification/perf/RESULTS-A9.md`。

## 8. 未解决项与风险

| 项 | 说明 | 影响 | 处理 |
|---|---|---|---|
| `AGG` 内存随 CPU 放大 | value 区 = 32 B × CPU 数；20 vCPU 实测 768 B/元素、合计 96.0 MiB | 高 CPU 数容器内存占用高、memlock 需求大 | 本阶段接受；必要时改 no-prealloc/LRU/分片 |
| ringbuf 满丢样本 | 256 KiB，用户态按 `--flush-ms` 轮询；多 key 实测 `sample_failed` 8.7 万–10.3 万（A5：50k keys × 2 窗口 = 100k 聚合行；A9 前置：50k keys） | 只丢样本；`sample_dropped=1` 可见，`Σcount` 不受影响 | 接受；可调 flush/ringbuf 或增大采样容量 |
| SIGKILL 丢 partial | 强杀不走退出路径 | 未关闭窗口的数据丢失 | 文档声明；运维用 SIGTERM |
| 无事件版本号 | 对象嵌入二进制，跨构建不兼容 | 混用新旧二进制会 `malformed` 或错乱 | 整仓重编；独立分发时才加版本 |
| 实测与估算一致性（已关闭） | 实测 768 B/元素、`AGG` 96.0 MiB，与 744–760 B/元素估算一致 | 无偏差 | 已回填 §4.3 |
| 迟到数据计数口径（已确认） | `late_dropped = AggTable.late_dropped() + SampleDedup.late_dropped()`（`probe/src/main.rs`） | 指标同时含聚合槽位与样本两处迟到 | README 与 stats 描述已按此口径 |
| 窗口相位依赖 ktime | 窗口按内核单调时钟整除，不对齐 wall-clock 整边界；首个窗口可能早于进程启动 | 按 wall-clock 整秒/整分断言会误判 | 文档说明；测试按相对窗口关系断言 |
| e2e 清理断言解析盲区 | `run_e2e*.sh` 用 `awk '/^xdp:/{...} /^[a-z]/{...}'` 解析 `bpftool net list`，以字母开头的接口行（如 `perf0(...)`）会被当作新段头跳过 | 并发测试时可能漏报非本脚本的残留挂载 | 属 C4 脚本问题；建议按“`xdp:` 段到空行”解析 |
| 网关/转发场景未验证 | 仅 ingress、无 MAC 重写/转发 | 不代表“可上线” | 文档声明；native 未验证 |

## 9. 未做范围

- **E6 规则匹配**：无规则表、无匹配逻辑；`action` 恒 `pass`、`rule_id = 0`、
  `reason = 0`，`default_action` 不生效，不丢弃任何包。
- **E8 上报**：无控制面连接、无审计/上报通道；数据只落两份本地 CSV。
- **IPv6 扩展头遍历**：只读 base header，扩展头链不解析（`protocol` 可能是扩展头号）。
- **egress / 双向同进程**：仅 ingress，一个进程一个接口；veth 两端需两个进程。
- **L4 深度解析**：只取 TCP/UDP 前 4 字节端口，不解析 flags/payload/应用层；非首分片
  端口为 0。
- **native XDP 适配**：WSL2 下 veth 走 driver、`lo` 走 generic；native 驱动未验证，
  未输出 native 准入清单。
- **数据库 sink**：`--sink` 仅 `csv`，DB 后端只留 trait 扩展点。
- **跨构建/跨版本 ABI 兼容**：本阶段明确不承诺；不提供旧对象/新 loader 的组合。
- **长时间运行的数据保留策略**：无窗口级 TTL/归档，CSV 持续追加（`--append` 语义由
  调用方控制）。
