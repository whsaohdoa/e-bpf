# 分层测试落地报告（S1 + S2）

本报告记录 TEST-PLAN 中 **S1 基建**与 **S2 管线化**两个 sprint 的落地结果：做了什么、
用什么证据证明、以及哪些退出标准只是"等价达成"而非逐字达成。功能语义（窗口关闭、迟到
样本、去重、CSV 列序）没有改变，`verification/` 里原有的真机脚本仍然原样可跑。

## 1. 分层与规模

| 层 | 位置 | 条数 | 本机耗时 | 需要特权/内核 |
|---|---|---|---|---|
| L0 单元 | `probe/src/*.rs` 的 `mod tests`、`probe-common/src/lib.rs` | 32 | 0.06 s | 否 |
| L1 契约 | `probe/tests/*.rs`（除 `pipeline`） | 0（S3 落地） | — | 否 |
| L2 管线 | `probe/tests/pipeline.rs` | 19 | 0.05 s | 否 |
| L3 真机 | `verification/*.sh`（`cargo xtask e2e --legacy`） | 4 个脚本 | 分钟级 | 是 |

`cargo xtask test` 一次跑完 L0→L1→L2，共 51 条；有 nextest 时走
`cargo nextest run --profile ci`，每层各写一个 `target/nextest/ci/junit-<layer>.xml`，
`cargo xtask report` 汇总它们。

L2 的 19 条 = TEST-PLAN 的 P-01…P-16 加上 3 条守卫（窗口时间戳渲染、IPv6 family、
BLOCK 键的 16 列 schema）。全部不 `sleep`、不加载 eBPF：假 AGG map 用 `Rc<RefCell<_>>`
让测试在 `Session` 存活期间还能注入"迟到 key"，脚本化 ringbuf 可以喂任意字节或报
`ReservationFailed`，sink 可以返回错误，时钟是注入的 `MonoClock` 替身。

## 2. E1 端口化

`probe` 从单 bin 变成 lib + 薄 bin：

| 文件 | 行数 | 职责 |
|---|---|---|
| `probe/src/lib.rs` | 23 | 导出 `aggregate`/`app`/`pipeline`/`ports`/`sink` |
| `probe/src/ports.rs` | 183 | `AggStore`/`SampleSource`/`TimeSource` 三个端口 + aya 适配实现 |
| `probe/src/pipeline.rs` | 410 | `Session`：drain → 关闭窗口 → 写 CSV → 统计；`Summary::stats_line` |
| `probe/src/app.rs` | 528 | CLI、`validate`、加载/attach、tokio 主循环（不可在进程内测的部分） |
| `probe/src/main.rs` | 8 | `fn main() -> ExitCode { probe::app::main() }` |

两处与 TEST-PLAN 原文的偏差，都是为了在 host 上可测：

1. `SampleSource::next_record` 返回 `Poll<'a>` 而不是 `Option<&[u8]>`。`aya::RingBuf` 的
   `next()` 返回借用 `&mut self` 的 `RingBufItem`，无法把切片借出；真实实现把 64 B 拷进
   自己的 `Vec` 再借出，多出来的 `Poll::ReservationFailed` 正好用来表达 P-08 的"预留
   失败"。
2. `TimeSource::mono_ns` 保留 `Result`：`clock_gettime` 可以失败，端口不该假设它不失败。

`Session` 的 `shutdown` 保持原来的六步顺序（drain 到空 → `scan_agg(None)` → 取 pending →
写聚合 → 取全部样本 → 写样本 → flush），因此 SIGINT 路径产出的 `window_complete=0` 行
与旧实现逐字一致；`stats:` 行的 11 个 token 顺序也逐字未动（e2e 脚本按 token 名 grep）。

## 3. L2 抓出的两处真 bug

写 L2 之前，这两个问题在真机上只能靠"偶尔发现"：

1. **并发 append 写两个表头**（P-12）。两个进程同时打开同一个空文件时都看到 `len == 0`，
   于是各写一行表头，行数多出 1。修复：`write_header_if_empty` 先在输出文件上取
   `flock(LOCK_EX)`（`FileLock`，Drop 解锁）再判断长度并写表头。
2. **写失败信息不含文件名**（P-13）。原来只有 `write sample CSV row`，多文件并发时无法
   定位。修复：`CsvSink`/`AggCsvSink` 记住自己的 `PathBuf`，`write`/`flush` 的 context
   变成 `write sample CSV row to <path>` 等。

顺带纠正了一个测试假设：`Stats::agg_records` 是"累计已写出的聚合行数"，不是"pending
行数"，迟到 key 被丢弃时它不会回退（P-06 的断言据此改为 1）。

## 4. 基础设施

- **xtask**（`probe/xtask`，仅 std，无第三方依赖）：`test --layer l0|l1|l2|all`、
  `bpf`、`e2e --legacy [--script NAME]`、`report`、`soak --minutes N`（S5 占位）。
  别名写在 `.cargo/config.toml`；那个文件里原来全局的 `runner = "sudo -E"` 已删除，
  `cargo test` 不再需要密码。
