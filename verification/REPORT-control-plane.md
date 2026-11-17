# 验收报告：控制面通信（Train 0，E8 的前半）

对应计划 `Train0-Plan-6x1.md` 的 E8 前半与 S1-7 验收（`:466`），契约 `docs/Control-Agent-API-v1.md`。
本分支只交付 **Rust 侧能力 + 进程内假控制面**，Spring 侧真控制面在下一分支（契约 §13.2）。

## 1. 结论

- G1 在 mock 上跑通：真 `probe-agent` 连 TLS 假控制面，10 次「发布新配置 → agent 应用 → ack」全部 `APPLIED`，
  退出码 0（`verification/control/g1.csv` 共 10 行）。
- 走 `Watch` 推送的 5 次：`publish → ack` P95 = **22 ms**；纯轮询的 5 次：P95 = **977 ms**；
  空闲态等心跳的那一次是 **4955 ms**。三个数都只代表 mock 环境（L4/L6）。
- S1-7 三条证据都在真进程上复现：重启后 `GetCursor` 续传 + `applied.json` 恢复；落后态在心跳内拉取新版；
  ack 里带 `applied_at`。
- 数据面仍是 **fail-open**：本分支没有引入任何 XDP drop 路径，`rules` 收下但不执行，ack 显式带
  `degraded_features:["rules"]`。
- 顺带修了一个拦截性 bug：`Watch` 推送到来的新版本会因为旧 `desired_sha256` 走同内容快捷路径，
  **不下载就 ack**（详见 §5 末段）。
- 计划 `:30` 的「发布 API 200 → ack」口径无法在此分支测量（Spring 侧没有发布 API，L6）；
  本报告用的是 mock `/publish` → ack，属已知偏离。

## 2. 方法

**拓扑**（两个真进程，脚本一次性拉起；没有测试替身充当 agent）：

```
probe-agent (root, XDP on lo)  ──gRPC + mTLS──▶  control-mock (127.0.0.1:N)
        │  state dir: applied.json                    │  HTTP admin 127.0.0.1:N+1
        ▼                                             ▼
   窗口聚合 → ReportWindows                      /publish /acks /state
```

- agent：`target/debug/probe-agent`（`probe-agent` bin，S-C 的门面），`--iface lo`，`--window-ms 1000
  --grace-ms 200 --flush-ms 200`，state dir 在 `mktemp -d` 下。
- 控制面：`target/debug/control-mock`（`--features mock`，**dev-only**，不是产品代码）。
- 证书：全部由 `control-mock` 用 `rcgen` 在启动时自签并写到 `--certs <dir>`，脚本不手搓 openssl。
  文件名是 `ca.pem` / `client-cert.pem` / `client-key.pem` / `server-cert.pem` / `server-key.pem`；
  agent 用前三个，`require_client_cert` 保持默认 `true`。
- 脚本：`bash verification/run_control_demo.sh`（root，容器内），日志与 CSV 写到工作区外层的
  `verification/control/`（不进 git）。脚本自带四个动作：构建、起 mock、起 agent、10 次发布并等待 ack。
- 每轮发布的字节都不同：契约 §6.4 的文档 B（63 B，含 `r-8080` 规则）逐轮改 `grace_ms` 的两个 varint 字节。
  文档里没有 version 字段，若字节相同，agent 会走 §6.3 的同内容快捷路径直接 ack（不下载、不 apply），
  那样测到的就不是「应用」。（第一次冒烟就是这么翻车的，见 §5 的 bug 记录。）
- 机器：`aya-dev` 容器（root，`CapEff=000001ffffffffff`），20 vCPU / 8 GiB，内核
  `6.6.87.2-microsoft-standard-WSL2`，`cargo 1.99.1`，`libprotoc 3.21.12`。XDP 挂在 `lo` 上
  （WSL2 下 `lo` 走 generic 模式，与数据面 e2e 的环境说明一致）。

**阶段划分**：

