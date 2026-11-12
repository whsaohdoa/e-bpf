# probe 功能测试工程化方案（Train 0 → 可持续）

- 日期：2026-11-10
- 范围：`probe/`（workspace：`probe`、`probe-common`、`probe-ebpf`）+ `verification/` 现有验收资产
- 结论摘要：现有功能测试**能证明一次验收，不能防止回归**。方案核心是**把 90% 的功能断言从内核搬到进程内**（L2 层），真机只保留 5 条连通性用例；bash 从"驱动 + 断言"退化为"环境搭建"；断言、报告、覆盖率全部机器化。

---

## 0. 一页纸

| 维度 | 现状 | 目标 | 关键手段 |
|---|---|---|---|
| 用例规模 | 27 个单测（全部纯逻辑）+ 4 个手工 bash | L0–L4 约 55 条，全部有稳定 test id | 分层 + 表驱动 |
| 未覆盖代码 | 1,210 行（`main.rs` 747 + `probe-ebpf/src/main.rs` 463，占 3,011 行 Rust 的 40%）零单测 | 用户态 ≥ 90% 行覆盖（排除 `main.rs`/`app.rs` 壳与接线），解析器 ≥ 95% | 端口化 + 解析器下沉 |
| 反馈时间 | 全套手工 ≈ 3–4 min（实测 agg 63 s、ipv6 29 s、另加构建），但要 root + 手工触发 + 人工读日志 | PR gate < 2 min（L0–L2），e2e < 6 min | 无 root 的进程内管线 |
| 失败信息 | 只有 `CHECK FAIL: <描述>` | `assert_eq` 级 diff（行级 CSV 差异） | Rust driver + golden CSV |
| 并行 | 不能（固定 IP `10.99.0.0/24`、固定 netns 名） | 每条用例独立 netns/veth/子网，可 `-j` 分片 | 用例内自建环境 |
| 确定性 | 依赖 ping 回显 + 窗口相位校准 + 20 s 超时轮询 | 虚拟时钟 + 定帧发包，零固定 sleep | 注入时钟 + AF_PACKET 定帧 |
| CI | 无 | 三档 gate（PR / main / nightly） | GitHub Actions + 特权容器 |
| 证据 | 手抄进 `REPORT-aggregation.md` §6 | JUnit XML → 自动生成报告 | `cargo xtask report` |

工作量估算：改造 8–10 人日 + 用例编写 5–6 人日 ≈ **14–15 人日**（单人 3 周 / 双人 2 周），可拆成 5 个可独立交付的 Sprint。

---

## 1. 现状判据：粗糙在哪

| # | 判据 | 证据 | 后果 |
|---|---|---|---|
| G1 | 无 CI、无统一入口，测试靠人记命令 | 仓库无 `.github/`、无 `Makefile`/`xtask`；`verification/*.sh` 只能手工 `bash` | 回归不可阻断，PR 无法验证，新人无法复现 |
| G2 | 测试资产不在版本控制 | 根目录无 `.git`（只有 `probe/.git`、`eBPF/.git`），`verification/`、`docx/` 完全 untracked | 测试脚本无 review、无历史、一次误删即永久丢失 |
| G3 | 断言只有"计数 + 子串" | `verification/run_aggregation.sh:70-79` 的 `check()` 把被测命令输出重定向进日志，失败只剩一行描述 | 失败不可诊断：`test 5 -eq 3` 不打印实际值，无行级 diff，无法定位是哪一列错 |
| G4 | 用例共享全局状态、无隔离 | `run_aggregation.sh:27-31` 注释明说必须与 `run_e2e.sh` 错开跑（都用 `10.99.0.0/24`）；phase A2–A7 顺序执行于同一 netns | 不能并行、不能单跑、一条挂后续级联 |
| G5 | 依赖真实流量与时序技巧 | 用 `ping`/`ping -f`/`traffic_gen`；`:118-160` 先"校准窗口相位"再等边界，20 s 超时用 `sleep 0.05` 轮询 | 每个 phase 数秒~十几秒，且结果随机器负载漂移；CI 上必然 flaky |
| G6 | 数据面解析零单测 | `probe-ebpf/src/main.rs` 463 行，`#[test]` 0 个；双层 VLAN、非首分片、`ihl>5`、截断包只在 e2e 覆盖 | 解析边界无回归保护，而这些正是最容易写错的代码 |
| G7 | 用户态主循环零单测 | `probe/src/main.rs:134` 的 `main()` 从 `Opt::parse` 一路到 `attach_xdp` 全耦合；`report_closed()` 在 `:459` 直接吃 `&mut AggMap` 并在 `:470` 内部调 `mono_now_ns()` | `drain_ring`/`scan_agg`/`report_closed` 无法在无 root 环境测；所有错误分支不可达 |
| G8 | 错误路径不可注入 | `agg_overflow`/`sample_failed`/`malformed`/`CONFIG` 缺失只能靠压测偶发（A5 的 103,414 次 `sample_failed` 是"观察到的"，不是"断言的"） | 最关键的降级语义（配置坏了也不丢包、样本失败不影响 count）没有确定性验证 |
| G9 | 证据手工誊抄 | `REPORT-aggregation.md` §6 的 PASS 与数字是人写的 | 文档与代码必然漂移，且无法作为 gate |
| G10 | 平台耦合 | `.cargo/config.toml` 全局 `runner = "sudo -E"`；脚本硬编码 `/workspace/...` | 任何人 `cargo test` 都要 sudo；路径不可移植 |
| G11 | 退出码截断（潜在假绿） | `run_aggregation.sh:831` `exit "$FAIL"`：`FAIL=256` 时 shell 取模 256 → **exit 0，失败跑成绿** | 用例数增长后这是必然踩到的坑 |

