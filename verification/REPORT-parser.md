# S3 解析器下沉报告

本报告记录 TEST-PLAN **S3 解析器**的落地结果：E2 把纯解析下沉到
`probe-common/src/parse.rs`、共享 fixture 帧表、U-PREFIX/U-KEY 两个 L0 用例组、
C-SCHEMA-01..03 契约层，以及实测覆盖率。数据面语义（接受/拒绝的边界、端口回退 0、
`XDP_PASS`）与 S2 完全一致；eBPF 目标仍可 attach，`cargo xtask e2e --legacy
--script run_e2e.sh` 在特权容器里跑通。

## 1. 结论

| 项目 | 结果 |
|---|---|
| `parse.rs` 行覆盖 | **99.1%**（323/326） |
| `probe-common` 行覆盖 | **99.2%**（523/527：`lib.rs` 200/201 + `parse.rs` 323/326） |
| 整体行覆盖（`probe` + `probe-common`，排除 `main.rs`/`app.rs`） | **93.2%**（1605/1722，S2 基线 91.1%） |
| `cargo xtask bpf` | 通过（`target/bpfel-unknown-none/debug/probe`） |
| `cargo xtask e2e --legacy --script run_e2e.sh` | **PASS**（phases A/B/C，`RESULT: PASS`） |
| `cargo xtask test` | 68 条全绿：L0 46（27 + 19）、L1 3、L2 19 |

## 2. 改了什么

### 2.1 新模块 `probe-common/src/parse.rs`

`no_std`、零分配、只读 `&impl Pkt`：

| 项 | 内容 |
|---|---|
| `pub trait Pkt` | `fn bytes(&self, off: usize, len: usize) -> Option<&[u8]>`；`impl Pkt for [u8]` 给 host 用 |
| `pub struct L3Info` | `src`/`dst`（`[u8;16]`，IPv4 左对齐补零）、`src_port`/`dst_port`、`protocol`、`family` |
| `pub enum ParseError` | `ShortEthernet`、`ShortVlan`、`UnsupportedEtherType`、`ShortIpHeader`、`UnsupportedIpVersion`、`BadIpv4Ihl`、`ShortIpv4Options` |
| `pub fn parse_ip` | Ethernet → 最多两层 802.1Q/802.1ad → IPv4/IPv6 → 端口 |

内核侧只剩 20 行胶水：`struct XdpPkt { data, data_end }` 实现 `Pkt`（`checked_add`
+ `to > data_end` 检查），`try_probe` 里构造一次并调用 `parse_ip`。原来的
`ptr_at`/`EthHdr`/`VlanHdr`/`Ipv4Hdr`/`Ipv6Hdr`/`L4Ports` 和三个解析函数全部删除，
`probe-ebpf/src/main.rs` 从 464 行降到 250 行。

**Verifier 策略**：`take::<N>` 把固定 N 字节拷到栈上数组，之后所有字段访问都是
stack-only，不再有"按 `ihl` 推导的变长切片读"。这是 TEST-PLAN §4 里写的备选方案
（"copy first 128 B"）的更精确版本：只拷当前检查需要的字节数。attach 成功且三类
流量计数与 ping 3/3 一致，说明 verifier 接受该形状。

**语义保持**（逐条与 S2 对照）：

- 最多两层 VLAN tag；第三个 tag 仍按"未知 ethertype"拒绝（旧代码 `for _ in 0..2`）。
- 非首分片（`frag_off & 0x1fff != 0`）端口为 0，首分片仍解析端口。
- L4 截断不是 reject：端口 0，包照常聚合。
- IPv6 不遍历扩展头，`protocol` 仍为 base header 的 Next Header。
- 解析失败只影响本包统计，`try_probe` 恒返回 `XDP_PASS`。

### 2.2 L0 用例

`parse.rs` 内联 11 条（其中 1 条是 fixture 回放）、`lib.rs` 2 条 U-PREFIX、
`probe/src/aggregate.rs` 1 条 U-KEY：

