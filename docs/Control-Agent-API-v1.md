# Agent ↔ 控制中心 接口契约 v1（gRPC，Train 0）

- 状态：Rust 侧（§13.1）已落地并有验收证据；Spring 侧（§13.2）待下一分支
- 日期：2026-11-14（同日首版为 REST + JSON，后改判 gRPC，依据见 §14）
- 适用：`probe` 仓库的 Rust agent 与 `control-center` 的 Spring 控制面，Train 0
- 上游文档：`docs/Train0-Plan-6x1.md`。冲突时以本文 §11 为准，§11 逐条说明为什么改。
- **结构以 §4 的 `control.proto` 为准，语义以本文其余部分为准。**

本文只定义 agent ↔ 控制面这一条链路。管理面 `/api/*` 与管理→控制 `/internal/v1/*` 不在范围。

---

## 0. 契约的三个组成部分

| 部分 | 内容 | 判定手段 |
| --- | --- | --- |
| 结构 | §4 一份 `control.proto`，两侧由它生成类型 | `protoc` 编译通过；两侧生成类型的字段号必须一致 |
| 语义 | 7 个 RPC、错误码、幂等、超时退避、状态机 | 逐条实现 + L4 集成测试 |
| 数据 | `ConfigDocument` 的序列化字节、`key_hash` 字节布局 | **测试向量**（§6.4、§7.3），两侧必须复算出同样的值 |

判定「两侧一致」的唯一手段是测试向量：`content_sha256` 两条、`key_hash` 三条。任一侧算不出来就是没对齐，禁止用「看起来一样」通过。

> 相对 REST 首版的变化：结构从「文档描述 + JSON Schema」变成 proto 本身，附录的 JSON Schema 因此删除；`content_sha256` 从「对 RFC 8785 规范化文本求哈希」变成「对传输字节求哈希」，规范化那一整节随之取消（理由见 §6.4 与 §14）。

---

## 1. 传输、端口与身份

### 1.1 协议与端口

- HTTP/2 over TLS 1.3，**mTLS**，ALPN 必须协商为 `h2`。明文 h2c 一律不实现、不降级。
- 独立端口 **9090**，只在 `CC_ROLE=control` 实例上启动；`mgmt` 实例不监听 9090。
- 为什么不复用 8080/8082：9090 是**机器身份域**（客户端证书），8080 是**人身份域**（Web、`/api/health`、`/api/machines`、k8s probe）。混在一个端口上的话，`clientAuth=REQUIRE` 会强加给浏览器与 probe，等于把整站逼上 mTLS。
- 服务端：Spring gRPC 的 **Netty** server factory（`spring.grpc.server.port=9090`），TLS 由 `GrpcSslContexts.forServer(cert, key).trustManager(ca).clientAuth(REQUIRE)` 配置。不用 servlet 模式——servlet 模式要求整站开 HTTP/2 并共用容器 TLS，会牵动现有 MVC 与 probe 配置，收益为零。
- 客户端：tonic + rustls，`ClientTlsConfig` 带 CA 与客户端证书，`domain_name` 由 `--tls-server-name` 覆盖（默认 `control-center`）。

### 1.2 身份

| 项 | 规定 |
| --- | --- |
| CA | Train 0 单一自签 CA（`ca.pem`），控制面 `trustManager` 用它校验 client cert |
| 客户端证书 | 每台 agent 一份 cert/key，预置磁盘，路径由 CLI `--tls-cert` / `--tls-key` / `--tls-ca` 给出 |
| `agent_id` | **只从 client cert 的 CN 取**（`CN=agent-m1`）。消息里没有 `agent_id` 字段，也不接受 |
| 证书指纹 | 由**服务端**从握手里的证书算（`sha256(DER)` 小写 hex），与 `control.agent.cert_fingerprint` 比对；agent 不上报 |
| 吊销 | `control.agent.status='revoked'` → 所有 RPC 返回 `FAILED_PRECONDITION` + `agent.revoked`；Train 0 不做 CRL/OCSP |
| server 校验 | agent 必须校验服务端证书链与主机名；不允许 `insecure` 开关进生产构建 |

REST 首版让 agent 在 register 里自报 `cert_fingerprint_sha256`，本版删掉：服务端手里就有证书，自报既多余又可伪造（自报值只能用来「比对」，而比对的对象本来就该由服务端自己算）。

### 1.3 测试用证书

- 测试现场用 `rcgen` 生成 CA + agent 证书，写临时目录，**不提交任何私钥**。
- 假控制面（stub）必须真跑 TLS 并校验 client cert；**用另一张证书连接必须握手失败**，这条要写成测试用例。没有这条，「mTLS 通过」是假的。

---

## 2. 通用约定

### 2.1 proto 与演进

- proto3，包名 `control.agent.v1`。
- 字段号一旦发布**不可复用、不可改类型**；删字段必须 `reserved <n>; reserved "name";`。
- 新增字段必须可缺省（proto3 标量天然缺省；需要区分「未设置」时用 `optional`）。控制面不得依赖 agent 尚不理解的字段。
- 枚举只能追加值；收到未知枚举值按 `UNSPECIFIED` 处理并记日志，不得 panic（proto3 会保留原值，两侧都要防「未来值把当前版本搞崩」）。

### 2.2 时间

- 一律 `google.protobuf.Timestamp`，UTC。发出时毫秒精度（纳秒位为 0）。
- 日志与验收材料用 RFC 3339、固定 3 位小数：`2026-11-20T10:00:02.113Z`。
- 跨机器比较只用控制面时钟（§9）；agent 的 `applied_at` 只用于人看时间线。

### 2.3 关联与观测

- metadata `x-request-id: <uuid v4>`，agent 生成；控制面回显在 status 的 **trailers** 里并写日志；agent 日志首行打印它。计划 :313 的「一次点击串到两端」靠它。
- 每个响应带 `server_time`，agent 只记录偏差，不校时。

### 2.4 错误

- 非 OK 一律：gRPC status + **trailer `x-error-code`**（闭合表见 §12），`status.description` 只给人看，agent 不得解析它做判断。
- 不用 `google.rpc.ErrorInfo`：那要额外 vendor `google/rpc/error_details.proto`，两侧各多一套依赖，Train 0 不值当。一个 trailer 足够；E8 需要结构化 details 时再补。
- **可重试性只看 status code**（§12.1），不看 `x-error-code`。

### 2.5 幂等

| RPC | 幂等键 | 重复调用的结果 |
| --- | --- | --- |
| Register | `agent_id`（来自证书） | 覆盖 `agent_version`/`boot_id`/`last_heartbeat`，返回同一绑定 |
| Heartbeat | 无状态 | 可重试 |
| GetConfig | 无状态 | 可重试 |
| AckConfig | `(agent_id, version)` | 覆盖，保留首次 `applied_at` |
| ReportWindows | `(agent_id, window_id, key_hash)` | `duplicates` 计数 +1，不报错 |
| GetCursor | 无状态 | 可重试 |
| Watch | 无状态（流） | 重连即重新订阅 |

### 2.6 超时与退避

| 调用 | deadline | 失败退避 |
| --- | --- | --- |
| Register | 5s | 1s / 2s / 4s，上限 10s |
| Heartbeat | 2s | 1s / 2s / 4s，上限 30s |
| GetConfig | 5s | 2s / 5s / 10s，上限 30s（计划 :189） |
| AckConfig | 5s | 1s / 2s / 4s，上限 10s，最多 5 次；失败不阻塞上报 |
| ReportWindows | 10s | 1s / 2s / 4s，上限 30s |
| GetCursor | 3s | 同 GetConfig |
| Watch | 无 deadline（长连） | 重连 1/2/4/8…，上限 60s |