被判"粗糙"的不是脚本长，而是：**断言粒度太粗、依赖真实内核时序、失败信息无价值、无隔离、无 gate**。四条各自独立可修。

---

## 2. 设计原则

1. **确定性优先**：虚拟时钟 > 相位校准；定帧发包 > 随机 ping；表驱动 fixture > 现场造流量。
2. **单一真源**：解析规则只有一份代码（`probe-common`），L1 单测与 L3 真机共用同一批帧 fixture 与同一批期望值。
3. **能下沉就下沉**：能在 L0/L1 断言的绝不下沉到 L3。真机只证明"能 attach、字节真的进了 ringbuf"。
4. **失败必须自解释**：断言失败要打印 expected/actual 与行号；禁止 `test`/`grep -q` 这种二值断言作为最终判据。
5. **证据自动生成**：JUnit XML + 归一化 CSV 是唯一事实来源，`REPORT.md` 由脚本生成而非手写。
6. **快**：PR 反馈 < 2 min，否则开发者会绕过。

---

## 3. 分层架构

| 层 | 名称 | 被测对象 | 依赖 | 用例目标 | 耗时 | 何时跑 |
|---|---|---|---|---|---|---|
| **L0** | 纯逻辑单测 | `aggregate.rs`、`sink.rs`、`probe-common`（含新 `parse.rs`） | 无（纯 `cargo test`） | 45 | < 3 s | 每次保存 / PR |
| **L1** | 契约测试 | CLI 参数校验、CSV schema、`stats:` token、README 与 CLI 一致性 | 无（`assert_cmd` + `insta`） | 14 | < 5 s | PR |
| **L2** | 进程内管线 | `drain_ring`/`scan_agg`/`report_closed`/sink 全链路，用**假内核**（内存 map + 脚本化样本源 + 虚拟时钟） | 无 root、无网卡 | 16 | < 2 s | PR |
| **L3** | 真机 e2e | attach/XDP 路径、真实 ringbuf、真实 per-CPU 合并、真实解析 | root + veth/netns（容器内） | 7 | < 6 min | main 合并 / 夜间 |
| **L4** | 长稳 / 性能 / 故障 | 吞吐、`Σcount` 守恒、溢出、SIGKILL、抖动率 | 同 L3 | 5 | 30 min | nightly |

**关键判断：现有 A2/A3/A4/A6/A7 的大部分断言其实考的是用户态聚合语义，不需要内核。** 把它们搬进 L2 后，这 5 个验收项从"几十秒 + 真实流量 + 时序技巧"变成"毫秒级 + 完全确定"，且可以在 Windows/macOS 上跑。

---

## 4. 关键改造（按依赖顺序）

### E1 主循环端口化：让"内核"可替换（1.5 人日）

现状 `report_closed(agg: &mut AggMap, ...)` 与 `mono_now_ns()` 把内核句柄与时钟焊死在管线里。抽出三个端口：