| 用例 | 覆盖点 |
|---|---|
| `u_parse_single_vlan_ipv4_tcp` | 单层 VLAN、IPv4 TCP 端口 |
| `u_parse_double_vlan_ipv6_udp` | QinQ（802.1ad + 802.1Q）、IPv6 UDP |
| `u_parse_three_vlan_tags_are_rejected` | 第三层 tag 拒绝 |
| `u_parse_non_first_fragment_has_no_ports` | 非首分片端口 0（仍接受） |
| `u_parse_first_fragment_parses_ports` | 首分片（MF 置位、offset 0）解析端口 |
| `u_parse_truncated_frames_are_rejected` | 10 B 帧、截断 VLAN、19 B IPv4、39 B IPv6、ihl=6 但只有 20 B |
| `u_parse_truncated_l4_keeps_the_packet` | 只有 IP 头也接受，端口 0 |
| `u_parse_rejects_bad_versions_and_ihl` | 版本 nibble 与 ethertype 不符、`ihl=4` |
| `u_parse_rejects_unknown_ethertype` | ARP/未知 ethertype |
| `u_parse_ipv6_extension_header_reports_base_protocol` | Next Header=0 时不遍历扩展头 |
| `u_parse_fixture_table_matches_the_index` | 回放 `tests/fixtures/frames/index.tsv` 全部 16 帧 |
| `u_prefix_01_mask_is_idempotent_and_keeps_prefix_bits` | 512 地址 × 129 前缀：掩码幂等 + 前缀内比特不变 |
| `u_prefix_02_family_boundaries_stay_aligned` | /0、/1、/32、/128、>128 clamp、IPv4 不溢出到 4..16 |
| `u_key_01_sort_records_is_total_and_deterministic` | 256 条随机记录、4 种旋转：顺序全序且与输入排列无关 |

## 3. fixture 帧表怎么用

位置：`probe/tests/fixtures/frames/`（16 个 `.bin` + `index.tsv`），与 L0 同在
`probe/tests/` 下，但**不是** L1 测试文件：xtask 只把 `*.rs` 当契约层，目录不影响分层。

`index.tsv` 列（TAB 分隔，`#` 开头为表头/注释）：

```
# file	family	src	dst	sport	dport	proto	accept
ipv4_tcp.bin	4	10.0.0.1	10.0.0.2	12345	80	6	1
...
truncated_eth.bin	-	-	-	0	0	0	0
```

- `accept=1`：`parse_ip` 必须 `Ok`，且 family/src/dst/sport/dport/proto 与表一致。
- `accept=0`：只要求 `Err`（具体变体由上面的定点单测覆盖，表里不重复记录）。
- 拒绝行的 `family/src/dst` 用 `-` 占位，避免编造"期望的半个解析结果"。

消费方式：

- **L0**：`u_parse_fixture_table_matches_the_index` 在 `parse.rs` 的 `mod tests` 里
  用 `std::fs` 读表并逐帧回放。它放在 `--lib` 内，因为 `cargo xtask test --layer l0`
  只跑 `-p probe -p probe-common --lib`。
- **L3（将来）**：E3 定帧发送器按同一份 `index.tsv` 发包，再用 `accept` 列断言
  聚合/样本 CSV 的期望，不需要另建一份"真机期望表"。
- `.gitattributes` 把 `*.bin` 标为 `binary`（防止 Windows `core.autocrlf` 改写原始
  字节），`index.tsv` 固定 `eol=lf`。

新增一帧：把原始 Ethernet 字节写成 `.bin`（无 pcap 头），在 `index.tsv` 追加一行，
两个断言（L0/L3）自动覆盖；不需要改代码。当前 16 帧 = 9 接受 + 7 拒绝，覆盖
单/双层 VLAN、IPv4/IPv6、TCP/UDP、分片、截断、未知 ethertype、坏 `ihl`、IPv6 扩展头。

## 4. 覆盖率实测

命令：`bash scripts/ci/coverage.sh`（`cargo llvm-cov -p probe -p probe-common
--ignore-filename-regex '(^|/)(main|app)\.rs$'`）。原始数据 `target/lcov.info`：