- 只对 `UNAVAILABLE` / `DEADLINE_EXCEEDED` / `RESOURCE_EXHAUSTED` 重试；其余按 §12 的动作处理。
- `RESOURCE_EXHAUSTED` 带 trailer `x-retry-after-ms`；agent 取 `max(该值, 退避)`。
- 同一时刻每类请求最多一个在途（单飞），避免重连风暴。
- 优雅关闭总预算 5s：停止窗口扫描 → 当前窗口以 `complete=false` 尽力上报（deadline 2s）→ 退出。

### 2.7 大小上限

| 项 | 上限 | 越界行为 |
| --- | --- | --- |
| `ConfigEnvelope.document` | 64 KiB | 控制面发布时拒绝：`INVALID_ARGUMENT` + `config.too_large` |
| `ReportWindowsRequest` | 1000 个窗口 或 4 MiB | `RESOURCE_EXHAUSTED` + `report.batch_too_large`，agent 拆批 |
| 其他消息 | 1 MiB | `RESOURCE_EXHAUSTED` + `request.too_large` |
| `rules` 条数 | ≤ 20（计划 :212） | 发布时拒绝：`config.invalid_document` |

- 两侧都把 `max_decoding_message_size`（Rust）/ `maxInboundMessageSize`（Java）显式设为 4 MiB，不要依赖库默认值随版本变化。
- 4 MiB 是相对 REST 版 1 MiB 的上调：proto 批量约为 JSON 的 1/4，同样的窗口数不再撞上限。

---

## 3. RPC 总览

| # | RPC | 方向 | 流 | Train 0 | 用途 |
| --- | --- | --- | --- | --- | --- |
| 1 | `Register` | agent → 控制面 | unary | 实现 | 首联、能力协商、拿心跳间隔 |
| 2 | `Heartbeat` | agent → 控制面 | unary | 实现 | 存活 + `desired_version` 对账 |
| 3 | `GetConfig` | agent → 控制面 | unary | 实现 | 拉全量配置快照 |
| 4 | `AckConfig` | agent → 控制面 | unary | 实现 | 上报应用结果（含降级） |
| 5 | `ReportWindows` | agent → 控制面 | unary | 实现 | 窗口聚合上报 |
| 6 | `GetCursor` | agent → 控制面 | unary | 实现 | 拿已收下的最大窗口号 |
| 7 | `Watch` | 控制面 → agent | server stream | 契约冻结；真控制面可 `UNIMPLEMENTED`，stub 必须实现 | 新版本推送 |

`Watch` 在 Train 0 的规则：

- 控制面若未实现，**必须返回 `UNIMPLEMENTED`**（不是关连接、不是挂住）。
- agent 必须把 `UNIMPLEMENTED` 视为「没有推送通道」，回退心跳轮询，并以 60s 上限退避重试。
- **stub 必须实现 Watch**，用来测 agent 的推送路径（§9 的 G1 带推送口径）；真控制面在 E8 前返回 `UNIMPLEMENTED` 是合规的。

---

## 4. `control.proto`（规范来源）

### 4.1 这段是可编译的

已用容器内 `libprotoc 3.21.12` 校验：

```
protoc --proto_path=. --descriptor_set_out=/dev/null control.proto
```

两侧实现时把下面这段**原样**抽成 `probe/agent/proto/control.proto` 与 `control-center/app/src/main/proto/control.proto`；两份必须逐字节相同，CI 里加一条 `diff` 检查即可（比任何文档走查都便宜）。

