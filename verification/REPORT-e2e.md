# S4 真机 e2e 落地报告（E-01…E-07）

分支：`test/test-plan-s4`（从 `main` 的 `35749f7` 新开）
对应计划：`verification/TEST-PLAN.md` §5.4（L3）、§9 的 S4 行
结论：**7 条用例全 Rust 化并全绿**，`cargo xtask e2e` 默认 `-j 4` 并行，
实测 10-20 s（预算 6 min）；三个 bash 断言脚本已删除，只剩 `run_perf.sh` 走 `--legacy`。
过程中抓到并修掉两个产品 bug（L4 端口字节序、启动期 SIGTERM 竞态）。

---

## 1. 环境

| 项 | 值 |
|---|---|
| 容器 | `aya-dev`（Ubuntu 24.04 + LLVM 23，特权 + host 网络，`/workspace` 挂外层目录） |
| 内核 | `6.6.87.2-microsoft-standard-WSL2`（Docker Desktop），20 CPU |
| 工具 | `cargo 1.99.1`、`bpftool v7.4.0`、`iproute2 6.1.0`、nightly + `bpf-linker` |
| sysctl | `kernel.bpf_stats_enabled=1`（E-01 自己打开，`run_cnt` 才可见） |

所有命令都在容器内、仓库根执行：

```bash
docker compose exec aya-dev bash -lc 'cd /workspace/probe && cargo xtask e2e'
```

## 2. 新增结构

| 路径 | 角色 |
|---|---|
| `testkit/`（新 crate `probe-testkit`，零外部依赖） | 真机测试工具箱：AF_PACKET 发送器、fixture 帧表、veth 对、子进程 probe 包装、CSV 读取、`bpftool` 查询 |
| `probe/tests/e2e.rs` | 7 条用例本体；全部 `#[ignore]`，只由 `cargo xtask e2e` 拉起 |
| `probe/tests/e2e_support/mod.rs` | `Env`（每个用例一份 veth + 产物目录）、`wait_for`（带超时的轮询）、`wait_window_start`（按 `CLOCK_MONOTONIC` 对齐窗口边界） |
| `xtask send --iface IF [--frames D] [--count N] [--only accept\|reject\|all]` | E3 定帧发送器：arm（`socket`+`bind`）→ 发送 → `close`，返回精确 `sent` 数 |
| `xtask e2e [--case ID] [--jobs N]` / `e2e --legacy` | Rust driver（默认 4 并发）/ 遗留 shell |

`testkit` 用 `unsafe extern "C"` 直接调 `socket`/`bind`/`sendto`/`if_nametoindex`/`close`/
`kill`/`clock_gettime`/`sched_getcpu`/`sched_setaffinity`，因此 `probe-testkit` 与 `xtask`
都保持“零 registry 依赖”（`xtask` 的这条约束是刻意的：它必须在 probe 依赖可拉取之前就能跑）。

## 3. 用例与实测

| id | 场景 | 关键断言 | 实测 |
|---|---|---|---|
| E-01 | attach 生命周期 | attach 前 `bpftool net` 无该项 → 后有一项；`run_cnt` 增长 10；SIGTERM 退出码 0 且 `packets_total=10`；退出后无残留 | 0.43 s |
| E-02 | 100 帧单窗口 | 恰好 1 行聚合 `count=100 window_complete=1`；`Σcount=100`、`packets_total=100`、`late_dropped=0`、`malformed=0`；`src_prefix=10.0.0.0/24` | 通过 |
| E-03 | 16 条帧矩阵 | 9 条 accept 的聚合键集合与 `index.tsv` 完全一致（`dst_addr/dst_port/protocol` 多重集）、样本 5 列一致；7 条 reject 无行；`packets_total=9` | 通过 |
| E-04 | SIGTERM 打断未关闭窗口 | 1 行 `count=5`、`window_complete=0`、窗口跨度=配置的 1 h；`agg_records=1`、`late_dropped=0` | 通过 |
| E-05 | `CONFIG` 不可识别 | `STATS[CONFIG_MISSING]≥10`（attach 期间读 map）、仍 `XDP_PASS`、`Σcount=10` | 通过 |
| E-06 | `small-maps`（AGG=1024）+ 1100 键 | `packets_total=1100`、`agg_overflow>0`、`Σcount+overflow=1100`、每键 `count=1` | 通过 |
| E-07 | 双端并发同写一份 CSV | 一条 header、2 行聚合（两 iface，`count` 3/4）、2 行样本、两个 probe 各自 `packets_total` 3/4、逐行列数完整 | 1.00 s |