- **G11**：`run_aggregation.sh`、`run_e2e.sh`、`run_e2e_ipv6.sh` 结尾的 `exit "$FAIL"`
  改成 `[ "$FAIL" -eq 0 ] || exit 1`。进程退出码只有 8 位，`FAIL=256` 会被截成 0，
  一次失败的验收会显示为绿。
- **G2**：`verification/` 纳入版本控制（4 个脚本、TEST-PLAN、3 份 REPORT、
  `traffic_gen.rs`）；多兆字节的 CSV、日志和 drawio 图不入库，用
  `verification/.gitignore` 挡住。
- **CI**：`fast` job 跑 fmt、`clippy -D warnings`、L0–L2、doc tests、覆盖率报告，
  `bpf` job 用 nightly 构建 `bpfel-unknown-none` 对象；两个 job 都在 `pull_request`
  上触发。

## 5. 覆盖率

`scripts/ci/coverage.sh` 现在按文件名排除 `main.rs`（8 行壳）与 `app.rs`（aya/tokio
接线，host 上跑不到），其余部分统计行覆盖率：

| 文件 | 行覆盖率 |
|---|---|
| `probe-common/src/lib.rs` | 99.3%（152/153） |
| `probe/src/aggregate.rs` | 98.3%（470/478） |
| `probe/src/sink.rs` | 94.3%（383/406） |
| `probe/src/pipeline.rs` | 88.3%（158/179） |
| `probe/src/ports.rs` | 0%（0/61）——aya 适配层，host 测试不经过 |
| 合计（排除 `main.rs`/`app.rs`） | 91%（1163/1277） |

TEST-PLAN 给 S2 的退出标准是"`main.rs` 覆盖率 ≥60%"。端口化之后 `main.rs` 只剩一行
调用，真正的可测逻辑搬进了 `pipeline.rs`（88.3%）；如果按"`main.rs` 这个文件"字面
统计，它仍然接近 0。这是口径调整，不是达标，S3 需要决定是给 `app.rs` 补进程内
可测的切分，还是接受"可测逻辑在 `pipeline.rs`"的统计口径。CI 里覆盖率目前**只报告不设
门**，注释写明了等 S3/S5 再开 `--fail-under-lines`。

## 6. 与 TEST-PLAN 退出标准的对照

| 标准 | 状态 |
|---|---|
| S1：PR 上有 <2 min 的 L0 gate | 达成。L0 32 条 0.06 s；fast job 的主要耗时是编译与 clippy |
| S1：`cargo test` 不再需要 sudo | 达成。`runner = "sudo -E"` 已删，见 `.cargo/config.toml` 注释 |
| S1：4 个 bash 挂进 `xtask e2e --legacy` | 达成（真机容器里执行；本分支未在 CI 上跑 L3） |
| S2：A2/A3/A6/A7 的断言 2 s 内跑完 | 达成。对应语义由 L2 的 P-01…P-05、P-16 覆盖，整层 0.05 s |
| S2：`main.rs` 覆盖率 ≥60% | **未按字面达成**，见 §5 的口径说明 |
| DoD：0 处固定 `sleep`（新增测试） | 达成。L2 全部用注入时钟推进水位 |
| DoD：0 处 `exit $FAIL` | 达成（3 处已改，见 §4） |
| DoD：CI 0 处 sudo | 达成 |

## 7. 没做的部分（S3+）

- **L1 契约层**：TEST-PLAN 的 C-* 用例需要 `assert_cmd` + `insta`，本分支刻意不引入
  新 dev-dependency，`xtask test --layer l1` 现在是空跑并打印提示。
- **L3 driver**：`xtask e2e --case <id>` 只报"随 e2e harness 一起到来"，真机仍走
  `--legacy` 的四个脚本；CI 也没有 e2e job（需要特权 runner/容器）。
- **L4 soak**：`xtask soak` 是稳定接口的占位，S5 落地。
- **覆盖率门**：见 §5，report-only。
- **P-11 的实现方式**：TEST-PLAN 写的是 proptest，这里用确定性扫描（长度 1..=256 ×
  family 0..=255）覆盖同一空间，避免为一个用例引入 proptest 及其传递依赖；P-12 用
  两个真线程 + 真文件，其余用例用内存 sink。

## 8. 提交

```
docs: describe the layered tests and drop the stale sudo runner note
test(xtask): write one JUnit report per layer
test(probe): add the L2 pipeline layer, xtask and a fast CI gate
fix(sink): name the CSV file in errors and serialise the header write
refactor(probe): split the binary into a library with explicit ports
```

顺序是刻意的：先加 lib target（否则 xtask 无法 `--test pipeline`），再把 L2 抓出的
sink 修复单独成一个 commit，最后才是测试与 CI 基建。