```rust
// probe/src/ports.rs
pub trait AggStore {
    /// 遍历 AGG 的全部 per-CPU 槽位（未合并的原始槽位）。
    fn for_each_slot(&self, f: &mut dyn FnMut(&AggKey, &AggValue));
    fn delete(&mut self, key: &AggKey);
    fn len(&self) -> usize;
}

pub trait SampleSource {
    /// 下一条 ringbuf 记录的原始字节；None = 已空。
    fn next_record(&mut self) -> Option<&[u8]>;
}

pub trait TimeSource { fn mono_ns(&self) -> u64; }
```

- `AggMap`/`SampleRing` 各实现一次（约 60 行，`unsafe` 不变）。
- `report_closed(agg: &mut dyn AggStore, ..., now_ns: u64)`；`scan_agg` 同理。
- `main()` 变为 `main() -> ExitCode { validate → run(opt) → 打印 stats }`，`run()` 返回 `Summary`，**纯函数边界**。

收益：L2 全套用例（含 `agg_overflow`/`malformed`/磁盘失败）不需要 root；可测逻辑从 `main.rs` 移到 `pipeline.rs`，`main.rs` 退化为壳（覆盖率口径随之修订，见 §8）。

**口径修订（2026-11-12，S2 落地时确认）**：拆分后 `main.rs` 只剩 8 行 `probe::app::main()`，`app.rs` 是 aya/tokio 接线，两者在 host 上都执行不到。本计划原先的「`main.rs` 覆盖率 ≥60%」**不再适用**，改判「承接这些逻辑的 `pipeline.rs` 行覆盖 ≥85%」，`main.rs`/`app.rs` 按文件名排除（与 `scripts/ci/coverage.sh` 的 `--ignore-filename-regex '(^|/)(main|app)\.rs$'` 一致）。这是口径调整而不是达标：被排除的两处代码仍然只在真机路径上被验证。
风险：低。`Sink`/`AggSink` 已经是 `&mut dyn`，风格一致。

### E2 解析器下沉到 `probe-common`（2 人日，唯一有技术风险项）

现在 `try_probe` 把"边界检查 + 解析 + 聚合"揉在一起，只能整体上真机。抽出纯解析：

```rust
// probe-common/src/parse.rs —— no_std，内核与用户态共用同一份
pub trait Pkt {
    /// 取 [off, off+len) 的字节；越界返回 None（eBPF 侧即 data/data_end 检查）。
    fn bytes(&self, off: usize, len: usize) -> Option<&[u8]>;
}

pub fn parse_ip(p: &impl Pkt) -> Option<L3Info> { /* eth → vlan×2 → v4/v6 → l4 ports */ }
```

- 内核侧 `struct XdpPkt { data: usize, data_end: usize }` 实现 `Pkt`，函数体就是现有 `ptr_at` 的检查，`#[inline(always)]` + 单态化，**零动态派发**。
- 主机侧 `impl Pkt for &[u8]`，于是同一批 fixture 在 L1 跑单测、在 L3 真机重放。
- 风险：verifier 可能拒绝"通过 trait 方法取切片"的写法。**先用 1 天做 spike**：若 `cargo +nightly build --target bpfel-unknown-none -Z build-std=core` + 一次 attach 通过则采纳；否则退回备选方案——把首 128 B 拷到栈上数组再解析（多一次 128 B 拷贝/包，用 A9 的性能基线守住 ≤2% 回退）。

### E3 确定性发包：用帧表替换 ping（1.5 人日）

`ping` 有三个不可控：内核回显导致双向计数、ICMP 限速、回显依赖对端协议栈。改为 AF_PACKET 直发定帧：

```
probe/tests/fixtures/frames/
  icmp_v4.pcap  tcp_v4_opt.pcap  udp_v4_frag2.pcap  vlan_ipv6_udp.pcap
  qinq_v6.pcap  truncated.pcap  ...
  index.tsv     # file  family src dst sport dport proto accept
```

`index.tsv` 一份，L1 读它断言 `parse_ip()`，L3 读它发帧后断言 CSV 行 —— 这就是原则 2 的落地。发包器为 `xtask send --frames ... --count N`，**发送前 arm、发送后 close，返回精确 sent 数**，`count==N` 是硬断言而不是"大约"。

### E4 CSV 归一化 + golden 快照（1 人日）