| 阶段 | mock 配置 | 计量版本 | 预期路径 |
| --- | --- | --- | --- |
| push | `{"watch_supported":true}` | v2–v6 | Watch 推送 → GetConfig → apply → Ack |
| poll | `{"watch_supported":false}` | v8–v12 | 心跳带 `desired_version` → GetConfig → apply → Ack |
| restart | 同上（同一个 mock） | — | 杀 agent → 重启 → GetCursor → 续传 |

v1 与 v7 是 warm-up：v1 是 mock 启动时自发布的向量（agent 首个 apply），v7 是重启后的第一次拉取
（包含重连，且是空闲态、要等 5 s 心跳）。两者都不进 CSV，v7 的延迟写进 `phases.txt`。

## 3. G1 数据

`verification/control/g1.csv`（原样，迭代号跳过的两个是 warm-up）：

| iter | transport | watch_used | publish_ms | ack_ms | publish_to_ack_ms | agent_apply_ms | degraded |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 2 | push | 1 | 1794921833615 | 1794921833620 | 5 | 4 | rules |
| 3 | push | 1 | 1794921833646 | 1794921833668 | 22 | 21 | rules |
| 4 | push | 1 | 1794921833732 | 1794921833738 | 6 | 5 | rules |
| 5 | push | 1 | 1794921833763 | 1794921833784 | 21 | 20 | rules |
| 6 | push | 1 | 1794921833849 | 1794921833854 | 5 | 4 | rules |
| 8 | poll | 0 | 1794921839659 | 1794921840629 | 970 | 969 | rules |
| 9 | poll | 0 | 1794921840698 | 1794921841632 | 934 | 934 | rules |
| 10 | poll | 0 | 1794921841666 | 1794921842635 | 969 | 968 | rules |
| 11 | poll | 0 | 1794921842691 | 1794921843636 | 945 | 946 | rules |
| 12 | poll | 0 | 1794921843661 | 1794921844638 | 977 | 979 | rules |

`publish_ms` 取自 mock `/publish` 响应里的 `published_at_ms`，`ack_ms` 取自 agent 的 ack 落在 mock 侧的时间
（`AckRecord.received_at_ms`），`agent_apply_ms` 是 agent 自己 ack 里的 `applied_at` 减去 `publish_ms`。

**两个口径**（n = 5，P95 取最近秩，即第 5 个值）：

| 口径 | 条件 | P95 | 逐次 | 与计划的关系 |
| --- | --- | --- | --- | --- |
| `watch_used=1` 推送 | mock 实现 `Watch` 并主动推 | **22 ms** | 5, 5, 6, 21, 22 | 对应契约 §9 的「推送路径 P95 ≤500 ms」，**只在 mock 上成立** |
| `watch_used=0` 纯轮询 | mock 不支持 `Watch`，agent 回退心跳 | **977 ms** | 934, 945, 969, 970, 977 | 对应落后态「P95 ≤3 s」，满足；不代表空闲态 |
| 空闲态首次拉取（n=1） | agent 追平后闲置 | 4955 ms | 4955 | 对应 `Train0-Plan-6x1.md:248` 的 5 s 心跳，L4 |

`watch_used` 不是猜的：mock 记下每次推送的 `(version, ts)`，ack 到达时若存在 `ts <= received_at` 的同版本推送
就记 `watch_used=true`（`agent/src/mock/mod.rs` 的 `ack_records()`）。

**必须写明的三件事**：

1. 真控制面的 `Watch` 返回 `UNIMPLEMENTED`（契约 §3、L4），agent 会回退轮询——推送那 5 行只在 mock 成立。
2. 本分支没有发布 API，mock 的 `/publish` 只是 dev-only 的 HTTP admin；计划 `:30` 的 G1 是
   「发布 API 200 → ack」，两者不是同一条链路（L6）。
3. `agent_apply_ms` 与 `publish_to_ack_ms` 相差 1 ms 量级，说明这段延迟几乎全在 agent 侧
   （解析 + 校验 + 应用 + 组包），网络与 mock 的调度开销可以忽略——这正是单机 mock 的局限。