```proto
syntax = "proto3";

package control.agent.v1;

import "google/protobuf/timestamp.proto";

option java_package = "io.probe.control.agent.v1";
option java_multiple_files = true;
option java_outer_classname = "AgentControlProto";

// Agent <-> control-center contract, Train 0 / v1.
// Transport: HTTP/2 + mutual TLS on a dedicated port (9090).
// Semantics live in docs/Control-Agent-API-v1.md; this file is the normative shape.
service AgentControl {
  rpc Register(RegisterRequest) returns (RegisterResponse);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  rpc GetConfig(GetConfigRequest) returns (GetConfigResponse);
  rpc AckConfig(AckConfigRequest) returns (AckConfigResponse);
  rpc ReportWindows(ReportWindowsRequest) returns (ReportWindowsResponse);
  rpc GetCursor(GetCursorRequest) returns (GetCursorResponse);
  rpc Watch(WatchRequest) returns (stream ConfigNotification);
}

// ---------------------------------------------------------------------------
// Enums whose zero value is a real ABI value (they mirror probe-common).
// 0 is therefore meaningful and must never be treated as "unset".
// ---------------------------------------------------------------------------

// Mirrors AggKey.family.
enum Family {
  FAMILY_ANY = 0;
  FAMILY_IPV4 = 4;
  FAMILY_IPV6 = 6;
}

// Mirrors AggKey.protocol (IANA protocol numbers).
enum L4Protocol {
  L4_PROTOCOL_ANY = 0;
  L4_PROTOCOL_ICMP = 1;
  L4_PROTOCOL_TCP = 6;
  L4_PROTOCOL_UDP = 17;
  L4_PROTOCOL_ICMPV6 = 58;
}

// Mirrors AggKey.action (ACTION_PASS = 0, ACTION_BLOCK = 1).
enum Action {
  ACTION_PASS = 0;
  ACTION_BLOCK = 1;
}

// Mirrors ProbeConfig.default_action (pass = 0, drop = 1).
enum DefaultAction {
  DEFAULT_ACTION_PASS = 0;
  DEFAULT_ACTION_DROP = 1;
}

// ---------------------------------------------------------------------------
// Enums where 0 means "not set" and is rejected by validation.
// ---------------------------------------------------------------------------

enum AgentStatus {
  AGENT_STATUS_UNSPECIFIED = 0;
  AGENT_STATUS_ACTIVE = 1;
  AGENT_STATUS_REVOKED = 2;
  AGENT_STATUS_PENDING = 3;
}

enum ConfigState {
  CONFIG_STATE_UNSPECIFIED = 0;
  CONFIG_STATE_MISSING = 1;
  CONFIG_STATE_APPLYING = 2;
  CONFIG_STATE_APPLIED = 3;
  CONFIG_STATE_REJECTED = 4;
}

enum AckResult {
  ACK_RESULT_UNSPECIFIED = 0;
  ACK_RESULT_APPLIED = 1;
  ACK_RESULT_REJECTED = 2;
  ACK_RESULT_FAILED = 3;
}

// ---------------------------------------------------------------------------
// Capabilities and registration
// ---------------------------------------------------------------------------

message Capabilities {
  // Supported document schema_version values. Train 0: [1].
  repeated uint32 config_schema = 1;
  // Supported default_action values. Train 0: [DEFAULT_ACTION_PASS].
  repeated DefaultAction default_action = 2;
  // Whether the agent can enforce rules. Train 0: false.
  bool rules_execution = 3;
}

message RegisterRequest {
  // Stable host identity (machine-id / serial). 1..128 chars.
  string machine_id = 1;
  // Semver of the agent binary.
  string agent_version = 2;
  // Random per process start; for log correlation only, never part of an id.
  string boot_id = 3;
  Capabilities capabilities = 4;
}

message RegisterResponse {
  // Derived from the client certificate CN; never from the request body.
  string agent_id = 1;
  string machine_id = 2;
  string group_id = 3;
  AgentStatus status = 4;
  // 0 = no config published for this group yet.
  uint64 desired_version = 5;
  // 32 bytes; empty when desired_version = 0.
  bytes desired_sha256 = 6;
  // Interval while lagging (desired != applied).
  uint32 heartbeat_interval_ms = 7;
  // Interval once caught up.
  uint32 idle_heartbeat_interval_ms = 8;
  google.protobuf.Timestamp server_time = 9;
  // Control plane's view of what this agent can actually apply.
  Capabilities effective_capabilities = 10;
}

// ---------------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------------

message HeartbeatRequest {
  uint64 current_version = 1;
  ConfigState config_state = 2;
  // Largest window id already acknowledged by the control plane (agent's view).
  uint64 last_reported_window_id = 3;
  uint32 pending_reports = 4;
  // True while a Watch stream is established.
  bool watch_connected = 5;
  google.protobuf.Timestamp agent_time = 6;
}

message HeartbeatResponse {
  uint64 desired_version = 1;
  bytes desired_sha256 = 2;
  uint32 heartbeat_interval_ms = 3;
  uint32 idle_heartbeat_interval_ms = 4;
  google.protobuf.Timestamp server_time = 5;
}

// ---------------------------------------------------------------------------
// Config pull
// ---------------------------------------------------------------------------

message GetConfigRequest {
  uint64 current_version = 1;
}

message GetConfigResponse {
  // True: nothing newer than current_version. Not an error.
  bool up_to_date = 1;
  // Present only when up_to_date = false.
  ConfigEnvelope config = 2;
}

message ConfigEnvelope {
  uint64 version = 1;
  uint32 schema_version = 2;
  // sha256 of `document`, 32 bytes. The agent hashes the received bytes as-is.
  bytes content_sha256 = 3;
  // Serialized ConfigDocument. Opaque to the agent: parse, never re-encode for hashing.
  bytes document = 4;
  google.protobuf.Timestamp published_at = 5;
}

message ConfigDocument {
  uint32 schema_version = 1;
  uint32 window_ms = 2;
  uint32 grace_ms = 3;
  uint32 src_prefix_v4 = 4;
  uint32 src_prefix_v6 = 5;
  DefaultAction default_action = 6;
  repeated Rule rules = 7;
}

message Rule {
  // 1-based position; must equal (index in `rules` + 1). 0 is never valid here.
  uint32 rule_index = 1;
  // ^[a-z0-9][a-z0-9._-]{0,31}$, unique within the document.
  string rule_id = 2;
  // 1..1000, unique within the document; lower matches first.
  uint32 priority = 3;
  bool enabled = 4;
  RuleMatch match = 5;
  Action action = 6;
}

message RuleMatch {
  Family family = 1;
  L4Protocol protocol = 2;
  // CIDR text, e.g. "10.0.1.0/24". Absent = any.
  optional string src_prefix = 3;
  // Single address text, e.g. "10.0.2.5". Absent = any.
  optional string dst_addr = 4;
  // 1..65535. Empty = any.
  repeated uint32 dst_ports = 5;
}

// ---------------------------------------------------------------------------
// Config ack
// ---------------------------------------------------------------------------

message AckConfigRequest {
  uint64 version = 1;
  bytes content_sha256 = 2;
  AckResult result = 3;
  // Agent wall clock; for the human timeline only.
  google.protobuf.Timestamp applied_at = 4;
  uint32 apply_ms = 5;
  // Features the agent accepted but cannot enforce, e.g. ["rules"].
  repeated string degraded_features = 6;
  // One of the contract error codes; required when result != APPLIED.
  string error_code = 7;
  // Human readable, <= 512 chars.
  string error_detail = 8;
}

message AckConfigResponse {
  bool recorded = 1;
  uint64 desired_version = 2;
  // Control-plane clock; G1 is measured from this.
  google.protobuf.Timestamp received_at = 3;
  google.protobuf.Timestamp server_time = 4;
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

message ReportWindowsRequest {
  // 1..1000 windows per call (see limits in the contract).
  repeated WindowReport windows = 1;
}

message WindowReport {
  // Monotonic window index: ts_ns / window_ns. Not wall clock.
  uint64 window_id = 1;
  uint32 window_ms = 2;
  // false = partial window cut by shutdown or grace timeout.
  bool complete = 3;
  // Estimated wall clock of the window start (send-time wall clock minus the
  // monotonic offset). For display only; alignment always uses window_id.
  google.protobuf.Timestamp window_start = 4;
  repeated Aggregate aggregates = 5;
  // Set only if the agent had to truncate this window to fit the batch limit.
  bool truncated = 6;
  repeated Sample samples = 7;
}

message Aggregate {
  // 32 black-box bytes, see key_hash layout in the contract.
  bytes key_hash = 1;
  // AggValue.count
  uint64 packets = 2;
  // AggValue.first_seen relative to window_id * window_ms.
  uint64 first_seen_offset_ns = 3;
  // AggValue.last_seen relative to window_id * window_ms.
  uint64 last_seen_offset_ns = 4;
  // AggValue.sample_dropped
  bool sample_dropped = 5;
  // AggValue.degraded (classification degraded, unrelated to ack.degraded_features)
  bool degraded = 6;
}

message Sample {
  // Optional but strongly recommended: links the sample to an aggregate row.
  bytes key_hash = 1;
  // Display only, e.g. "10.0.1.7:53124 -> 10.0.2.5:8080 tcp".
  string five_tuple = 2;
  // Relative to window_id * window_ms.
  uint64 ts_offset_ns = 3;
}

message RejectedWindow {
  uint64 window_id = 1;
  string error_code = 2;
}

message ReportWindowsResponse {
  uint32 accepted = 1;
  uint32 duplicates = 2;
  uint32 rejected = 3;
  repeated RejectedWindow rejected_windows = 4;
  // Largest window id fully committed. Echoed so the agent can advance its cursor.
  uint64 last_window_id = 5;
  google.protobuf.Timestamp server_time = 6;
}

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

message GetCursorRequest {}

message GetCursorResponse {
  // Absent = this agent never delivered a window (start from 0).
  optional uint64 last_window_id = 1;
  google.protobuf.Timestamp server_time = 2;
}

// ---------------------------------------------------------------------------
// Push (Watch). Train 0: the control plane MAY answer UNIMPLEMENTED; agents
// must fall back to heartbeat polling and retry with a long backoff.
// ---------------------------------------------------------------------------

message WatchRequest {
  uint64 current_version = 1;
}

message ConfigNotification {
  uint64 version = 1;
  bytes content_sha256 = 2;
  google.protobuf.Timestamp issued_at = 3;
}
```

### 4.2 枚举值与 ABI 的对应

| proto 枚举 | 取值 | 对应 |
| --- | --- | --- |
| `Family` | `ANY=0, IPV4=4, IPV6=6` | `AggKey.family` |
| `L4Protocol` | `ANY=0, ICMP=1, TCP=6, UDP=17, ICMPV6=58` | `AggKey.protocol`（IANA 号） |
| `Action` | `PASS=0, BLOCK=1` | `AggKey.action` |
| `DefaultAction` | `PASS=0, DROP=1` | `ProbeConfig.default_action` |

刻意用 ABI 数值而不是 1..n：`document → ProbeConfig` 与 `key_hash` 的字节填充都成为恒等映射，少一层转换表，也少一类「映射表抄错」的 bug。

另有三个枚举（`AgentStatus` / `ConfigState` / `AckResult`）用 `UNSPECIFIED=0`，因为它们的「未设置」必须能被发现——尤其 `AckResult`：若 0 表示 APPLIED，agent 漏填一个字段就会变成**假成功**。

---

## 5. RPC 语义

### 5.1 `Register`

首联 + 能力协商。**不是发证**：证书是预置的，这里只登记与对齐参数。