- 归一化函数（测试专用）：`ts_unix_ms`/`first_seen_ms`/`last_seen_ms` → `now - base` 的相对毫秒；`window_start_ms` → `window_id`；`iface` 保留。归一化后 CSV 才是可比对的。
- 用 `insta` 存 `tests/snapshots/*.csv`，diff 直接显示哪一列变了。
- 归一化器本身要有单测（防止"归一化把真差异吃掉"）。

### E5 bash → Rust driver，bash 只搭环境（2 人日）

- 环境自建：`E2eEnv::new(case_id)` 创建 `pe2e<pid>_<case>` netns + 唯一 veth + `10.<128+shard>.<case>.0/24` 子网，`Drop` 里清理。**每用例独立 → 天然并行、可单跑、无级联**（解决 G4）。
- 断言在 Rust：`assert_eq!(rows, want!("..."))`，失败打印归一化 diff + 原始 CSV 路径。
- `verification/*.sh` 保留为 `xtask e2e --legacy` 的历史对照，一个 Sprint 后删除。
- 顺带修 G11：`exit "$FAIL"` → `[ "$FAIL" -eq 0 ] || exit 1`。

### E6 单入口 `cargo xtask`（0.5 人日）

```
cargo xtask test [--layer l0|l1|l2|all]   # 默认 all，无 root
cargo xtask e2e  [--case e2e-03]          # 需 root/特权容器
cargo xtask soak --minutes 30
cargo xtask report                        # JUnit + 归一化 CSV → REPORT.md
cargo xtask bpf                           # 只编译 eBPF 目标（1 min 快速门）
```

同时删掉 `.cargo/config.toml` 的全局 `runner = "sudo -E"`（G10）——特权只属于 e2e 入口。

---

## 5. 用例清单

约定：`U-` L0、`C-` L1、`P-` L2、`E-` L3、`S-` L4；"覆盖"列写对应验收项（现有 A1–A11 或新增 A12–A15）。

### 5.1 L0 纯逻辑（现有 27 条保留，新增 12 条）

| id | 场景 | 断言 | 覆盖 |
|---|---|---|---|
| U-ABI-01..06 | 现有布局单测 | `size_of`/`offset_of` 钉死 64/56/32 B | A1 A10 |
| U-PREFIX-01 | `mask_src_prefix` 幂等（proptest，任意 16 B × 任意合法长度） | `f(f(x)) == f(x)` | A12 |
| U-PREFIX-02 | 边界：`/0` 全零、`/32` 原值、`/128` 原值、IPv4 只动前 4 B | 逐字节相等 | A12 |
| U-KEY-01 | `sort_records` 是全序且稳定（proptest） | 反对称 + 传递 + 同集合排序结果唯一 | A3 |
| U-WIN-01 | `window_start/end` 在 `u64::MAX`、`window_ns` 不整除时无溢出 panic | 关系式成立 | A6 |
| U-WM-01 | `closed_window_watermark(now<grace)` 返回 `None` | 边界三态 | A6 A7 |
| U-FMT-01 | IPv6 RFC 5952：`::`、`::1`、末尾零压缩、嵌入 IPv4 不压缩 | 字符串精确 | A8 |
| U-FMT-02 | 未知 action/direction → `unknown`；`rule_id` 恒 0 的渲染 | 字符串精确 | A3 |
| U-PORT-01 | 端口 0/65535/网络序 `from_be` 往返 | 整数相等 | A8 |
| U-CLOCK-01 | 负单调偏移、跨 epoch、毫秒舍入、`window_end-window_start == window_ms` | 关系式 | A6 |

### 5.2 L1 契约（14 条，新建 `probe/tests/cli.rs`、`csv_schema.rs`）