| 文件 | 行覆盖 | 备注 |
|---|---|---|
| `probe-common/src/parse.rs` | 323/326 = **99.1%** | 门 ≥95% |
| `probe-common/src/lib.rs` | 200/201 = 99.5% | |
| `probe-common` 合计 | 523/527 = **99.2%** | 门 ≥90%，S2 为 99.3% |
| `probe/src/aggregate.rs` | 541/549 = 98.5% | S2 为 98.3% |
| `probe/src/sink.rs` | 383/406 = 94.3% | 与 S2 相同（L1 走的是同一批 sink 路径） |
| `probe/src/pipeline.rs` | 158/179 = 88.3% | S2 门 ≥85% |
| `probe/src/ports.rs` | 0/61 = 0.0% | aya 适配层，仍需真机 |
| **合计** | 1605/1722 = **93.2%** | S2 基线 91.1% |

`parse.rs` 剩 3 行未覆盖：`take` 里 `src.len() != N` 的防御性判断（`Pkt` 合约保证
长度，无法从合法实现触发）以及 `[u8]::bytes` 的 `checked_add` 溢出路径。

## 5. 与计划的偏差

1. **`parse_ip` 返回 `Result<L3Info, ParseError>`，不是计划初稿写的 `Option<L3Info>`（TEST-PLAN §4 已按实测更新）。**
   `Err` 分支在数据面同样只是 `XDP_PASS`，成本相同；但 `ParseError` 让"截断/坏 ihl/
   未知 ethertype"在测试里可以精确断言，避免一个 `None` 把三类 bug 混在一起。
2. **未引入 proptest（也未引入任何新依赖）。** U-PREFIX-01 用固定 LCG 扫描
   512 地址 × 129 前缀，U-KEY-01 用 256 条 LCG 记录 × 4 种旋转。理由是容器 registry
   缓存没有这些 crate，且 S2 已用同样的确定性扫描替代 proptest（TEST-PLAN §5.1 已
   标注）；随机种子写死，失败可复现。
3. **C-SCHEMA 用手写严格解析器，不用 `csv`/`assert_cmd`/`insta`。** 仓库的 CSV 方言
   本来就窄（逗号、LF、无引号、无空字段），测试直接按这个方言断言，遇到引号/CR/
   空字段/列数不符立刻失败——比通用 CSV 解析器更严格。C-SCHEMA-01/02 调用
   `CsvSink`/`AggCsvSink`（真实写盘）而不是启动 `probe` 进程：`--append` 的重复表头
   与丢行语义都在 sink 层，启动进程还需要 eBPF/root，违反"无 root 可跑"。
4. **U-KEY-01 放在 `probe/src/aggregate.rs`，不在 `probe-common`。** `sort_records`
   与 `AggRecord` 是用户态聚合记录，不在 ABI 里；放进 `probe-common` 等于把 CSV 排序
   语义塞进共享 ABI crate。U-PREFIX-01/02 则留在 `probe-common/src/lib.rs`，因为
   `mask_src_prefix` 是 ABI 侧函数。
5. **C-SCHEMA-03 的并发单位是线程，不是进程。** 8 个线程同时 `create(append=true)`
   + 各写 50 行，覆盖 `flock` 表头竞态与 `O_APPEND` 整行不撕裂。跨进程 `--append`
   已由 L2 的 `p12` 和 `run_e2e.sh`（两个 probe 进程共写 CSV）覆盖。

## 6. 没做的部分

- **L3 发包重放尚未实现**：fixture 帧表已按 E3 的形状准备好（含期望列），但
  `probe/tests/e2e_*.rs` 属于 S4，本次没有新增发送器。
- **C-SCHEMA 未覆盖 CLI 参数层**（`--append` 的 clap 解析、退出码、`--help` 文本）。
  L1 目前只钉住 sink 写出的字节契约；CLI 进程级契约仍缺一个不需要 BPF 的入口
  （`probe` 二进制目前没有 dry-run 模式，S4 的 driver 可以补）。
- **e2e 只跑了一次**（PASS），没有做 flake 统计；L4 长稳属于 S5。
- 覆盖率工具：`aya-dev` 镜像里原本没有 `cargo-llvm-cov`，本次在容器内
  `cargo install cargo-llvm-cov --locked`（v0.9.1）后测得；镜像/Dockerfile 未改。