- `agent_id` 来自证书 CN；控制面查不到该 CN → `PERMISSION_DENIED` + `agent.identity_mismatch`。
- `machine_id` 首次绑定后不可变；与既有绑定不一致 → `FAILED_PRECONDITION` + `agent.machine_conflict`。变更归属走管理面，agent 侧不做。
- `capabilities` 是**裁剪依据**（§6.3）；`effective_capabilities` 是控制面裁剪后的结果，agent 只信这个。
- 重复 Register 幂等。
- `desired_version = 0` 表示该组尚无任何已发布配置。
- `boot_id` 每次进程启动随机生成，只用于日志串联，**不参与** `window_id` 计算（§5.5.1）。

### 5.2 `Heartbeat`

- `config_state` 报真实状态：`MISSING`（还没成功应用过任何配置）/ `APPLYING` / `APPLIED` / `REJECTED`。
- `desired_version > current_version` → agent 立即 `GetConfig`。没有 Watch 时这是唯一的发现机制。
- `desired_sha256` 让 agent 在「版本号变了但内容没变」时跳过下载；但 hash 相同仍要校验 `schema_version`。
- 节奏：落后 → `heartbeat_interval_ms`（1s）；追平 → `idle_heartbeat_interval_ms`（5s）。两个值由 Register/Heartbeat 响应下发，不写死在 agent 里。
- `AGENT_STATUS_REVOKED` → agent 停上报、保留数据面现状、非 0 退出。

### 5.3 `GetConfig`

- **「已最新」用 `up_to_date=true` 表达，不用错误码。** REST 首版的 `409 config.no_newer_version` 在 gRPC 下没有对应物：把正常路径塞进错误状态会污染 metrics、触发客户端的重试逻辑，还要 agent 专门写异常分支。同理，组内无配置（`desired_version=0`）也返回 `up_to_date=true`——「有没有配置」由心跳的 `desired_version` 表达，不需要第二个错误码（REST 版的 `404 config.not_published` 一并取消）。
- agent 校验顺序（**必须按此顺序**，便于两侧对同一份坏数据得出同一结论）：
  1. `config.version > current_version`
  2. `schema_version` ∈ `capabilities.config_schema`
  3. `sha256(document)` == `content_sha256`（**对收到的字节求哈希，不重新序列化**）
  4. `document` 能解析成 `ConfigDocument`（proto 解码）
  5. 字段值域（§6.2）
  6. 能力裁剪（§6.3）
- 任一失败都不 apply，并把原因写进 ack 的 `error_code`。
- 全量快照，无增量补丁（计划 :185）。

### 5.4 `AckConfig`

- `result`：`APPLIED`（写 CONFIG 成功 + 回读校验，规矩见 `probe/probe/src/app.rs:178-181`）/ `REJECTED`（文档不合规，**没动数据面**）/ `FAILED`（校验通过但落地失败，数据面可能已变）。
- `ACK_RESULT_UNSPECIFIED` 是非法值：控制面收到按 `request.invalid` 记错，**不得**当成 APPLIED。
- `degraded_features`：接受了但没能生效的能力。Train 0 唯一可能是 `"rules"`。
- `received_at` 是控制面时钟，G1 用它（§9）。
- 幂等：同 `(agent_id, version)` 覆盖，保留首次 `applied_at`。
- `version > desired_version` → `FAILED_PRECONDITION` + `ack.version_unknown`。

### 5.5 `ReportWindows`

- 一个请求可含多个窗口（合批，省 RPC），上限见 §2.7。
- `key_hash` 32 字节原样收，控制面**不重算**——重算等于把两侧对齐问题搬到生产环境天天报警。
- 每行按 `(agent_id, window_id, key_hash)` upsert；已存在的算 `duplicates`。
- `window_id` 早于控制面游标超过 1000 的窗口 → 丢弃并记 `report.window_too_old`（防止宿主重启回零后覆盖上一段运行的同号数据，见 §5.5.1）。
- 响应里的 `last_window_id` 是已提交的最大窗口号，agent 用它推进游标，不必额外调 `GetCursor`。
- `accepted + duplicates + rejected` 必须等于提交的窗口数；不等说明控制面丢行，agent 记日志。

#### 5.5.1 已知局限：宿主重启后窗口号回零

`window_id = ts_ns / window_ns`，`ts_ns` 来自 `CLOCK_MONOTONIC`（`probe/probe/src/aggregate.rs:334-344`）。

- 进程重启继续递增 → S1-7 的「重启恢复」可过。
- **宿主重启回零**，而 `report` 主键是 `(agent_id, window_id, key_hash)`，新行会被 upsert 到上一次开机同号的行上。
- Train 0 只承诺进程重启续传；修法是给窗口号加 boot epoch（`window_id_wire = boot_epoch << 40 | mono_window`），会改主键语义，留到 E8 与控制面表结构一起改。这条必须写进报告，不能含糊。

### 5.6 `GetCursor`

- `last_window_id` 用 `optional`：**不存在**表示这台 agent 从未上报过（从 0 开始），与「上报过 0 号窗口」区分开。
- 语义：已完整收下的最大窗口号（含 `complete=false` 的那一批）；agent 从 `+1` 开始补发。
- 权威在控制面（`max(window_id)` from `report` 表），agent 只做内存缓存。计划 :250 的「游标只存内存」按 S1-7「重启恢复」(:466) 改为**控制面权威**。

### 5.7 `Watch`

- 请求只带 `current_version`；控制面在版本变化时推一条 `ConfigNotification`。通知里**没有配置体**——内容永远从 `GetConfig` 拉，避免两处下发路径。
- 建流后控制面应立即比对一次 `current_version`，落后就马上推一条，避免「订阅晚了一步」的静默窗口。
- 断线：agent 退避重连（上限 60s），重连后先 `GetConfig` 兜底再订阅。
- Train 0：真控制面返回 `UNIMPLEMENTED`（合法），agent 回退轮询；stub 必须实现，用来验证推送路径。

---

## 6. 配置文档 v1

### 6.1 信封

| 字段 | 说明 |
| --- | --- |
| `version` | 对应 `control.config_version.version`，全局单调递增 |
| `schema_version` | 本文定义 = `1` |
| `content_sha256` | `sha256(document)`，32 字节；**对传输的字节求哈希**，不重新序列化 |
| `document` | 序列化后的 `ConfigDocument`，≤ 64 KiB |
| `published_at` | 控制面发布时刻 |

`desired_version` = 该 agent 所属 `group_id` 在 `config_version` 里的 `max(version)`；组内无配置为 `0`。

> 版本号是**全局计数器**，可见性按组过滤。同一组看到的序列可能跳号（7 → 12），agent 只判断 `>`，**不得要求 `+1`**。

### 6.2 `document` 字段与值域

| 字段 | 值域 | 换算到 `ProbeConfig` |
| --- | --- | --- |
| `schema_version` | 必须 = 信封的 `schema_version`（=1） | — |
| `window_ms` | 1000–60000 | `window_ns = window_ms * 1_000_000` |
| `grace_ms` | 0–`window_ms` | `grace_ns = grace_ms * 1_000_000` |
| `src_prefix_v4` | 0–32 | `ipv4_prefix_len` |
| `src_prefix_v6` | 0–128 | `ipv6_prefix_len` |
| `default_action` | Train 0 只允许 `PASS` | `default_action` |
| `rules` | ≤ 20；Train 0 允许非空但**不执行** | 无对应字段，仅日志与 ack 降级 |

规则字段：