| id | 场景 | 断言 | 覆盖 |
|---|---|---|---|
| C-CLI-01 | `--flush-ms 0` / `--window-ms 0` / `--scan-ms 0` | exit 1，stderr 含参数名 | A14 |
| C-CLI-02 | `--src-prefix-v4 33` / `--src-prefix-v6 129` | exit 1 | A14 |
| C-CLI-03 | `--window-ms u64::MAX` | exit 1，不 panic、不写出空 CSV | A14 |
| C-CLI-04 | 未知 `--sink`、未知 flag | exit 2（clap），stderr 快照 | A14 |
| C-CLI-05 | `--help` 全文快照（insta） | 与基线逐字一致 | A14 |
| C-SCHEMA-01 | 样本 CSV：7 列、类型、端口必为整数（含 0）、header 仅一次 | 用 `csv` crate 严格解析 | A8 |
| C-SCHEMA-02 | 聚合 CSV：16 列、`src_prefix` 是合法 CIDR、`action∈{pass,block}`、`window_complete∈{0,1}` | 严格解析 | A6 |
| C-SCHEMA-03 | `--append` 时 header 不重复、`O_APPEND` 并发写不互相覆盖 | 两 writer 各写 N 行 → 2N 行可解析 | A8 |
| C-STATS-01 | `stats:` 行 token 集合与顺序固定、值全为十进制非负 | 逐 token 正则 | A2 |
| C-STATS-02 | `rows_written == sample_rows` | 等式 | A2 |
| C-STATS-03 | 12 个 token 全部存在（缺任何一个即失败） | 集合相等 | A2 |
| C-DOC-01 | README 的 CLI 参数表 ⊆ `Opt` 且默认值一致 | 表解析 + 逐项比对 | A11 |
| C-DOC-02 | README 的 CSV 列名与 `sink.rs` 常量一致 | 字符串相等 | A11 |
| C-EXIT-01 | 正常 SIGINT 退出码 0；写盘失败退出码非 0 | exit code | A15 |

### 5.3 L2 进程内管线（16 条，**本方案的收益核心**）

驱动方式：

```rust
let mut h = Harness::new(config);          // 假 AGG/假 ringbuf/假时钟
h.at_ns(1_000_000).sample(ev_a).agg(key_a, AV { count: 1, .. });
h.at_ns(2_000_000).tick();                 // drain + scan + report
h.sigint();
let (samples, aggs, stats) = h.finish();   // 归一化后可做 insta 快照
```

| id | 场景 | 断言 | 覆盖 / 迁移自 |
|---|---|---|---|
| P-01 | 单窗口同键 3 包 | 1 行 `count=3`、first/last=min/max、`window_complete=1`、样本 1 行 | A2 |
| P-02 | 同一 key 两个 CPU 槽位 | 求和 / min / max / 标志取或 | A3 |
| P-03 | 两个源地址同属 /24 | 1 行 `src_prefix=10.99.0.0/24`、`count=6` | A3 |
| P-04 | 相邻两窗口 | 2 行、边界相接、无重复窗口、`Σcount` 守恒 | A6 |
| P-05 | 空窗口 | 水位线推进，不产出空行 | A6 |
| P-06 | 迟到样本 + 迟到 key | `late_dropped` 精确计数、旧窗口不被重开 | A2 |
| P-07 | SIGINT 时窗口未关闭 | partial 行 `window_complete=0` + stats 全 token | A7 |
| P-08 | ringbuf 满（`SampleSource` 返回 Err/None） | `sample_failed` 计数、`sample_dropped=1`、**count 不变** | A13 新增 |
| P-09 | AGG 插入失败 | `agg_overflow` 计数、该包不产样本、不 panic | A13 新增 |
| P-10 | malformed：长度 48/63/65、`family=5`、`family=0` | `malformed` 计数、**不写任何 CSV 行**、不崩 | A13 新增 |
| P-11 | 记录长度随机（proptest，1..=256 B）→ **已改为确定性扫描**：长度 1..=256 × family 0..=255 全组合，不引入 proptest | 只有 64 且 family∈{4,6} 才进管线 | A13 新增 |
| P-12 | 两进程 `--append` 同一文件（线程模拟） | 行数守恒、无错位/覆盖 | A8 |
| P-13 | sink 写失败（只读目录 / ENOSPC 模拟） | 进程返回非 0、错误信息含路径、不吞异常 | A15 新增 |
| P-14 | 时钟：负偏移（启动晚于窗口起点） | `window_start_ms` 可早于启动时间，跨度恒等 | A6 |
| P-15 | 极端时长：`window_ms=1`、`grace_ms=0` | 无除零、无同窗口重复上报 | A6 |
| P-16 | `Σcount == 注入包数` 的不变式（任意脚本序列，proptest） | 守恒 | A4 A5 |

### 5.4 L3 真机 e2e（7 条，全部 Rust）