```
xtask: e2e (default build: case_e01, case_e02, case_e03, case_e04, case_e05, case_e07)
test result: ok. 6 passed; 0 failed; 0 ignored; … finished in 1.69s
xtask: e2e (small-maps build: case_e06)
test result: ok. 1 passed; 0 failed; 0 ignored; … finished in 0.32s
```

连跑 4 次（含冷构建一次）：19 s、10 s、10 s、10 s，全绿。单条用例都是亚秒级到 1 s。

## 4. S4 抓到的产品 bug

### 4.1 L4 端口字节序（E-03 抓到）

- 现象：样本 CSV 的端口是字节颠倒的（`12345` 打印成 `14640`，`80` 打印成 `20480`），
  而聚合 CSV 的端口看起来正常。
- 根因：ABI 文档（`probe-common/src/lib.rs:80-82`、`README.md:139`、`README.md:200-201`）
  规定端口是**网络序**，但 `probe-common/src/parse.rs` 的 `parse_ports` 用
  `u16::from_be_bytes` 转成了主机序；遵守 ABI 的 `probe/src/sink.rs:92-93`
  `u16::from_be(raw.src_port)` 于是又转了一次。聚合写出当时直接打印 `key.dst_port` 原始值，
  恰好因为同样的 producer 侧转换才“看起来对”——两个 bug 互相抵消了一半。
- 判定依据：`sink.rs` 的 `from_be` 来自最早的提交（`abd724f`），`parse_ports` 的转换是 S1–S3
  解析器下沉时引入的（`35749f7`）；README、ABI 注释和 L2 harness（`probe/tests/pipeline.rs:80`
  用 `dst_port.to_be()` 构造事件）三处都指向网络序。
- 修复（改生产者、文档不动）：`parse_ports` 改为 `u16::from_ne_bytes([ports[0], ports[1]])`
  / `[ports[2], ports[3]]`（保留网线字节，附注释说明 userspace 用 `from_be` 还原）；
  `sink.rs` 聚合写出补上 `u16::from_be(key.dst_port)`；`probe/src/parse.rs` 的单测与 fixture
  矩阵测试改成 `u16::from_be(info.src_port)`，`sink.rs` 的聚合单测用 `8080u16.to_be()` 构造键。
- 回归：L0 19 条、L2 19 条全绿；E-02/E-03/E-06 的 CSV 端口全部正确（1…1100）。

### 4.2 启动期 SIGTERM 竞态（E-04 并行时暴露）

- 现象：`-j 4` 下偶发 `probe printed no stats line`，日志只到 `attached XDP program to …`。
- 根因：`probe/src/app.rs` 的 `signal::ctrl_c()` / `unix_signal(SIGTERM)` 注册在 attach、
  sink 创建之后；此时到达的 SIGTERM 走默认处置，进程直接死掉，既不 drain 也不 flush。
  生产上等价于“`systemctl start` 后立刻 `systemctl stop` 会丢 partial 数据”。
- 修复：两组 handler 提前到 `run()` 开头（`raise_memlock()` 之后、`Ebpf::load` 之前）。
  tokio 在创建 `signal()` 时就注册处置，之后到达的信号由 `sigterm.recv()` 在
  `select!` 里补齐处理。
- 回归：E-04 单跑与 `-j 4` 并行连跑 4 次均绿。

### 4.3 测试基础设施：map 必须按 id 而不是按名字查

E-05 最初用 `bpftool map update name CONFIG` / `map dump name STATS`，`-j 4` 下会命中
**别的用例**的同名 map，把并发用例的 `CONFIG` 改坏。改为先 `bpftool net` 拿本用例的
program id，再 `bpftool prog show id N` 解析 `map_ids`，逐个 `bpftool map show id M` 反查
名字，最后全部按 id 操作（`testkit::net::prog_map_ids` / `stat_counter(map_id, …)` /
`update_config(map_id, …)`）。

## 5. 并行与“无固定 sleep”

- 隔离：每个用例自建 veth 对（名字带 pid + 序号，≤15 字符）与
  `target/e2e/<case>/` 产物目录，`Drop` 删除；不建 netns（XDP 只看 L2 ingress，
  veth 两端同在根 netns 也能收帧，且省掉 netns 清理竞态）。