| 字段 | 值域/说明 |
| --- | --- |
| `rule_index` | 必须等于数组下标 + 1（**校验项**，不只是约定） |
| `rule_id` | `^[a-z0-9][a-z0-9._-]{0,31}$`，文档内唯一 |
| `priority` | 1–1000，文档内唯一；小者先匹配 |
| `enabled` | `false` 的规则**仍占下标**（保证 `key_hash` 里的序号稳定），执行时跳过 |
| `match.family` / `match.protocol` | 见 §4.2 枚举 |
| `match.src_prefix` / `match.dst_addr` | CIDR / 单地址文本；不设 = 不限 |
| `match.dst_ports` | 1–65535；空 = 不限 |
| `action` | `PASS` / `BLOCK` |

两个必须写进单测的细节：

1. **`DEFAULT_ACTION_PASS = 0` 在 proto3 里被省略**，所以文档 A 的 12 字节里**没有** `default_action` 字段。缺这个字段 = pass，不是「非法」。控制面要下发 `DROP`（E8 才允许）时值为 1，线上看得见，agent 按 §6.3 硬拒绝。
2. `grace_ms <= window_ms` 与「`priority` 唯一」proto 表达不了，靠代码校验。

### 6.3 能力协商（防「ack applied 但没落地」）

| 配置内容 | 允许值 | 越界时 |
| --- | --- | --- |
| `default_action` | Train 0 只有 `PASS` | **硬拒绝**：ack `REJECTED` + `config.unsupported_default_action` |
| `rules` 非空 | `rules_execution=false` | **降级接受**：ack `APPLIED` + `degraded_features:["rules"]` |
| `schema_version` | `capabilities.config_schema` 列出的值 | 拒绝：`config.schema_unsupported` |

为什么区别对待：`default_action` 决定「不匹配时是否放行」，静默忽略等于改了安全语义；`rules` 只是「额外拦截」，降级后流量行为与上一版一致，且 Web 能显示「已应用、规则未生效」。宁可在控制面显式降级，也不要假装拦住了。

### 6.4 测试向量（字节 + 哈希）

**文档 A**（无规则，Train 0 典型配置）。`ConfigDocument` 序列化后 **12 字节**：

```
080110904e18d00f20182840
```

```
content_sha256 = 971d3944b510ab8ea6cbc469ccae565c9ad0a60f27801e9cad052adbe4253c05
```

**文档 B**（含 1 条规则，即 `r-8080`）。**63 字节**：

```
080110904e18d00f201828403a3108011206722d38303830180a20012a1f080410061a0b31302e302e312e302f3234220831302e302e322e352a02903f3001
```

```
content_sha256 = c3e4eea7f545643b966956fbea32fbbc3fb44acc019999dd5ed9552940b3d30f
```

两侧职责**不对称**，单测也不同：

| 侧 | 单测断言 |
| --- | --- |
| 控制面 | 把 `doc_a.txt`（附录 A）编码成 `ConfigDocument` → 字节必须逐字节等于上面 hex（它产生字节，所以必须验证编码器）；`sha256(bytes)` 等于期望值 |
| agent | 把 hex 解码成 `ConfigDocument` → 字段值符合预期；`sha256(收到的字节)` 等于期望值。**agent 不负责产出字节**，所以不需要「重编码后仍相等」的断言 |

这个不对称是设计的一部分：字节只由控制面产生一次并**原样落库**（`config_version` 存 `bytea`），读回不重算。于是「两侧各自规范化、结果不一致 → 每台 agent 都 hash 不匹配」这个失败模式从根上消失——REST 首版正是被这个风险卡住（Java 侧没有成熟的 RFC 8785 实现）。

复算方式见附录 A。

---

## 7. `key_hash`

### 7.1 为什么用二进制而不是文本

计划 :258 只给了 `"9a2f..."` 的示意。用文本拼五元组会踩格式差异：Rust `Ipv6Addr::fmt` 按 RFC 5952 压缩，Java `Inet6Address.getHostAddress()` **不压缩**（`2001:db8:0:0:0:0:0:1` vs `2001:db8::1`），两侧算出的 hash 必然不同，幂等直接失效。所以哈希输入是**定长字节**，不含任何人类可读格式。

这一条不受传输方式影响，gRPC 版与 REST 版完全相同。

### 7.2 字节布局（56 B）

```
偏移  长度  字段            编码
0     13    magic           ASCII "probe.agg.v1" + 0x00    → 70726f62652e6167672e763100
13     1    direction       1 = ingress（DIRECTION_INGRESS）
14     1    family          4 = IPv4, 6 = IPv6
15     1    action          0 = pass, 1 = block（ACTION_PASS/ACTION_BLOCK）
16     1    reason          原因码 u8，0 = 无
17     4    rule_index      规则序号，**1-based**，0 = 未命中；大端
21    16    src_prefix      按前缀长度掩码后的地址，左对齐，其余补 0
37    16    dst_addr        完整地址，左对齐，其余补 0
53     2    dst_port        大端；ICMP/无端口为 0
55     1    protocol        1=ICMP, 6=TCP, 17=UDP, 58=ICMPv6, 0=其他
```

```
key_hash = lowercase_hex( sha256( 上述 56 B ) )
```

要点：

- **不含 `window_id`**：窗口号是 `report` 主键的另一列，含进去会让同一流量在每个窗口都成为新记录，跨窗口聚合就断了。
- IPv4 不做 v4-mapped-v6 映射：前 4 字节放地址、后 12 字节为 0。
- `src_prefix` 必须先掩码（`10.0.1.7/24` → `10.0.1.0`），否则同前缀不同源地址会产生大量 key。
- `rule_index` 用 **1-based** 是为了让 `0` 表示「未命中」——`AggKey.rule_id` 的语义就是 0 = 无规则。
- 字段顺序由 `AggKey`（`probe/probe-common/src/lib.rs:109`）决定：`protocol` 在最后、`dst_port` 在其前，**不要**改成网络序的直觉顺序。

### 7.3 测试向量

| # | 场景 | `key_hash` |
| --- | --- | --- |
| A | IPv4 TCP `10.0.1.0/24 → 10.0.2.5:8080`，pass，未命中规则（`rule_index=0`、`reason=0`） | `2bd3476570235caf978971eac565dc1e63931786ae3c817bd31ea795c3e23c4a` |
| B | 同上五元组，block，命中 `rule_index=1`、`reason=1` | `2da4fcf437006d16db67fb98e71cc0d6e5248715e11ef6d4811786a7b6f8f115` |
| C | IPv6 UDP `fd00:99::/64 → [fd00:99::1]:5353`，pass | `d0e9cc4a039d43a800cfeb64950ec348e59b3089c84914a5d8cde15f5c775624` |

三者的 56 B 输入（hex，每行一条，可直接 `sha256 -`）：

```
70726f62652e6167672e76310001040000000000000a0001000000000000000000000000000a0002050000000000000000000000001f9006
70726f62652e6167672e76310001040101000000010a0001000000000000000000000000000a0002050000000000000000000000001f9006
70726f62652e6167672e7631000106000000000000fd000099000000000000000000000000fd00009900000000000000000000000114e911
```

对照 §7.2 的分段：A 与 B 只差 `action` / `reason` / `rule_index` 三个字段，可作为「字段顺序抄错」的探针——若 B 算出来等于 A，说明 `rule_index` 被漏进哈希或顺序错位。

---

## 8. 上报与配置应用的状态机（agent 侧）