## 4. S1-7 三条证据

### 4.1 重启恢复：`GetCursor` 续传 + `applied.json` 恢复

脚本在 poll 阶段结束后杀掉 agent 再起（mock 不重启，游标在它内存里）：

```
[INFO  probe_agent] previous /tmp/tmp.isWiKXlpKc/state/applied.json: {
      "agent_version": "0.1.0",
      "sha256": "1a13b8ae8ced3be970176de48df7b1d80f887c27215c09eb8106cd59c5f864fb",
      "version": 12,
      "written_at_unix_s": 1794921844
    }
[INFO  agent::agent_loop] registered as agent-1 (machine host-ba080d849081ade5, group group-1)
[INFO  agent::agent_loop] resuming after window 3865
```

- `resuming after window 3865` 是 `GetCursor` 的返回值（`agent/src/agent_loop.rs:456`），
  与重启前 `/state` 里的 `cursor: 3865` 一致；`applied.json` 里的 version 12 被读回，agent 没有重放旧配置。
- 重启后 mock 侧收到的新窗口是 `3875`（`state-restart.json`，`cursor` 随之更新为 3875），> 3865，
  即从游标之后继续，不重复提交已 ack 的窗口。
- 一个诚实的小插曲：重启后第一条 `ReportWindows` 带的是 `window_ids:[387]`——那是 agent 在第一个
  `GetConfig` 落地前用 **bootstrap 的 10 s 窗口**算出来的号（`10 s × 387 ≈ 3870 s`，与投递时刻吻合），
  但它**小于**游标 3865（游标落在 1 s 窗口序列上，两个序列不同刻度）。这说明「窗口号可能乱序到达」
  是真实存在的，控制面的 upsert 语义（`(agent_id, window_id, key_hash)`）必须容忍它；这也是 L3 的一部分。

### 4.2 落后自动拉取：心跳间隔内发起 `GetConfig`

agent 已应用到 v7，随后 mock 发布 v8（字节不同）。agent 处于「落后」心跳档（1 s）：

```
13:23:59.659Z  mock 发布 v8
13:24:00.629Z  mock 收到 v8 的 ack  ← 差 970 ms（`g1.csv` 第 8 行，UTC）
[INFO  agent::agent_loop] applied config version 8 (1 rules, 1 degraded)
[INFO  agent::agent_loop] acked config version 8 as Applied (recorded: true)
```

970 ms < `heartbeat_interval_ms` (1000 ms)，且 `state-poll.json` 里 `GetConfig: 7` 与 ack 次数吻合
（7 次 GetConfig 覆盖 v7–v12 加 1 次重连），说明这次拉取由心跳携带的 `desired_version` 触发，
不是 Watch（该阶段 `watch_subscriptions: 0`、`watch_pushes: []`）。空闲态那次 4955 ms 的拉取同理，
只是落在 5 s 档。

### 4.3 ack 里带 `applied_at`

````json
// /acks 里 v2 的记录（push 阶段第 1 次计量）
{"applied_at_ms": 1794921833619, "apply_ms": 0,
 "content_sha256": "4504083eae92bc6065d87a6f7b28b5b236c7abdbd46eb7c6c64e29626bce79e1",
 "degraded_features": ["rules"], "error_code": "",
 "received_at_ms": 1794921833620, "result": "APPLIED", "result_code": 1,
 "version": 2, "watch_used": true}
````

三个字段是同一条 ack 的：`applied_at_ms`（agent 应用完成的时刻）、`received_at_ms`（mock 收到的时刻，
G1 取这个，契约 §9）、`content_sha256`（agent 对**收到的字节**算的，与 `/publish` 返回的 `sha256` 相同）。
`degraded_features:["rules"]` 与 L2 一致——文档 B 带 1 条规则，agent 收下但不执行。

## 5. 偏离与顺带修的东西