| id | 场景 | 断言 | 覆盖 |
|---|---|---|---|
| E-01 | attach 生命周期 | `bpftool net` 有 attach → 退出后为空；无 probe 进程残留；`run_cnt` 增长 | A8 A11 |
| E-02 | IPv4 定帧 100 包（AF_PACKET，单窗口 200 ms） | `Σcount=100`、`packets_total=100`、无 late/malformed | A2 A4 |
| E-03 | 帧矩阵端到端（`index.tsv` 全量） | 每帧对应 CSV 行的 family/地址/端口/proto 与 fixture 期望一致 | A12 新增 |
| E-04 | SIGTERM 未关闭窗口 | partial 行 + stats 完整 + flush 成功 | A7 |
| E-05 | `bpftool map delete name CONFIG` 后发包 | `STATS_CONFIG_MISSING>0`、**仍 XDP_PASS、count 正常** | A13 新增 |
| E-06 | `small-maps` 特性构建（AGG=1024）+ 1100 个不同 key | `agg_overflow>0`、已接受 key 的 count 守恒、无丢包 | A13 新增 |
| E-07 | 双端并发 `--append`（veth 两端各一 probe） | 一份 CSV 两种 iface、行数守恒、无交错损坏 | A8 |

### 5.5 L4 长稳 / 性能 / 抖动（5 条，nightly）

| id | 场景 | 判据 |
|---|---|---|
| S-01 | 30 min 100k pps | `Σcount` 误差 ≤0.1%、`agg_overflow=0`、RSS 增长 <5% |
| S-02 | 性能回归 | `pps`/用户态 CPU/`sample_rows/s` 与基线 JSON 比值在 ±5% 内 |
| S-03 | 抖动探测：e2e 连跑 5 次 | 5/5 绿，flake 率 0 才允许上 gate |
| S-04 | SIGKILL（不可 flush） | 文档化的"允许丢 partial"，但 CSV 不损坏、可解析 |
| S-05 | `cargo mutants`（`aggregate.rs`） | mutation score ≥ 70% |

---

## 6. 确定性与抗抖动

| 风险源 | 现有做法 | 新做法 |
|---|---|---|
| 窗口边界相位 | 先跑一次校准测量 `WIN_PHASE`（`run_aggregation.sh:118-160`），再 `sleep 0.05` 轮询 | L2 用虚拟时钟精确置边界；L3 把窗口压到 200 ms，一帧表在 1 ms 内发完，**断言关系式**（`end-start==window_ms`、`first_seen≥start`、`last_seen<end`）而非绝对毫秒 |
| 流量数量不精确 | `ping -f -c 1000`，靠内核不丢 | AF_PACKET 定帧，sender 返回精确 `sent`，`count==sent` 硬断言 |
| 等待方式 | 固定 `sleep` + 20 s 超时 | `wait_until(predicate, timeout)` 轮询 CSV 行数/进程退出；**仓库内禁止固定 sleep**（lint 可加 `grep -n "sleep [0-9]"` 门禁） |
| 用例互相干扰 | 固定 IP/netns，注释要求错开跑 | 每用例自建 `pe2e<pid>_<case>` netns + 独立 /24，`Drop` 清理 |
| 重试掩盖问题 | 无重试（坏了就红） | 仅 L3 允许 `--retries 2`，且重试次数进 JUnit 属性，flake 率作为指标暴露而非隐藏 |

---

## 7. CI 流水线

三档 gate：

| 档 | 触发 | 内容 | 预算 |
|---|---|---|---|
| fast | 每个 PR | `fmt --check`、`clippy -D warnings`、`nextest l0+l1+l2`、`cargo deny`、覆盖率上传 | **< 2 min** |
| bpf | 每个 PR（并行） | `cargo +nightly build -p probe-ebpf --target bpfel-unknown-none -Z build-std=core`（等价 build.rs 的编译门） | < 2 min |
| e2e | push main / 打 tag / 手动 | 特权容器内 L3（`--privileged`，挂 `/sys/kernel/btf`），产物：JUnit + 归一化 CSV + `bpftool net` | < 6 min |
| nightly | 定时 | L4（S-01..S-05）+ e2e×5 抖动探测 | 45 min |