- 同步：全部靠“等状态”而不是“等时间”——`wait_for` 5 ms 轮询 + 30 s 超时（超时即断言失败），
  `probe.wait_attached()` 等 `attached XDP program to '<iface>'` 日志，
  E-02 用 `CLOCK_MONOTONIC`（与 `bpf_ktime_get_ns()` 同源）对齐 200 ms 窗口边界，
  E-04 直接 `SIGTERM`（`sendto` 返回时对端 XDP 已跑完，不需要等待）。
- E-06 需要“所有插入落在同一个 per-CPU map”：`testkit::net::pin_current_cpu()`
  把测试线程钉到当前 CPU。实测（`taskset -c 9` 发 1100 帧）确认 veth XDP 在发送方
  CPU 上下文执行：`STATS` 只有 `CPU 09` 有值（`4c 04`=1100），其余全 0。
- 残留的 `thread::sleep` 只有两处轮询间隔（5 ms / 10 ms），语义是“下一次检查”，
  不是“等它准备好”。

## 6. 删除的脚本

```text
verification/run_e2e.sh           （IPv4 回归，58 条 CHECK）  → E-01/E-02/E-03/E-07
verification/run_e2e_ipv6.sh      （IPv6 端到端，89 条 CHECK） → E-03 的 ipv6_* / qinq_* 帧 + E-07
verification/run_aggregation.sh   （聚合验收 A2–A7）           → E-02/E-04/E-05/E-06
```

保留 `verification/run_perf.sh`（性能对比不属于 e2e 契约）与它用的
`verification/traffic_gen.rs`；`cargo xtask e2e --legacy` 现在只指向它。
历史报告（`REPORT-xdp-*.md`、`REPORT-aggregation.md`、`REPORT-parser.md`）保持原样，
它们记录的是当时的脚本，不做逐字回改。

## 7. 与计划的偏差

| 计划 | 实际 | 原因 |
|---|---|---|
| E5：每个用例建 netns + `10.<128+shard>.<case>.0/24` | 不建 netns，veth 对留根 netns、不配 IP | XDP 在 L2 ingress 就能收到帧；不配 IP 反而没有内核自发流量（ARP/ND/ICMP），`packets_total` 才能精确等于发送数，也才能 `-j` 并行 |
| E-05：`bpftool map delete name CONFIG` | 写一个 `version != CONFIG_VERSION` 的 `CONFIG` | `Array` map 没有“删元素”，`bpf_map_delete_elem` 返回 EINVAL，probe 也不 pin map；版本不匹配走的是**同一条** `load_config` fallback 分支（`probe-ebpf/src/main.rs:74-105`），`STATS_CONFIG_MISSING` 同样累加 |
| E-06：单次构建 + 1100 键 | `--features small-maps` 二次构建 pass | 默认 131072 条目在合理帧数内不可能溢出；`small-maps` 把 `AGG_MAX_ENTRIES` 降到 1024，`xtask e2e` 自动分两趟跑 |
| E-03：16 条帧全部注入 | `truncated_eth.bin`（10 B）只断言发送失败 | `AF_PACKET`/`SOCK_RAW` 的 `sendto` 拒收短于以太头的帧（实测 10 B/13 B → `EINVAL`，14 B 起 OK）；该解析分支由 L0 的解析器单测覆盖 |
| S-03：e2e 连跑 5 次 | 连跑 4 次全绿 | 属 S5 的抖动看板（20 次）范围，S4 只做到“并行连跑稳定” |

## 8. 验收对照（TEST-PLAN §10）

- [x] `cargo xtask e2e` < 6 min 全绿、可 `-j 4`：实测 10-20 s。
- [x] 0 处 `exit "$FAIL"`：三个带该模式并会导致 `FAIL=256 → exit 0` 假绿的脚本已删除。
- [x] e2e 无固定 `sleep`：见 §5；`run_perf.sh` 作为遗留性能脚本仍有 `sleep`，不在 e2e 契约内。
- [ ] 20 次 CI 抖动 < 1%：留待 S5。

## 9. 复现

```bash
# 全部 7 条（默认 4 并发，自动分 small-maps 两趟）
cargo xtask e2e

# 单条 / 指定并发
cargo xtask e2e --case e04 --jobs 1

# 只发帧、不跑断言（手工排障用）
cargo xtask send --iface veth0 --count 3 --only accept

# 产物
ls target/e2e/e02/
```