```
启动
 ├─ Register（退避 1/2/4s，上限 10s）
 │   ├─ agent.revoked → 非 0 退出
 │   └─ ok → agent_id / group_id / 心跳间隔 / desired_version
 ├─ GetCursor → last_window_id（可选；缺省从 0）
 ├─ GetConfig(current_version=0)
 │   ├─ up_to_date=true  → config_state=MISSING，进循环（组内还没配置）
 │   └─ config 到达      → 校验(§5.3 六步) → 写 CONFIG → 回读 → attach → AckConfig
 │        ├─ REJECTED → config_state=REJECTED，等新版本
 │        └─ APPLIED  → config_state=APPLIED
 ├─ Watch 订阅（后台；UNIMPLEMENTED → 停止重试、改轮询，60s 后再试一次）
 └─ 心跳 / 上报循环
     ├─ 每 heartbeat_interval：Heartbeat（落后 1s，追平 5s）
     ├─ desired > current → GetConfig（单飞）
     ├─ 新关窗 → ReportWindows → 用响应 last_window_id 推进游标
     └─ SIGTERM → 切 partial 窗口 → 尽力上报（deadline 2s）→ 退出
```

不可重入点：`applyConfig` 与 `GetConfig` 互斥；上报与心跳可并行，但上报失败不能阻塞心跳。

### 8.1 `fail-closed` 的现状（必须写进报告，不能含糊）

计划 :190 要求「配置未加载时数据面 fail-closed 全丢」。**Rust 侧 Train 0 做不到**：`probe/README.md:540-543` 明确 `action` 恒 `pass`、`rule_id=0`、`reason=0`、不读 `default_action`，E6/E7 未实现，没有 XDP drop 路径。

本契约的临时规定：

- 未拿到配置时 agent **不 attach** XDP（等价于放行），心跳报 `config_state=MISSING`。
- 因此当前行为是 **fail-open**，与计划 :190 相反。这条必须写进 verification 报告与 `README.md` 的已知限制，**不得**在验收材料里写成「已 fail-closed」。
- 真正的 fail-closed 依赖 E6 落地（`default_action=DROP` + 匹配到即 drop），届时只需把 `default_action` 解禁，契约其余部分不变。

---

## 9. 时间与 G1 口径

- 端到端时延 = `published_at`（控制面发布）→ `received_at`（控制面收到 ack）。
- **两条路径分开量，报告必须写明用的是哪条**：

| 路径 | 组成 | 目标 |
| --- | --- | --- |
| 带 `Watch`（stub 可测） | 推送 → apply → ack，无轮询 | P95 **≤ 500ms**（计划 :30 要求 ≤3s，留足余量） |
| 纯轮询（真 Spring 侧在 E8 前） | 最坏 = 一个心跳间隔 + RTT + apply | 落后态（1s）P95 ≤ 3s 可达；空闲态（5s）最坏 ~5.5s |

- 所以：**G1 验收必须在「已 applied 的基线」上发新版**，并允许 1s 的轮询滞后。用 stub 测的是「推送/心跳 → 拉取 → 校验 → ack」，与计划 :30 的「发布 API 200 → ack」不是同一口径，报告要写清偏离（§11 最后一行）。
- agent 的 `applied_at` 只用于人看时间线，不参与 P95 计算。

---

## 10. Train 0 已知限制（汇总，验收材料必须照抄）

| # | 限制 | 影响 | 详见 |
| --- | --- | --- | --- |
| L1 | 数据面恒放行，没有 XDP drop 路径 | 实际行为是 **fail-open**，与计划 :190 相反；不得写成「已 fail-closed」 | §8.1 |
| L2 | `rules` 不在 agent 执行 | 带规则的配置只能降级应用，ack 里必须出现 `degraded_features:["rules"]` | §6.3 |
| L3 | `window_id` 来自 `CLOCK_MONOTONIC` | 宿主重启后回零，会 upsert 覆盖上一段运行的同号行；只承诺进程重启续传 | §5.5.1 |
| L4 | 真控制面的 `Watch` 返回 `UNIMPLEMENTED` | 配置生效延迟退化为心跳间隔（落后 1s / 追平 5s）；推送路径只在 stub 上被测过 | §3、§9 |
| L5 | 单一 CA、无吊销列表 | 吊销只靠 `control.agent.status`，不校验 CRL/OCSP；证书轮换不做 | §1.2 |
| L6 | 无发布 API 的端到端链路 | G1 只能以 stub 为基准测「推送/心跳 → 拉取 → 校验 → ack」，不是计划 :30 的「发布 API 200 → ack」 | §9、§11 |

Train 0 期间若某条口径被迫调整（例如 gRPC 库版本导致的 metadata 行为差异），在本节追加一行日期 + 原因，不要直接改正文而不留痕。

---

## 11. 与 `Train0-Plan-6x1.md` 的差异（逐条）

| 计划 | 本文 | 理由 |
| --- | --- | --- |
| `/agent/v1/*` HTTP + JSON 接口表（:315-336） | gRPC `control.agent.v1.AgentControl`，独立端口 9090 | 用户决策；proto 单一 schema + 推送 + 免规范化（§14） |
| notify 回调，500ms × 3（:176、:333） | `Watch` 服务端流；Train 0 控制面可返回 `UNIMPLEMENTED`，agent 回退轮询 | 回调要求 agent 监听入站端口；流式推送没有这个代价，且 `70-netpol.yaml` 只放行 mgmt→control。实测（mock 支持 Watch）：推送路径 5 次 5–22 ms；不支持 Watch 时纯轮询 934–977 ms |
| 每 10s 心跳（:179） | 落后 1s / 追平 5s，由响应下发 | 覆盖「通知丢失」的时效；写死不如下发 |
| 空闲 5s 心跳（:248） | 同右（合并口径） | 原文两处矛盾，取 5s 为追平态 |
| register = 引导令牌 + CSR（:329） | 预置证书 mTLS，Register 仅登记/协商 | 用户决策；控制面 Train 0 不发证，省掉令牌生命周期 |
| 「agent 拒绝 `version<=current`(409)、hash 不符(422)」（:183） | `up_to_date=true`（正常路径）；hash 不符由 agent 判 → ack `REJECTED` + `config.hash_mismatch` | agent 没有响应面；gRPC 里没有「409」，把正常路径做成错误会污染 metrics 与重试逻辑 |
| 规则匹配 + block（:195-197） | 恒 `PASS`、`rule_index=0`、`rules` 降级接受 | E6 未实现（`probe/README.md:540-543`） |
| fail-closed 全丢（:190） | 未实现（fail-open）：不 attach + `config_state=MISSING` | 需 XDP drop；见 §8.1 |
| 游标只存内存（:250） | 控制面为权威，重启先 `GetCursor` | 与 S1-7「重启恢复」(:466) 冲突，取 S1-7 |
| 上报样例 `action:"block"`、`rule_id:"r-8080"`（:264-265） | 样例改为 pass / 未命中，但保留「含 1 条规则」的文档 B 作编码向量 | 让样例可被执行侧真实复算（附 `content_sha256` 与 `key_hash` 向量） |
| 时间 `10:00:00Z`（:256） | 毫秒精度（`Timestamp` 纳秒位为 0） | 毫秒级 `first_seen` 要能比 |
| `window_id` 作为跨重启/跨机器稳定标识（:249 隐含） | 明确来自 `CLOCK_MONOTONIC`，宿主重启回零；Train 0 只承诺进程重启续传 | 与实现一致（`aggregate.rs:334-344`）；加 boot epoch 要改主键，留 E8（§5.5.1） |
| 上报批 1 MiB 级 | 4 MiB / 1000 窗口 | proto 比 JSON 紧凑约 4×，同一上限会过早拆批 |
| 计划 :30「发布 API 200 → ack P95 ≤3s」 | stub 口径 = mock 的 `/publish` → 拉取 → 校验 → ack（推送 5 次，P95 22 ms；纯轮询 5 次，P95 977 ms，见 `probe/verification/REPORT-control-plane.md`） | Train 0 的 Spring 侧没有发布 API，无法按原口径测；两张数都只在 mock 上成立，不代表生产 |