```yaml
name: ci
on: [push, pull_request]
jobs:
  fast:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: Swatinem/rust-cache@v2
      - run: cargo fmt --all --check
      - run: cargo clippy --workspace --exclude probe-ebpf --all-targets -- -D warnings
      - uses: taiki-e/install-action@nextest
      - run: cargo nextest run --workspace --exclude probe-ebpf --profile ci
      - run: cargo llvm-cov --workspace --exclude probe-ebpf --ignore-filename-regex '(^|/)(main|app)\.rs$' --fail-under-lines 90
  bpf:
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - run: rustup toolchain install nightly --component rust-src
      - run: cargo +nightly build -p probe-ebpf --target bpfel-unknown-none -Z build-std=core
  e2e:
    needs: [fast, bpf]
    if: github.ref == 'refs/heads/main'
    runs-on: ubuntu-24.04
    container:
      image: ghcr.io/${{ github.repository }}/aya-dev:ubuntu24.04-llvm23
      options: --privileged
    steps:
      - uses: actions/checkout@v4
      - run: cargo xtask e2e --junit target/e2e.junit.xml
      - uses: actions/upload-artifact@v4
        if: always()
        with: { name: e2e, path: target/e2e-*.{junit.xml,csv,log} }
```

配套要点：
- **镜像预热**：`aya-dev:ubuntu24.04-llvm23` 有 3.49 GB 且首次构建 5–6 min，推到 GHCR 后每次只 pull 增量；`cargo` 缓存用 `Swatinem/rust-cache`（含 `target/` 与 `~/.cargo`）。
- **BTF 依赖**：容器内需 `/sys/kernel/btf/vmlinux`（宿主内核 ≥5.8 且开启 BTF）。GitHub 托管 runner 满足；若未来换自托管，把该路径作为前置检查并 FAIL 得明确。
- **不要**在 CI 上用 `sudo`（G10 的根源），特权由容器 `--privileged` 提供。

---

## 8. 覆盖率与质量门

| 门 | 阈值 | 说明 |
|---|---|---|
| `probe` 行覆盖 | ≥ 90%（排除 `main.rs`/`app.rs`） | `main.rs` 是 8 行壳、`app.rs` 是 aya/tokio 接线，host 上跑不到，按文件名排除；原 `main.rs` 的可测逻辑在 `pipeline.rs`，该文件单独 ≥ 85% |
| `probe-common` 行覆盖 | ≥ 90%；`parse.rs` ≥ 95% | 解析器是安全关键路径 |
| mutation score | `aggregate.rs` ≥ 70%（nightly） | 防止"覆盖率高但断言空" |
| clippy | `-D warnings` | 含 `unwrap_used` 只对非测试代码放开 |
| `cargo deny` | 无 RUSTSEC / 许可证白名单 | 供应链 |
| 文档一致性 | C-DOC-01/02 必须绿 | README 是契约，不是文案 |

覆盖率对 `probe-ebpf` 无意义（无宿主执行），**不设阈值**；它的正确性由 E2/E3 保证。

**S2 实测基线（2026-11-12，`scripts/ci/coverage.sh`）**：`probe-common` 99.3%（152/153）、
`aggregate.rs` 98.3%（470/478）、`sink.rs` 94.3%（383/406）、`pipeline.rs` 88.3%（158/179）、
`ports.rs` 0%（0/61，aya 适配层）、合计 91.1%（1163/1277）。`pipeline.rs` 已过 85% 门，
但 CI 目前只上传报告、不设 `--fail-under-lines`：`ports.rs` 的 0% 会把整体拉到阈值边缘，
等 S5 补齐长稳与真机覆盖后再把门打开（届时需决定 `ports.rs` 是排除还是补 host 侧桩）。

---

## 9. 迁移路线

| Sprint | 内容 | 产出 | 依赖 | 人日 | 退出标准 |
|---|---|---|---|---|---|
| **S1 基建** | `verification/` 纳入 git（或独立仓库）；`cargo xtask` 骨架；nextest + llvm-cov + JUnit；删 `sudo -E` runner；修 G11；CI fast/bpf 两档；现有 4 个 bash 挂进 `xtask e2e --legacy` | 绿 PR gate | 无 | 1.5 | PR 上有 <2 min 的 L0 gate；`cargo test` 不再需要 sudo |
| **S2 管线化** | E1 端口化 + L2 Harness + P-01..P-16 | `probe/tests/pipeline.rs` + 16 条用例 | S1 | 3 | A2/A3/A6/A7 的断言在 2 s 内跑完；`pipeline.rs` 行覆盖 ≥85%（`main.rs` 已拆成 8 行壳，口径见 §8） |
| **S3 解析器** | E2 下沉 + fixture 帧表 + U-PREFIX/U-KEY + C-SCHEMA | `probe-common/src/parse.rs` + `index.tsv` | S2 | 3.5 | 双层 VLAN/非首分片/截断各有单测；eBPF 目标仍可 attach |
| **S4 真机** | E3 定帧发送器 + E5 Rust driver + E-01..E-07；删除 3 个 bash 断言脚本 | `probe/tests/e2e_*.rs` | S3 | 4 | e2e < 6 min 且可 `-j` 并行；无固定 sleep |
| **S5 长稳与报告** | L4 + `xtask report`（JUnit→REPORT.md）+ `small-maps` 特性 + flake 看板 | 自动生成的 `REPORT-*.md` | S4 | 2.5 | A1–A15 全部有 test id 且报告自动生成；抖动 5/5 |