与契约 §11 的 14 条同一批（三列，便于对照）。「用户决策」= 这些偏离是需求方拍板的，不是实现妥协。

| # | 计划口径 | 本分支实际 | 影响 |
| --- | --- | --- | --- |
| 1 | `/agent/v1/*` HTTP + JSON | gRPC `control.agent.v1.AgentControl`，独立 9090（用户决策） | 需要 proto 工具链；换来单一 schema 与原生流 |
| 2 | notify 回调，500 ms × 3 | `Watch` 服务端流 + 回退轮询（用户决策：不监听入站端口） | 真控制面未实现，退化为心跳间隔（L4） |
| 3 | 每 10 s 心跳 | 落后 1 s / 追平 5 s，由响应下发 | 实测落后 970 ms、空闲 4955 ms，符合下发值 |
| 4 | 空闲 5 s 心跳 | 同右（合并原文两处矛盾） | 空闲态最坏等 5 s，L4 |
| 5 | register = 引导令牌 + CSR | 预置证书 mTLS（用户决策） | 没有发放与轮换流程，L5；吊销只靠状态列 |
| 6 | agent 拒绝 `version<=current`(409)、hash 不符(422) | `up_to_date=true` 走正常路径；hash 不符由 agent 判并 ack `REJECTED` | gRPC 里没有 409；正常路径不污染 metrics |
| 7 | 规则匹配 + block | 恒 `PASS`、`rule_index=0`，`rules` 降级接受 | E6 未实现；ack 必须带 `degraded_features`（L2） |
| 8 | fail-closed 全丢 | 未实现，实际是 **fail-open** | 见 §6，L1 |
| 9 | 游标只存内存 | 控制面为权威，重启先 `GetCursor` | 与 S1-7 冲突时取 S1-7；§4.1 已验 |
| 10 | 上报样例 `action:"block"` / `r-8080` | 样例改 pass，但保留文档 B 作编码向量 | 样例能被真实复算（含 `content_sha256`） |
| 11 | 时间 `10:00:00Z` | 毫秒精度（`Timestamp` 纳秒位为 0） | 毫秒级 `first_seen` 可比 |
| 12 | `window_id` 跨重启/跨机器稳定 | 明确来自 `CLOCK_MONOTONIC`，只承诺进程重启续传 | boot epoch 留 E8（L3）；§4.1 的 344 就是乱序实例 |
| 13 | 上报批 1 MiB | 4 MiB / 1000 窗口 | proto 比 JSON 紧凑，同一上限会过早拆批 |
| 14 | G1 = 发布 API 200 → ack | mock `/publish` → ack（本报告 §3） | 无发布 API，口径不同（L6） |

**本分支自己的两处偏离**（不在 §11 里，是实现时被迫做的取舍）：

- **扩展了 dev-only 的 `control-mock`**：原来的 mock 只能被 Cargo 测试调用（`MockShared::publish`），
  没有 `/health`、没有发布入口，连 `--certs` 这个参数名都不存在（是 `--print-certs`）。为了让
  「10 次发布 → ack」可复现，新增了 HTTP admin（`/health`、`/publish`、`/acks`、`/state`）、
  `--admin-listen`、`--certs` 别名，并让零配置启动时自动发布契约 §6.4 的文档 B。全部在 `mock` feature 下，
  不进入产品路径，也不影响 `probe-agent` 的行为；测试用 `MockControl::start` 的用法不变。
- **`rules` 已冻结但 Train 0 不执行**：README 与报告都按 L2 写「降级应用」，没有把它写成「规则已生效」。