---

## 12. 错误码（闭合）

### 12.1 传输层：gRPC status → 可重试性

| status | 可重试 | 典型触发 |
| --- | --- | --- |
| `UNAVAILABLE` | 是 | 控制面未启动 / 连接被拒 / 连接被重置 |
| `DEADLINE_EXCEEDED` | 是 | 超过 §2.6 的 deadline |
| `RESOURCE_EXHAUSTED` | 是（按 `x-retry-after-ms`） | 过载 / 批太大 |
| `INTERNAL` | 是 | 未预期异常 |
| `INVALID_ARGUMENT` | 否 | 字段非法、必填缺失、`UNSPECIFIED` 枚举 |
| `UNAUTHENTICATED` | 否（退出） | 没带 client cert / 证书链不受信 |
| `PERMISSION_DENIED` | 否（退出） | CN 查不到 / 指纹不符 |
| `FAILED_PRECONDITION` | 否（按 `x-error-code` 处理） | 已吊销 / machine 冲突 / ack 版本未知 |
| `NOT_FOUND` | 否 | 未 Register 就调其它 RPC |
| `UNIMPLEMENTED` | 否（回退轮询） | `Watch` 未实现 |

### 12.2 `x-error-code` 闭合表

| `code` | status | 触发条件 | agent 动作 |
| --- | --- | --- | --- |
| `request.invalid` | `INVALID_ARGUMENT` | 字段非法 / 必填缺失 / `UNSPECIFIED` 枚举 | 记错，不重试 |
| `request.too_large` | `RESOURCE_EXHAUSTED` | 非报告类消息 > 1 MiB | 记错，不重试 |
| `agent.unauthenticated` | `UNAUTHENTICATED` | 无 client cert / 证书链不受信 | 退出（配置错误，重试无意义） |
| `agent.identity_mismatch` | `PERMISSION_DENIED` | CN 查不到 / 证书指纹与库不符 | 退出 |
| `agent.machine_conflict` | `FAILED_PRECONDITION` | `machine_id` 与既有绑定不一致 | 退出，报运维 |
| `agent.not_registered` | `NOT_FOUND` | 未 Register 就调用其它 RPC | 先 Register |
| `agent.revoked` | `FAILED_PRECONDITION` | 机器被吊销 | 停上报、非 0 退出 |
| `ack.version_unknown` | `FAILED_PRECONDITION` | ack 的 `version > desired_version` | 记错，重拉配置 |
| `config.too_large` | `INVALID_ARGUMENT` | `document` > 64 KiB | 发布侧配置错误 |
| `config.invalid_document` | `INVALID_ARGUMENT` | 值域非法 / `rule_index` 不连续 / `priority` 重复 | 发布侧配置错误 |
| `report.batch_too_large` | `RESOURCE_EXHAUSTED` | 超 1000 窗口或 4 MiB | 拆批重发 |
| `report.malformed` | `INVALID_ARGUMENT` | 窗口号回退 / 窗口字段缺失 | 丢弃该批并记错（不重试同一批） |
| `report.window_too_old` | （响应字段） | 窗口号早于游标 1000 以上 | 记 `rejected_windows`，跳过 |
| `server.busy` | `RESOURCE_EXHAUSTED` | 控制面过载 | 按 `x-retry-after-ms` 退避 |
| `server.error` | `INTERNAL` | 未预期异常 | 按 §2.6 退避重试 |

### 12.3 agent 在 ack 里返回的 `error_code`

| `code` | 含义 |
| --- | --- |
| `config.stale_version` | 收到的 `version <= applied_version`（不 apply） |
| `config.hash_mismatch` | `sha256(document)` 与信封不符 |
| `config.schema_unsupported` | `schema_version` 不在 `capabilities.config_schema` |
| `config.invalid_document` | 字段越界（`window_ms` 超出、`priority` 重复、`rule_index` 不对、CIDR 非法等） |
| `config.unsupported_default_action` | `default_action != PASS`（Train 0） |
| `bpf.write_config_failed` | 写 CONFIG 或回读校验失败 |
| `bpf.attach_failed` | XDP attach/替换失败 |

`error_detail` ≤ 512 字符，只给人看。

---

## 13. 两侧落地清单

### 13.1 本分支已完成（Rust，`probe/agent/`）

| 项 | 产出 | 状态 |
| --- | --- | --- |
| 构建 | `build.rs` 用 `tonic-build` 编 proto；容器镜像加 `protobuf-compiler`（aya-dev 里已确认可装 `libprotoc 3.21.12`） | 本分支已完成 |
| 纯逻辑（L0） | `ConfigDocument` 解码 + §5.3 六步校验 + 换算 `ProbeConfig`；`content_sha256`（§6.4 两向量）；`key_hash`（§7.3 三向量）；错误码枚举 | 本分支已完成 |
| 客户端 | `ControlClient` trait + tonic 实现（mTLS）；deadline / 退避 / 单飞；`Watch` 客户端与回退 | 本分支已完成 |
| 状态机 | Register → GetCursor → GetConfig → validate → apply → Ack → 心跳/上报循环（§8） | 本分支已完成 |
| 进程 | `probe-agent` bin：CLI/env、Session 门面、SIGTERM partial 窗口 | 本分支已完成 |
| 门禁 | `probe/Cargo.toml` members / default-members；`xtask` 新增 `--layer agent`（`-p agent --tests`）并纳入 `--layer all`；`.github/workflows/ci.yml` 的 rust paths-filter 加 `agent/**` | 本分支已完成 |
| 证据 | `probe/verification/run_control_demo.sh`（真 `probe-agent` + 假控制面，10 次发布→ack）与 `probe/verification/REPORT-control-plane.md` | 本分支已完成 |

### 13.2 下一分支待做（Spring，`control-center/`）

| 项 | 产出 | 状态 |
| --- | --- | --- |
| 依赖 | `spring-grpc` starter + protobuf gradle 插件（构建期下 protoc）+ `grpc-services`（reflection，仅开发环境） | 下一分支待做 |
| 端口 | 9090 只在 `cc.role=control` 启用；TLS + `clientAuth(REQUIRE)` | 下一分支待做 |
| 存储 | `control.agent` 增 `agent_version` / `boot_id` / `applied_version` / `applied_at`（Liquibase 变更集）；`config_version.document` 用 `bytea` 存**编码后的字节**；`report` upsert 用 `(agent_id, window_id, key_hash)` | 下一分支待做 |
| 接口 | 6 个 RPC 实现，`Watch` 先返回 `UNIMPLEMENTED`；`desired_version = max(version) where group_id = ?` | 下一分支待做 |
| 校验 | 发布时把 `ConfigDocument` 编码一次、算 `content_sha256`、原样落库；**读回不重算**（存的是字节，不存在 jsonb 键序漂移） | 下一分支待做 |
| 观测 | `received_at` / `applied_at` 双时间落库，G1 取前者；Micrometer gRPC metrics | 下一分支待做 |

### 13.3 部署

| 项 | 状态 |
| --- | --- |
| `deploy/k3d/k8s/70-netpol.yaml` 新增 `allow-control-grpc-from-agents`（control 的 9090 ingress；8080 的 `allow-control-from-mgmt-only` 未放宽） | 本分支已完成 |
| `compose.yaml` 的 cc-control 发布 `127.0.0.1:${CC_CONTROL_GRPC_PORT:-9090}`；`deploy/k3d/k8s/40-control.yaml` 的 Service/容器加 `grpc/9090` | 本分支已完成 |
| readiness：`grpc-services` 自带的 gRPC health，或沿用现有 HTTP health | 下一分支待做（Train 0 沿用 HTTP，少一处变更） |