依赖关系是严格的（E2 依赖 E1 的 fixture 复用；E3 依赖 E2 的 `index.tsv`），但 **S1 与 S2 可以并行开工**：S1 改基础设施，S2 只碰 `probe/src`。

**进度（2026-11-12）**：S1、S2 已落地，结果与偏差见 `REPORT-layered-tests.md`；
S3 已落地（分支 `test/test-plan-s3`），`parse.rs` 行覆盖 99.1%、`probe-common` 99.2%，
整体 93.2%，`run_e2e.sh` 在特权容器 PASS，结果与偏差见 `REPORT-parser.md`
（含 `parse_ip` 改用 `Result` 的说明）。S4 起未开工。落地时对计划的两处修订已就地标注：
覆盖率口径（§8、E1 正文）与 §5.1 中 P-11 用确定性扫描替代 proptest 的说明。

**第 0 天就能做的三件小事**（不等重构；S1 已全部完成）：
1. `verification/` 进 git —— 现在就丢在版本控制外，风险最大、成本最低。
2. `check()` 失败时把被测命令的 stdout/stderr 打到日志，并把 `exit "$FAIL"` 改成 `[ "$FAIL" -eq 0 ] || exit 1`。
3. 给 `validate()`（`probe/src/main.rs:298-333`）补 4 个单测——它已经是纯函数，只是没人测。

---

## 10. 完成标准（DoD）

- [ ] 每个 A1–A15 验收项都能用一个 `test id` 精确定位，且 `cargo xtask report` 自动生成证据。
- [ ] `cargo xtask test` 在无 root、无网络的干净机器上 < 30 s 全绿（覆盖 L0–L2）。
- [ ] `cargo xtask e2e` 在特权容器内 < 6 min 全绿，可 `-j 4` 并行。
- [ ] 仓库内 0 处固定 `sleep`、0 处 `exit $FAIL`、0 处 `sudo` in CI。
- [ ] 人为回退任一 bug（例如把 `window_complete` 恒置 1、把 `late_dropped` 恒置 0），必须有测试变红——这是"测试有效"的唯一证明，用 `cargo mutants` 或手工 mutation 抽查。
- [ ] 抖动：连续 20 次 CI e2e 无失败（flake 率 < 1%）。

---

## 11. 范围外

- 内核版本矩阵（只承诺 nightly 的一个 LTS + 容器内固定内核）；多架构（arm64 的 XDP 语义差异另立项目）。
- Verifier 层面的 fuzz（用 `bpftool prog load` 的性质测试成本过高，本期不做）。
- UI/管理面（Train 0 尚未引入）。
- 引入 pytest/pytest-bdd/Bats 等异构测试栈——仓库是 Rust，测试也应是 Rust，否则又多一套要维护的语义。

---

## 12. 风险

| 风险 | 概率 | 影响 | 缓解 |
|---|---|---|---|
| E2 的 `Pkt` trait 被 verifier 拒绝 | 中 | 阻塞 S3 | 1 天 spike 先验证；备选"首 128 B 拷栈"，用 A9 基线守 ≤2% 回退 |
| 特权容器在 CI 不可用 | 低 | e2e 只能本地跑 | 保留自托管 runner 备选；e2e 不阻塞 PR（只阻塞 main） |
| 用例数增长后维护成本 | 中 | 变成新的"粗糙" | 表驱动 + fixture 单源；每条新用例必须能指出它守护的是哪条契约 |
| L2 假内核与真内核语义漂移 | 中 | 假绿 | E-01/E-03 强制走真机；`for_each_slot` 的假实现与 `AggMap` 实现共享同一份合并代码 |
| 重构期功能回归 | 中 | Train 0 交付延期 | 顺序是"先加测试再改结构"：S2 先用现有结构加 L0/L1，再端口化 |