**顺带修的一个拦截性 bug**（`agent/src/agent_loop.rs`，Watch 分支）：通知到达时只置了「有新版本」，
没有清掉上一次的 `desired_sha256`。于是 `sync_config` 看到 `desired_version > applied_version` 且
`desired_sha256 == applied_sha256`，直接走同内容快捷路径——**不下载、不校验、不应用，却 ack 了新版本**，
并且把旧 hash 回给控制面。第一次冒烟就是这个现象：publish v2 后 `ack v2`、`content_sha256` 仍是 v1 的
`c3e4ee…`、没有 `applied config version 2` 日志、`GetConfig` 计数不增。修法是一行
`self.desired_sha256.clear();`（通知不带 body，旧 hash 不再可信）。修完后 v2 正确打印
`applied config version 2 (1 rules, 1 degraded)`。这条属于「如果不修，G1 数据全是假的」，所以放在本分支。

## 6. 已知限制

照抄契约 §10（验收材料要求原样）：

| # | 限制 | 影响 |
| --- | --- | --- |
| L1 | 数据面恒放行，没有 XDP drop 路径 | 实际行为是 **fail-open**，与计划 :190 相反；不得写成「已 fail-closed」 |
| L2 | `rules` 不在 agent 执行 | 带规则的配置只能降级应用，ack 里必须出现 `degraded_features:["rules"]` |
| L3 | `window_id` 来自 `CLOCK_MONOTONIC` | 宿主重启后回零，会 upsert 覆盖上一段运行的同号行；只承诺进程重启续传 |
| L4 | 真控制面的 `Watch` 返回 `UNIMPLEMENTED` | 配置生效延迟退化为心跳间隔（落后 1s / 追平 5s）；推送路径只在 stub 上被测过 |
| L5 | 单一 CA、无吊销列表 | 吊销只靠 `control.agent.status`，不校验 CRL/OCSP；证书轮换不做 |
| L6 | 无发布 API 的端到端链路 | G1 只能以 stub 为基准测「推送/心跳 → 拉取 → 校验 → ack」，不是计划 :30 的「发布 API 200 → ack」 |

**数据面恒放行，Train 0 是 fail-open，不是 fail-closed。**

补充两条本分支实测到的边界：

- 证书是每次运行现签的一次性 CA，`control-mock` 退出后目录即弃；没有任何持久化或轮换语义（L5）。
- agent 重启后第一条上报可能带 bootstrap 窗口号（§4.1 的 344），控制面必须按 upsert 处理乱序窗口（L3）。

## 7. 没做的事（一句一条）

- 真控制面的七个 RPC 服务端（Spring + Postgres），包括 `desired_version` 查询与 upsert。
- 真控制面的 `Watch` 实现（现在只会 `UNIMPLEMENTED`）。
- `config_version.document` 改 `bytea`、`control.agent` 加列、发布 API（Spring 侧迁移）。
- boot epoch（窗口号跨宿主重启的单调性），需要改主键，留 E8。
- 证书签发/轮换/吊销列表（CRL/OCSP），L5。
- 规则执行（E6）：`rules` 只解析、不匹配、不丢包。
- `probe` 二进制写 CSV 的行为没变，也没有把 CSV 数据接到上报链路（`--csv-dir` 时窗口不上报）。

## 8. 怎么复现

干净 checkout（`feat/agent-control-plane`），容器 `aya-dev` 里：

```bash
cd /workspace/probe
protoc --version                              # 构建前置：本容器已是 libprotoc 3.21.12
cargo build -p agent --features mock --bins   # probe-agent + control-mock
bash verification/run_control_demo.sh         # root；退出码 0 = 10 次全部 ack
```

产物在工作区外层：`verification/control/g1.csv`、`mock-*.log`、`agent-*.log`、`acks-*.json`、
`state-*.json`、`phases.txt`。重跑会覆盖，端口随机（19000–19800）、临时目录 `mktemp -d`，
所以可以反复跑。`CONTROL_DEMO_KEEP=1` 保留证书与 mock 配置，`CONTROL_DEMO_OUT=<dir>` 换输出目录。

口径核对：§3 的每个数都能在上面三个文件里找到——CSV 的 `publish_to_ack_ms` 是
`ack_ms - publish_ms`，P95 取该口径 5 行的最大值（最近秩）。