端口最初是**预留**；截至 2026-11-18，`control` 角色已在 9090 提供 `control.agent.v1.AgentControl`
（六个方法，`Watch` 仍 `UNIMPLEMENTED`，mTLS `clientAuth=REQUIRE`），见 `control-center/README.md` §6。
两端尚未端到端接线（E8），逐项状态以 `docs/STATUS.md` 为准。

---

## 14. 决策记录：为什么是 gRPC 而不是 REST

首版契约（同一天早些时候）写成 REST + JSON，随后改判 gRPC。前提是：Spring 侧尚未开始实现，改传输没有返工成本。

**收益（按对本项目的价值排序）**

1. **删掉规范化**。REST 版需要两侧各实现 RFC 8785 子集来算 `content_sha256`，而 Java 侧没有成熟实现——这是整份契约里最容易错、错了最难查的一块。现在字节由控制面单方产生并原样落库，agent 只对收到的字节求哈希（§6.4）。
2. **推送**。REST 版只能靠 1s 心跳近似 notify（偏离计划 :176）；`Watch` 一条流就位，Train 0 至少把接口和 agent 侧路径定死。
3. **一份 schema**。两侧类型由 proto 生成，字段漂移在编译期暴露；REST 版靠两份手写 DTO + 文档走查。
4. **效率**。批量约为 JSON 的 1/4；HTTP/2 多路复用让心跳/上报/推送共用一条 mTLS 连接。

**代价（都已接受）**

1. protoc 进两条构建链（Rust 编译期、Gradle 构建期）。
2. 调试从 `curl` 变 `grpcurl` + reflection；`verification/*.md` 的转录方式要跟着改。
3. HTTP 状态码语义要翻译成 gRPC status + `x-error-code`（§12）。
4. 团队要会 grpcurl 与代码生成。

**前置条件（已具备）**：Spring gRPC 已是 `spring-projects` 下的官方项目（1.0.0 GA 于 2025-12，当前 1.x），Netty server 原生支持 TLS/mTLS，不需要第三方 starter；servlet 模式虽然存在，但要求整站 HTTP/2，本契约不用它（§1.1）。

**改判条件（回退 REST）**：若接入时发现 spring-grpc 与现有 Boot 3.5 / Spring MVC 栈冲突到必须改整站 TLS，或 protoc 无法进 CI，则回退 REST。届时本文结构不变，只需把 §4/§5 换成 HTTP 绑定、§6.4 恢复规范化一节。

---

## 15. 已验证的决策（结论已落正文）

| # | 事项 | 结论 |
| --- | --- | --- |
| Q1 | 契约与 proto 是否进 `probe` 仓库版本化 | 已落地：proto 在 `probe/agent/proto/control.proto`，与 §4 逐字节相同；本 md 留外层 `docs/`；`probe/README.md` 的「控制面通信」一节给出指针 |
| Q2 | `rules` 非空：降级接受 vs 拒绝整份配置 | 降级接受 + `degraded_features:["rules"]`（§6.3）；demo 的 10 次 ack 全是这条路，`degraded=rules` |
| Q3 | `rule_index` 用 1-based（0 = 未命中） | 是（§7.2）；解码向量与 `key_hash` 向量都按 1-based 钉住 |
| Q4 | 心跳 1s/5s 由响应下发，而非写死在 agent | 是（§5.2）；实测空闲态等 4955 ms 拉到新版，落后态 934–977 ms |
| Q5 | 上报 `samples[].key_hash` | 带：agent 侧按 §7.2 算好再发（`agent/src/keyhash.rs`），控制面不再从文本重算 |
| Q6 | stub 是否实现错误分支 | 是：`agent/tests/{auth,happy,retry,watch}.rs` 覆盖 hash 不符、版本不前进、吊销、`UNAVAILABLE`/`DEADLINE_EXCEEDED`、Watch 不支持的回落 |
| Q7（新） | Train 0 是否实现 `Watch` | agent 侧与 mock 都实现（推送 5 次 5–22 ms）；真控制面仍返回 `UNIMPLEMENTED`，agent 回退轮询 |

---

## 附录 A：如何复算测试向量

`aya-dev` **镜像里默认没有 protoc**；本次是在运行中的容器里 `apt-get install -y protobuf-compiler` 装的（`libprotoc 3.21.12`）。换新容器先确认 `protoc --version`，否则下文命令会失败。把 §4 的 proto 存成 `control.proto`，再写两个 text-format 输入：

`doc_a.txt`

```
schema_version: 1
window_ms: 10000
grace_ms: 2000
src_prefix_v4: 24
src_prefix_v6: 64
default_action: DEFAULT_ACTION_PASS
```

`doc_b.txt`

```
schema_version: 1
window_ms: 10000
grace_ms: 2000
src_prefix_v4: 24
src_prefix_v6: 64
default_action: DEFAULT_ACTION_PASS
rules {
  rule_index: 1
  rule_id: "r-8080"
  priority: 10
  enabled: true
  match {
    family: FAMILY_IPV4
    protocol: L4_PROTOCOL_TCP
    src_prefix: "10.0.1.0/24"
    dst_addr: "10.0.2.5"
    dst_ports: 8080
  }
  action: ACTION_BLOCK
}
```

复算：

```
protoc --proto_path=. --encode=control.agent.v1.ConfigDocument control.proto < doc_a.txt | sha256sum
# 971d3944b510ab8ea6cbc469ccae565c9ad0a60f27801e9cad052adbe4253c05  -

protoc --proto_path=. --encode=control.agent.v1.ConfigDocument control.proto < doc_b.txt | sha256sum
# c3e4eea7f545643b966956fbea32fbbc3fb44acc019999dd5ed9552940b3d30f  -

# 看字节（63 字节那条）
protoc --proto_path=. --encode=control.agent.v1.ConfigDocument control.proto < doc_b.txt | od -An -tx1 -v | tr -d ' \n'

# 反向验证：字节能解回同一份 text-format
protoc --proto_path=. --encode=control.agent.v1.ConfigDocument control.proto < doc_b.txt \
  | protoc --proto_path=. --decode=control.agent.v1.ConfigDocument control.proto
```

`sha256sum` 输出的就是 `content_sha256`，不带 `-` 后缀；`protoc` 的编码是规范化的（同一份消息、同一字段号 → 同一字节），所以这一步可复现。

`key_hash` 的复算（§7.3 三行 hex）：

```
printf '%s' 70726f...9006 | xxd -r -p | sha256sum
```

---

## 附录 B：`ConfigDocument` 字段号登记（冻结）

| # | 字段 | 类型 | 冻结说明 |
| --- | --- | --- | --- |
| 1 | `schema_version` | `uint32` | 改变文档语义才 bump；agent 用能力列表判断 |
| 2 | `window_ms` | `uint32` | — |
| 3 | `grace_ms` | `uint32` | — |
| 4 | `src_prefix_v4` | `uint32` | — |
| 5 | `src_prefix_v6` | `uint32` | — |
| 6 | `default_action` | `DefaultAction` | `PASS=0` 被 proto3 省略（§6.2） |
| 7 | `rules` | `repeated Rule` | 顺序即 `rule_index` |

`Rule`：`1 rule_index` / `2 rule_id` / `3 priority` / `4 enabled` / `5 match` / `6 action`。
删除任何字段必须 `reserved`，且 bump `schema_version`——因为 `content_sha256` 覆盖的是字节，字段语义变化必须让旧 agent 走「schema 不支持」的拒绝路径，而不是静默按旧含义解释新字节。
