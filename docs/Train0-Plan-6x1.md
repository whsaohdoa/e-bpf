# Train 0 敏捷开发计划

> 本文只定义 **Train 0** 及其 **Sprint 1–6**，不包含其他 Train 的排期。
> 版本 v1.0 ｜ 2026-11-05 ｜ 周期 6 周（6 × 1 周）

## 0. 一页纸

| 项目 | 决策 |
| --- | --- |
| Train 0 定位 | 奠基 Train：端到端最小闭环 + 高风险技术验证，不追求完整功能 |
| 交付范围 | A 档最小闭环：登录/RBAC3/组机/审计 → 提案发布 → 通知拉取 → eBPF 五元组过滤 → 聚合上报 → Web 展示 |
| 节奏 | 6 个 Sprint × 1 周，共 6 周；最后预留 2 天缓冲 |
| 团队 | 4–5 人：PO 兼 SM、后端 2、eBPF 1、前端/QA 1 |
| 设计规模 | 100 台机器 / 20 组 / 1k 规则 / 单机 10Gbps（仅设计目标，不作验收） |
| 运行环境 | 控制面 K8s（kind/k3d）；数据面容器 + veth + generic XDP；目标内核 ≥5.15 |
| 身份 | 本地账号库；OIDC 仅预留接口 |
| 验收剧本 | 发布 8080 拦截 → ≤3s 生效 → 打流丢包 → 10s 聚合 → Web/审计可见 |
| 明确不验证 | native XDP、10Gbps、缓存回退/fail-fast、封装/分片/SAVI/泛洪可用性 |

### 0.1 Train 0 目标

用 6 周跑通一条真实可演示的最小闭环，并把最危险的数据面能力从“未知”变成“有结论”。

`组员登录 → 查看组内机器 → 提案拦截规则 → 组管理员发布 → 控制中心 PEP 鉴权并生成配置版本 → 通知 agent → agent 拉取并应用 → eBPF 丢弃 8080 → 10s 聚合与样本上报 → Web 可见 → 审计可查`

### 0.2 成功标准

| ID | 标准 | 度量 |
| --- | --- | --- |
| G1 | 发布到生效时延 | 发布 API 200 → agent apply ack，P95 ≤3s（10 次） |
| G2 | 聚合正确性 | 10s 窗口 count 与发送包数偏差 ≤1%；first_seen/last_seen 落在窗口内；每键 1 条样本 |
| G3 | 可见性 | 窗口结束后 ≤5s，Web/API 可查聚合记录、拦截原因、规则 ID |
| G4 | 权限正确性 | 6 个负向用例 100% 拒绝；除网络层直连外均产生审计事件 |
| G5 | 风险收敛 | 4 份设计/spike 结论通过评审，写明可行/不可行/前置条件 |

6 个负向用例：组员发布、组员直连控制中心、组管理员发布自己的提案、跨组访问、过期 token、重放 jti。

### 0.3 非目标

- 不验证 native XDP、网卡驱动兼容性、10Gbps 性能。
- 不实现配置缓存回退、无缓存 fail-fast、跨版本兼容、配置回滚。
- 不做控制中心独立 UI；管理员只走 CLI。
- 不实现 Geneve/GRE/IPsec 内层匹配、分片状态机、SAVI、泛洪防护的生产能力。
- 不做 HA、审计 WORM/长期留存、域间策略执行、非本机收包 MAC 重写转发。

## 1. 范围与优先级

### 1.1 Must：Train 0 必须交付

| Epic | 内容 | 验收信号 |
| --- | --- | --- |
| E1 身份与 RBAC3 | 本地登录；全局 platform_admin/auditor；组内 group_admin/member；用户多组；机器唯一组；SoD | 权限矩阵测试全绿 |
| E2 组与机器 | 组、机器元数据；组内只读视图；机器状态来自控制中心 | 组员仅见本组机器 |
| E3 提案与发布 | draft → proposed → published；提案人 ≠ 发布人；break-glass | 负向用例全部拒绝 |
| E4 配置版本与下发 | 全局单调版本（int + sha256）；发布 → 通知 → 拉取 → apply ack | 发布到 ack P95 ≤3s |
| E5 PEP | 验签、aud/exp/jti、资源级鉴权、拒绝审计 | 越权 403 且可审计 |
| E6 数据面过滤 | IPv4/IPv6、TCP/UDP/ICMPv4/v6、五元组、默认拒绝未识别、分片/封装安全丢弃 | 打流结果符合预期 |
| E7 聚合与样本 | 10s 窗口、聚合键、count/first/last、1 样本/键、ringbuf、sample_dropped | 计数偏差 ≤1% |
| E8 上报 | 增量游标、幂等键、拦截独立队列、背压时先降级放行样本 | 重复上报不重计 |
| E9 Web | 登录、机器列表/详情、规则提案、流量统计、审计简版 | 基线剧本可在 Web 完成（发布除外） |
| E10 环境与设计 | kind/k3d、agent 容器、veth、造流、CI；4 份设计 | 一键起环境；设计评审通过 |

### 1.2 Should（容量不足时按序砍）

1. Web 审计筛选、导出、分页优化。
2. IPv6 与 ICMP 的分协议统计。
3. 规则命中 TopN、时间范围选择。
4. 审计详情字段（UA/IP/request_id 展示）。
5. 通知失败的可视化告警。

> 若最终只有 4 人（无独立前端/QA），Sprint 1 即砍 Should 1–2，Web 缩为“机器 + 规则提案”两页。

### 1.3 Out

- native XDP、驱动兼容性、10Gbps 压测。
- 配置本地缓存、fail-fast、跨版本兼容、配置回滚。
- 控制中心独立 UI、OIDC/LDAP 实现。
- 封装内层匹配、分片状态机、SAVI、泛洪防护的生产实现。
- 域间策略执行（仅预留 schema）、非本机收包 MAC 重写转发。
- HA/多实例、审计 WORM、长期留存、多租户配额。
- 机器跨组迁移流程（Train 0 只保证“一台机器一个组”的约束）。

### 1.4 设计目标 vs 验收目标

| 维度 | 设计目标 | Train 0 验收 |
| --- | --- | --- |
| 机器数 | 100 | 1 台主机器 + 1 台跨组机器 |
| 组数 | 20 | 2 |
| 规则数 | 控制面 1k；数据面线性匹配，后续优化 | 1 条拦截 + 少量放行规则 |
| 吞吐 | 单机 10Gbps | veth 功能验证，不做吞吐结论 |
| XDP 模式 | native | generic |
| 配置版本 | 全局单调递增 | 同左 |

### 1.5 需求覆盖状态

| 需求 | Train 0 状态 | 说明 |
| --- | --- | --- |
| 管理层用户/RBAC/审计库 | 实现 | 本地账号；RBAC3；审计只追加 |
| 组员只读 + 提案，组管理员发布 | 实现 | 提案人 ≠ 发布人；break-glass 强审计 |
| 每台机器只属于一个组 | 实现 | 机器主键唯一 + 单组外键 |
| 管理层通过 API 查询控制中心 | 实现 | 服务身份 + 短 JWT |
| 控制中心两类主体 + 资源级 PEP | 实现 | human_admin / service_proxy；agent 设备通道除外 |
| 规则尽快同步到指定机器 | 实现 | notify + pull + heartbeat 对账，P95 ≤3s |
| 头部可识别攻击过滤 | 部分 | 五元组与协议字段；深度头部攻击规则不在 Train 0 |
| ICMP/SYN/UDP 泛洪防护 | 设计 | spike 结论，不实现限速 |
| 源地址校验（SAVI） | 设计 | spike 结论，不实现 |
| 五元组记录 + 聚合 + 样本 | 实现 | 10s 窗口；1 样本/键；分片/封装仅安全丢弃 |
| 启动拉取 + 缓存 + fail-fast | 部分 | 拉取 + 版本号；缓存/fail-fast 不在 Train 0 |
| 配置更新通知 | 实现 | notify + heartbeat 对账 |
| IPv4/IPv6/ICMP + 默认拒绝 | 实现 | 未识别协议/扩展头/封装默认丢 |
| 拦截优先上报 | 实现 | 双 ringbuf + 双队列；背压降级放行样本 |
| 域间策略 | 不在范围 | schema 预留，不执行 |
| 增量上报 + 断点续传 | 部分 | 内存游标 + 幂等；进程重启续传不保证 |
| 非本机转发 / MAC 重写 | 不在范围 | Train 0 仅 ingress |

## 2. 关键设计决策

### 2.1 主体、认证与信任边界

控制中心只接受两类**用户/服务请求主体**：管理员人工主体、管理层服务身份代理的组员请求。agent 是设备主体，走独立 mTLS 通道，不计入这两类。

| 主体 | 认证方式 | 凭证 | 访问路径 | 说明 |
| --- | --- | --- | --- | --- |
| 组员 / 组管理员 | 管理 Web 本地密码 + HttpOnly 会话 | 浏览器仅持会话；服务端另签 5 分钟 JWT | 浏览器 → 管理 BFF → 控制中心 | control token 不下发浏览器 |
| 平台管理员 | 管理面 CLI 登录 | `typ=human_admin` 的 5 分钟 JWT | CLI → 控制中心 | 唯一可直连控制中心的用户主体 |
| 审计员 | 管理面登录 | 只读会话 | 管理 Web/API | 不能修改任何配置 |
| Agent | mTLS 设备证书 | 证书 CN=agent_id | Agent → 控制中心 | 只能访问自身配置与上报接口 |

```text
组员浏览器 ──HTTPS──▶ 管理 Web/BFF ──mTLS + JWT(5m)──▶ 控制中心 PEP ──▶ 配置/状态库
管理员 CLI ──登录管理面──▶ JWT(human_admin) ──mTLS──▶ 控制中心 PEP
Agent ──mTLS(设备证书)──▶ 控制中心：注册 / 心跳 / 拉配置 / 上报
```

硬规则：

- 管理面只做认证与粗授权（角色、组）；控制中心做资源级最终鉴权（机器归属、规则归属、动作）。
- 控制中心不信任请求体里的 `group_id`，一律从 token + DB 解析。
- JWT 使用 ES256，`aud=control`，有效期 ≤300s，`jti` 一次性；Train 0 用单实例内存防重放。
- 组员 token 同时含 `sub`（用户）与 `act`（管理服务），审计两者都记录。
- 管理面到控制中心、agent 到控制中心均 mTLS；浏览器无法直接解析控制中心地址。

### 2.2 RBAC3 角色、继承与约束

| 角色 | 作用域 | 继承 | 关键权限 |
| --- | --- | --- | --- |
| platform_admin | 全局 | group_admin（所有组） | 管理组/机器；break-glass 发布；全量审计 |
| auditor | 全局 | —（只读） | 全量审计；只读机器/流量 |
| group_admin | 组内 | member | 组内只读；提案；发布非本人提案 |
| member | 组内 | — | 组内只读；创建提案 |

约束：

- C1：每台机器恰好属于 1 个组（`machine.group_id NOT NULL`；机器主键唯一；不存在机器-组多对多表）。
- C2：用户可属多组，每组只能有 1 个角色。
- C3：提案人 ≠ 发布人；platform_admin 可 break-glass，但必须填写原因并产生高优先级审计。
- C4：member 不能发布、不能改规则、不能改机器。
- C5：审计只追加，不提供 UPDATE/DELETE API。
- C6：control token 的 `aud` 必须是 `control`，过期即拒绝，`jti` 重放即拒绝。

| 动作 | member | group_admin | platform_admin | auditor |
| --- | --- | --- | --- | --- |
| 查看组内机器/流量/规则 | ✓ | ✓ | ✓ | ✓ |
| 创建提案 | ✓ | ✓ | ✓ | ✗ |
| 发布提案 | ✗ | ✓（非本人） | ✓（break-glass 需原因） | ✗ |
| 管理组成员 | ✗ | ✗（Train 0） | ✓ | ✗ |
| 查看审计 | 仅本人操作 | 组内 | 全量 | 全量 |
| 直连控制中心 | ✗ | ✗ | ✓（CLI） | ✗ |

> 组管理员在 Train 0 不管理组成员；成员变更由 platform_admin 执行，避免在 6 周内引入邀请、撤销、所有权转移等额外流程。

### 2.3 配置下发链路

1. group_admin 发布 → 管理面校验 SoD → 控制中心 `POST /internal/v1/proposals/{id}/publish`。
2. 控制中心 PEP 鉴权 → 写入新版本 `vN`（全局单调 int + `sha256` + `schema_version`）→ 写 outbox。
3. 控制中心按规则目标（组或机器）计算受影响机器，逐台 `POST /agent/v1/notify {version}`（500ms 超时，3 次重试）。
4. agent 收到通知 → `GET /agent/v1/config?current_version=vN-1` → 校验版本、hash、schema → 应用到 eBPF → `POST /agent/v1/ack`。
5. 控制中心记录 `applied_at`，Web 显示机器配置版本与最后应用时间。
6. 若通知丢失：agent 每 10s 心跳，响应携带 `desired_version`；发现落后即主动拉取。

版本与幂等：

- agent 拒绝 `version <= current`（409）与 hash 不匹配（422）。
- 发布操作幂等：同一提案重复发布返回同一版本，不重复递增。
- 配置为全量快照；Train 0 不做增量配置包。

Train 0 的临时行为（正式语义不在本期验收）：

- 无本地缓存；启动拉取失败按 2s / 5s / 10s 退避重试，退避上限 30s。
- 配置未加载时数据面 **fail-closed 全丢**，进程不退出；这是临时安全兜底，不等于正式的 fail-fast。
- 不做跨版本兼容承诺；控制中心与 agent 必须同版本部署。

### 2.4 数据面 Train 0 行为

| 流量 | 行为 | 原因码 |
| --- | --- | --- |
| IPv4/IPv6 TCP/UDP | 五元组规则匹配；命中 block 丢弃，否则放行并计数 | `rule_id` / `default` |
| ICMPv4/ICMPv6 | 按协议/源/目的匹配；无规则则放行并计数 | `rule_id` / `default` |
| ARP/LLDP | 放行，不计入 IP 流量 | `l2_pass` |
| 分片（MF 或 offset > 0） | 丢弃 | `fragment_unsupported` |
| VLAN | 丢弃 | `vlan_unsupported` |
| Geneve/VXLAN/GRE/IPsec | 丢弃 | `encap_unsupported` |
| IPv6 扩展头 | 丢弃 | `ipv6_ext_unsupported` |
| 未知 EtherType / IP protocol | 丢弃 | `unsupported_proto` |
| 无配置 | 全丢 | `no_config` |

说明：

- Train 0 只做 ingress、单网口、generic XDP；不做非本机转发。
- 分片一律丢弃是 fail-closed 的临时策略，避免“首片放行、后续片绕过”的漏洞；分片状态机由 spike 定稿。
- 封装流量一律丢弃，避免只匹配外层头造成“看似有策略、实际可绕过”；内层匹配由 spike 定稿。
- 规则匹配用线性数组，按优先级 first-match；Train 0 验收 ≤20 条规则。

关键 map：

| Map | 类型 | Key | Value | 说明 |
| --- | --- | --- | --- | --- |
| `config_map` | array | index | version / window_ns / default_action | 单条配置 |
| `rules` | array | index | match/action/rule_id/priority | ≤20 条 |
| `agg_map` | per-CPU hash | 聚合键 + window_id | count/first/last/sample_dropped/degraded | 131072 上限 |
| `rb_block` | ringbuf | — | 拦截样本 | 1MB，优先 |
| `rb_pass` | ringbuf | — | 放行样本 | 256KB，可降级 |
| `stats` | per-CPU array | index | 计数器 | 溢出/丢样/版本 |

聚合键：

```text
(window_id, direction, src_prefix, dst_addr, dst_port, proto, action, rule_id/reason)
```

- `direction` 在 Train 0 固定为 `ingress`。
- 源地址聚合前缀默认 IPv4 `/24`、IPv6 `/64`，由配置下发。
- ICMP 的 `dst_port` 固定为 0，用 protocol 区分。
- `window_id = bpf_ktime_get_ns() / window_ns`，默认 10s；用户态用 boot time 换算墙钟时间。

### 2.5 聚合、样本与上报

- eBPF 每个 CPU 维护 `agg_map`，同一个 `(window_id, key)` 只累加 count，更新 first/last。
- 用户态每秒扫描已关闭窗口，按 key 合并、删除，生成聚合记录。
- 窗口关闭后保留 2s grace 再合并，避免边界包被拆成两条；grace 之后的迟到包计入 `late_dropped`，不重开窗口。
- 每个 key 每窗口最多 1 条完整五元组样本；eBPF 只在首包尝试写 ringbuf，用户态按 key 去重。
- ringbuf 满时置 `sample_dropped=1`，聚合 count 不受影响。
- 拦截样本走 `rb_block`，放行样本走 `rb_pass`；上报端先排空 block，背压时先丢 pass 样本，并置 `degraded=1`。
- 聚合记录字段：`window_start`、`window_end`、`count`、`first_seen`、`last_seen`、`sample_dropped`、`degraded`。

上报：

- agent 每秒检查；发现新关闭窗口立即批量 `POST /agent/v1/reports`，空闲时每 5s 发一次心跳；启动或重连后先取 `GET /agent/v1/reports/cursor`，补发该窗口之后的记录。
- 控制中心以 `(agent_id, window_id, key_hash)` 为幂等键 upsert，重复上报不重复计数。
- 游标只存内存；进程重启后的续传不在 Train 0 验收范围，但重复上报必须安全。

```json
{
  "agent_id": "agent-01",
  "batch_id": "1f5c...",
  "window": {"id": 1234567, "start": "2026-11-20T10:00:00Z", "end": "2026-11-20T10:00:10Z"},
  "aggregates": [{
    "key_hash": "9a2f...",
    "direction": "ingress",
    "src_prefix": "10.0.1.0/24",
    "dst_addr": "10.0.2.5",
    "dst_port": 8080,
    "proto": "tcp",
    "action": "block",
    "rule_id": "r-8080",
    "reason": null,
    "count": 100,
    "first_seen": "2026-11-20T10:00:02.113Z",
    "last_seen": "2026-11-20T10:00:09.887Z",
    "sample_dropped": false,
    "degraded": false
  }],
  "samples": [{"five_tuple": "10.0.1.7:53124 -> 10.0.2.5:8080 tcp", "ts": "2026-11-20T10:00:02.113Z"}]
}
```

### 2.6 Web 与 CLI

| 页面 | 内容 | 权限 |
| --- | --- | --- |
| `/machines` | 组内机器、在线状态、配置版本、24h 拦截/放行计数 | 组内只读 |
| `/machines/:id` | 网口、最后应用时间、Top 拦截规则、最近聚合 | 组内只读 |
| `/rules` | 规则状态、命中数、提案表单、发布按钮 | 组员可提案；组管理员可发布非本人提案 |
| `/audit` | 时间/动作/主体/结果筛选 | 组内；platform_admin/auditor 全量 |

CLI（管理员）：

```text
fwctl login --management https://mgmt.example
fwctl config publish --proposal <id>
fwctl config rollback --to <version>      # 仅 break-glass，Train 0 可返回 501
fwctl machine list --group g1
fwctl audit tail --since 1h
fwctl break-glass publish --proposal <id> --reason "P1 处置"
```

### 2.7 审计

事件类型：

- `auth.login.success` / `auth.login.failure`
- `rbac.denied`
- `proposal.create` / `proposal.publish` / `proposal.self_publish_denied`
- `break_glass.use`
- `config.pull` / `config.apply` / `config.apply_failed`
- `pep.deny`
- `agent.register` / `agent.offline`

字段：`id, ts, actor_id, actor_type, act_as, group_id, action, resource_type, resource_id, result, reason, request_id, ip, ua`。

- 管理库存权威审计；控制中心产生的事件先写本地 outbox，再批量转发 `POST /internal/v1/audit/events`。
- 审计表只追加；DB 层禁止 UPDATE/DELETE（触发器或最小权限账号）。
- 所有请求带 `request_id`，可从 Web 一次点击串到 agent 日志。

### 2.8 接口边界

| 侧 | 方法/路径 | 用途 |
| --- | --- | --- |
| 管理面 | `POST /api/auth/login` | 本地登录 |
| 管理面 | `GET /api/machines` / `GET /api/machines/{id}` | 组内机器与状态 |
| 管理面 | `GET /api/machines/{id}/traffic` | 流量与拦截聚合 |
| 管理面 | `POST /api/proposals` | 创建提案 |
| 管理面 | `POST /api/proposals/{id}/publish` | 发布（校验 SoD） |
| 管理面 | `GET /api/audit` | 审计查询 |
| 管理→控制 | `GET /internal/v1/machines` | 机器清单（服务身份） |
| 管理→控制 | `POST /internal/v1/proposals/{id}/publish` | 发布到控制面 |
| 管理→控制 | `GET /internal/v1/traffic` | 聚合查询 |
| 管理→控制 | `POST /internal/v1/audit/events` | 审计转发 |
| Agent | `POST /agent/v1/register` | 一次性引导令牌 + CSR |
| Agent | `POST /agent/v1/heartbeat` | 心跳，返回 `desired_version` |
| Agent | `GET /agent/v1/config` | 拉取全量配置 |
| Agent | `POST /agent/v1/ack` | 应用结果 |
| Agent | `POST /agent/v1/notify` | 控制中心 → agent 的通知回调 |
| Agent | `POST /agent/v1/reports` | 聚合与样本上报 |
| Agent | `GET /agent/v1/reports/cursor` | 断点游标 |

## 3. 团队、容量与节奏

### 3.1 团队

| 角色 | 人数 | 主要职责 | 关键产出 |
| --- | --- | --- | --- |
| PO 兼 SM | 1 | 范围、优先级、验收、演示、风险管理 | Sprint 目标、验收剧本、砍单决策 |
| 后端 A | 1 | 管理面：认证、RBAC3、组/机器、提案发布、审计、BFF | 管理 API、权限矩阵、审计链 |
| 后端 B | 1 | 控制面：配置版本、通知、PEP、上报 ingest；agent 用户态 | 控制 API、agent、上报链路 |
| eBPF | 1 | XDP 解析、规则匹配、聚合 map、样本 ringbuf、4 项 spike | eBPF 程序、设计结论 |
| 前端/QA | 1 | Web 4 页、E2E、环境、造流、安全用例 | Web、E2E 脚本、演示环境 |

### 3.2 容量

- 4 名交付人力 × 5 人日 = 20 人日/Sprint。
- 扣除每日站会、计划、评审、回顾、支持约 2 人日，可用约 **18 人日/Sprint**。
- 6 个 Sprint 承诺合计 95.5 人日；Sprint 6 仅承诺 10 人日，预留 8 人日吸收溢出与彩排。
- 估算为净开发+自测，不含代码评审与跨人联调；跨人项按主责计。

### 3.3 仪式

| 仪式 | 时间 | 时长 |
| --- | --- | --- |
| Sprint Planning | 每周一 | 1h |
| Daily | 每日 10:00 | 15min |
| Backlog Refinement | 每周三 | 30min |
| Sprint Review | 每周五 | 1h |
| Retro | 每周五 | 45min |
| 风险/依赖同步 | 每周五 | 15min |

### 3.4 建议日历

| 周 | 日期 | 里程碑 |
| --- | --- | --- |
| W1 | 2026-11-09 ~ 11-13 | Sprint 1：契约、环境、模型、登录、解析骨架；Review 11-13 |
| W2 | 2026-11-16 ~ 11-20 | Sprint 2：RBAC、提案发布、配置版本/通知、规则匹配；Review 11-20 |
| W3 | 2026-11-23 ~ 11-27 | Sprint 3：agent、聚合 map、Web 骨架、端到端丢包；Review 11-27 |
| W4 | 2026-11-30 ~ 12-04 | Sprint 4：PEP、代理、SoD、样本、窗口聚合器；Review 12-04 |
| W5 | 2026-12-07 ~ 12-11 | Sprint 5：上报、ingest/查询、Web 统计、2 份 spike；Review 12-11 |
| W6 | 2026-12-14 ~ 12-18 | Sprint 6：剩余 spike、安全 E2E、彩排×3、文档、Demo 12-18 |
| 缓冲 | 2026-12-21 ~ 12-22 | 缺陷修复、限制清单签字 |

> 若启动日不是 2026-11-09，整体平移，Sprint 长度不变。

## 4. Sprint 1（W1）：契约、骨架与环境

### 4.1 Sprint 目标

**把地基钉死：契约、数据模型、环境、登录与解析骨架在第 1 周一次到位。**

### 4.2 Sprint 1 Backlog

| ID | 交付项 | 主责 | 人日 | 完成定义 |
| --- | --- | --- | --- | --- |
| S1-0 | 架构工作坊与契约冻结：API、配置 schema、聚合键、错误码、演示拓扑、威胁模型 | 全员 | 1.5 | 契约文档签字；变更需 PO 批准 |
| S1-1 | 领域模型与迁移：user/group/membership/machine/rule/proposal/config_version/audit | 后端 A | 3 | 迁移可重复；机器唯一组约束生效 |
| S1-2 | 本地登录、会话、ES256 JWT（5min、aud=control、jti）、OIDC 接口占位 | 后端 A | 3 | 登录/登出/过期/CSRF 测试通过 |
| S1-10 | 环境：kind/k3d、特权 agent 容器、veth pair、造流、Makefile | 前端/QA | 3 | `make up` 一条命令起环境 |
| S1-8a | eBPF 解析骨架：Ethernet/IPv4/IPv6/TCP/UDP/ICMP 头解析、边界与长度校验 | eBPF | 4 | verifier 通过；pcap 单测覆盖 4 类协议 |
| S1-12 | CI：fmt/lint/单测/集成/eBPF 编译/镜像构建 | 前端/QA | 1.5 | main 绿；流水线 <10min |

合计 16 人日。

### 4.3 Sprint 1 演示

1. `make up` 一条命令启动 kind/k3d、Postgres 与管理/控制空服务。
2. 种子用户登录，检查 JWT 的 `aud=control`、`exp≤300s`、`jti`。
3. 迁移 up/down；机器唯一组约束拒绝第二条组关系。
4. eBPF 解析器对 IPv4/IPv6/TCP/UDP/ICMP 样本通过；verifier 通过。
5. CI 绿。

### 4.4 Sprint 1 退出标准

- 契约文档签字；环境可复现；登录/JWT 可用；解析骨架覆盖 4 类协议。
- 0 个 P0；P1 有 workaround。

### 4.5 Sprint 1 依赖与风险

- S1-0 必须在前 2 天完成，否则后续接口会各自定义。
- kind 内特权容器第 3 天前验证；失败切 docker compose 跑 agent。
- S1-8a 是 W2 规则匹配的硬前置；若第 4 天完成度 <70%，立即砍 S1-11b 与 Should，只保 IPv4 TCP/UDP。

## 5. Sprint 2（W2）：管理面与控制面主链路

### 5.1 Sprint 目标

**让提案、发布、版本、通知在 API 层跑通；agent 未接入也能用 mock 验证。**

### 5.2 Sprint 2 Backlog

| ID | 交付项 | 主责 | 人日 | 完成定义 |
| --- | --- | --- | --- | --- |
| S1-3 | RBAC3 策略引擎：继承、SoD、多组、资源归属 | 后端 A | 3 | 12 个策略单测覆盖权限矩阵 |
| S1-4 | 组/用户/机器读 API；机器列表来自控制中心内部读接口 | 后端 A | 2 | member 查不到他组机器 |
| S1-5 | 提案/发布 API；proposer ≠ actor；版本 int+hash；outbox | 后端 A | 3 | 重复发布幂等；自发布 403 |
| S1-6 | 控制面：版本存储、agent 注册表、notify、pull、内部机器读接口 | 后端 B | 5 | 通知 3 次重试；旧版本 409；列表可分页 |
| S1-8b | eBPF 规则数组匹配、优先级、默认策略、pass/drop 计数 | eBPF | 4 | 规则命中/不命中/默认拒绝用例通过 |

合计 17 人日。

### 5.3 Sprint 2 演示

1. 12 个 RBAC 策略单测通过。
2. `bob` 提案，`alice` 发布；`bob` 自发布返回 403。
3. 控制中心生成 v1；mock agent 收到 notify 后拉取；旧版本返回 409。
4. eBPF 规则匹配单测：IPv4/IPv6/ICMP 命中、不命中、默认拒绝；计数正确。
5. 审计出现 proposal.create、proposal.publish、config.pull。

### 5.4 Sprint 2 退出标准

- 权限矩阵与 SoD 通过；版本单调；通知 3 次重试；eBPF 规则匹配单测通过。
- 0 个 P0；P1 有 workaround。

### 5.5 Sprint 2 依赖与风险

- S1-5 → S1-6 → S1-7（W3）；S1-8b → S1-9（W3）。
- S1-6 是 W3 的硬前置；若第 4 天未完成，后端 A 支援，S1-4 顺延到 W3。
- W2 为 17/18 人日，无多余缓冲；周内不再接新需求。

## 6. Sprint 3（W3）：端到端丢包

### 6.1 Sprint 目标

**达到第一个可演示闭环：一条规则从 Web 到 veth 真实丢包。**

### 6.2 Sprint 3 Backlog

| ID | 交付项 | 主责 | 人日 | 完成定义 |
| --- | --- | --- | --- | --- |
| S1-7 | Agent：注册/心跳（返回 desired_version）、启动拉取、notify、校验、应用、ack、日志/指标 | 后端 B | 5 | 重启恢复；落后自动拉取；ack 含 applied_at |
| S1-9 | eBPF：per-CPU 聚合 map，key 含 window_id，count/first/last/overflow | eBPF | 4 | 10s 窗口滚动；计数误差 ≤1% |
| S1-11a | Web：登录、机器列表/详情 | 前端/QA | 3 | 组内可见；无发布权限 |
| S1-11b | Web：规则列表/提案表单；发布按钮按角色隐藏 | 前端/QA | 2 | 提案成功；按钮权限正确 |
| S1-13 | 审计基础：登录、提案、发布、拉取、应用；request_id；列表 API | 后端 B | 1.5 | 事件可按 request_id 串联 |
| S1-14 | 种子数据与演示脚本：2 组、2 机器、4 用户、1 规则；脚本幂等 | 前端/QA | 2 | `make seed`；演示可重复 |

合计 17.5 人日。

### 6.3 Sprint 3 演示

1. `make up && make seed`。
2. `bob` 登录 → 机器列表只有 `m1`，看不到 `m2`。
3. `bob` 提交“丢弃 m1 的 8080”提案。
4. `bob` 调发布 API → 403。
5. `alice` 在 Web 发布（按钮仅组管理员可见）→ 控制中心生成 v1 → agent 通知/拉取/ack。
6. 观测发布到 ack 的耗时（目标 P95 ≤3s）。
7. 流量容器访问 `m1:8080` 超时；访问 `m1:9090` 成功。
8. 审计列表出现 proposal.create、proposal.publish、config.pull、config.apply。

### 6.4 Sprint 3 退出标准

- 端到端丢包 10/10 成功。
- 发布到 apply ack P95 ≤3s（单机、单规则、无并发发布）。
- 配置版本单调；旧版本被拒绝；agent 落后可由心跳纠正。
- 四类审计事件齐全，request_id 可串联。
- 0 个 P0；P1 ≤2 且有 workaround。

### 6.5 Sprint 3 依赖与风险

- S1-7 依赖 S1-6；S1-9 依赖 S1-8b；Web 依赖 S1-4/S1-5。
- W3 是第一个“可演示闭环”节点，不允许用 mock 替代真实 agent。
- 若 S1-7 延期，W4 的 PEP 仍可并行，但 W6 验收风险上升；S1-14 可顺延到 W4，但 W4 无余量，需同步砍 Should。

## 7. Sprint 4（W4）：授权与样本

### 7.1 Sprint 目标

**把“谁可以做什么”落实到 PEP，把“拦截优先”落实到双 ringbuf。**

### 7.2 Sprint 4 Backlog

| ID | 交付项 | 主责 | 人日 | 完成定义 |
| --- | --- | --- | --- | --- |
| S2-1 | PEP 中间件：验签、aud/exp/jti、主体类型、资源级鉴权、403+审计 | 后端 B | 4 | PEP 相关负向用例全部拒绝；直连由 NetworkPolicy 验证 |
| S2-2 | 管理面代理与服务身份：mTLS、token 不下发浏览器、sub/act、超时重试 | 后端 A | 3 | 组员无法直连；链路可 trace |
| S2-3 | SoD 完整流程 + break-glass：原因 ≥10 字、审计标记、审计员告警 | 后端 A | 3 | 自发布 403；break-glass 可查 |
| S2-4 | eBPF 样本与优先级：rb_block/rb_pass、首包样本、sample_dropped、key 含 rule/reason | eBPF | 4 | 1 样本/键；ringbuf 满计数不丢 |
| S2-5a | 窗口聚合器：按 window_id 扫描/合并/删除、2s grace、late_dropped | 后端 B | 3 | 单测覆盖边界包与迟到包 |

合计 17 人日。

### 7.3 Sprint 4 演示

1. 6 个负向用例在 API 层全部拒绝：组员发布、组员直连、组管理员自发、跨组、过期 token、重放 jti。
2. break-glass 必须填原因；审计标记 `break_glass.use`。
3. 样本：block/pass 双 ringbuf；填满 ringbuf 后 `sample_dropped=1`，聚合计数不变。
4. 窗口聚合器单测：扫描/合并/删除、2s grace、late_dropped。

### 7.4 Sprint 4 退出标准

- PEP 负向用例全过；拒绝事件可审计。
- 双 ringbuf 与 sample_dropped 行为符合预期。
- 窗口聚合器单测通过；0 个 P0。

### 7.5 Sprint 4 依赖与风险

- S2-1 依赖 S1-2/S1-3；S2-2 依赖 S1-6；S2-4 依赖 S1-9；S2-5a 依赖 S1-9。
- 若 S2-1 延期，W5 的上报仍可推进，但最终验收会被阻塞；W6 的 8 人日余量用于兜底。
- W4 为 17/18 人日，周内不接新需求。

## 8. Sprint 5（W5）：聚合可见与设计收敛

### 8.1 Sprint 目标

**让 10s 聚合在 Web 可见，并把 4 个高风险项收敛到 2 份结论。**

### 8.2 Sprint 5 Backlog

| ID | 交付项 | 主责 | 人日 | 完成定义 |
| --- | --- | --- | --- | --- |
| S2-5b | 上报客户端：游标、幂等键、block 优先队列、429 降级 | 后端 B | 2 | 窗口后 ≤5s 入库；重发不重计 |
| S2-6 | 控制面 ingest + 查询：批量写入、去重、机器/组/时间窗/TopN | 后端 A | 4 | 查询 P95 <500ms（1k 记录） |
| S2-7 | Web 流量页：总览、趋势、TopN、拦截列表、样本详情 | 前端/QA | 4 | 窗口后 ≤5s 可见；与 API 一致 |
| S2-8 | Web 规则/审计页：状态、命中数、发布权限；审计筛选 | 前端/QA | 3 | 仅 group_admin 可发布 |
| S2-9 | 审计补全：PEP 拒绝、配置、break-glass；DB 只追加约束 | 后端 A | 2 | UPDATE/DELETE 被拒 |
| S2-10a | 设计/spike：Geneve/GRE/IPsec 内层匹配、分片状态机 | eBPF | 3 | 2 份结论评审通过 |

合计 18 人日。

### 8.3 Sprint 5 演示

1. 10s 聚合在 Web 可见：count、first/last、1 条样本、action=block；窗口结束后 ≤5s。
2. 审计筛选、规则命中数、发布权限正确。
3. 2 份设计结论：封装内层、分片，写明可行/不可行/前置条件。
4. 模拟 429：block 聚合仍送达，pass 样本降级，`degraded=1`。

### 8.4 Sprint 5 退出标准

- G2/G3 达标；审计与权限页面可用；2 份设计评审通过。
- 0 个 P0；P1 有 workaround。

### 8.5 Sprint 5 依赖与风险

- S2-5b 依赖 S2-5a/S2-6；S2-7 依赖 S2-6；S2-10a 可独立推进。
- W5 为 18/18 人日，满负荷；若 S2-5b 延期，W6 的 8 人日余量承接。
- 设计/spike 每项限 1.5 人日，先结论后细节。

## 9. Sprint 6（W6）：验收与移交

### 9.1 Sprint 目标

**把“能演示”变成“可重复演示、可交接”。**

### 9.2 Sprint 6 Backlog

| ID | 交付项 | 主责 | 人日 | 完成定义 |
| --- | --- | --- | --- | --- |
| S2-10b | 设计/spike：SAVI、ICMP/SYN/UDP 泛洪防护 | eBPF | 2 | 2 份结论评审通过 |
| S2-11 | 安全与 E2E：6 负向 + 基线自动化；重放/过期/跨组/直连 | 前端/QA | 3 | CI 可跑；报告归档 |
| S2-12 | 验收彩排 ×3 + 已知限制清单 + 演示手册 | PO + 全员 | 3 | 3/3 通过；限制清单签字 |
| S2-13 | 文档：部署/排障/故障处置手顺/ADR 汇总 | 后端 A | 2 | 新人 30min 起环境 |

合计 10 人日；余量 8 人日用于承接 W1–W5 溢出与缺陷修复。

### 9.3 Sprint 6 演示

- 执行第 10 节的 17 步验收剧本，连续 3 次，记录 G1–G5 实测值。
- 4 份设计结论各 5 分钟：结论、约束、验证方法、后续工作量。
- 已知限制清单与交接文档签字。

### 9.4 Sprint 6 退出标准

- G1–G5 全部达标。
- 6 个负向用例 100% 拒绝（审计事件或 NetworkPolicy 日志可查）。
- 连续 3 次验收剧本无阻断性失败。
- 4 份设计/spike 评审通过并归档。
- 0 个 P0；P1 全部有 workaround 或明确不在 Train 0 范围。

### 9.5 Sprint 6 依赖与风险

- 依赖 W1–W5 的全部交付；8 人日余量优先修 P1，不接新功能。
- 若 W5 延期超过 3 天，立即砍 Should 1–2，保验收剧本。
- Demo 固定 2026-12-18；缓冲 12-21~12-22 只做修复与签字。

## 10. 验收剧本

### 10.1 前置条件

- `make up` 启动 kind/k3d：management、control、Postgres、1 个 agent 容器（m1，g1）、1 个流量容器；可选 m2（g2）。
- 种子用户：`admin`（platform_admin）、`aud`（auditor）、`alice`（g1 group_admin）、`bob`（g1 member）、`carol`（g2 member）。
- m1 属于 g1，m2 属于 g2；每台机器只有一个组；m1 的 veth 同时配置 IPv4 与 IPv6 地址。
- 控制面与 agent 使用同一版本；时钟由宿主机同步。

### 10.2 步骤与期望

| # | 操作 | 期望 | 观测点 |
| --- | --- | --- | --- |
| 1 | `bob` 登录 | 200；会话 Cookie 为 HttpOnly | `/api/me` 返回 g1:member |
| 2 | `bob` 查机器列表 | 只返回 m1；m2 返回 404 | Web 机器页 |
| 3 | `bob` 查看 m1 流量/规则 | 只读，无发布按钮 | Web |
| 4 | `bob` 创建“block dst 8080”提案 | 201，状态 proposed | 提案 API |
| 5 | `bob` 尝试发布 | 403 `sod.proposer_cannot_publish` | 审计事件 |
| 6 | `alice` 登录；先新建一条提案并尝试发布 → 403；再发布 bob 的提案 | 403 `sod.self_publish_denied`；随后 200；config v1；通知发出 | 发布 API / 审计 |
| 7 | 测量发布到生效 | 10 次 P95 ≤3s | `publish_at` → `applied_at` |
| 8 | 向 m1:8080 发送 100 个固定 TCP SYN（raw，无重传） | 100% 丢弃；reason=rule:r-8080 | eBPF counter / 聚合 |
| 9 | 向 m1:9090 发送 100 个固定 TCP SYN（raw，无重传） | 100% 放行 | 聚合 |
| 10 | 发送 10 个 ICMP echo；发送 IPv6 TCP 9090 | 放行并计数 | 聚合 |
| 11 | 发送 GRE 或未知协议；发送分片 | 丢弃，原因分别为 `encap_unsupported`、`fragment_unsupported` | 聚合 |
| 12 | 等待窗口结束 + ≤5s | Web/API 出现聚合：count、first/last、1 条样本、action=block | 流量页/API |
| 13 | `aud` 查询审计 | 看到步骤 4/5/6/12 的事件与 request_id | 审计页 |
| 14 | `carol` 的 token 访问 m1 | 403/404；产生 `pep.deny` | 审计 |
| 15 | `bob` 直连控制中心 | 连接被 NetworkPolicy 拒绝（记录策略日志）；若放行则 401 并产生 `pep.deny` | 网络策略/审计 |
| 16 | 过期 token；同一 jti 重放 | 401；产生拒绝审计 | 审计 |
| 17 | `admin` break-glass 发布 alice 在步骤 6 被拒的那条提案 | 要求原因；审计标记 `break_glass.use`；审计员可见告警 | 审计 |

### 10.3 判定

- G1：步骤 7 的 P95 ≤3s。
- G2：步骤 8/9 的 count 与发送数偏差 ≤1%；first/last 落在窗口内；block key 恰好 1 条样本。
- G3：步骤 12 在窗口结束后 ≤5s 可见。
- G4：步骤 5/6/14/15/16 全部拒绝；其中 5/6/14/16 产生审计事件，步骤 15 若为网络层拒绝则校验 NetworkPolicy 日志。
- G5：4 份设计文档评审记录齐全。

## 11. 风险与应对

| ID | 风险 | 概率/影响 | 触发信号 | 应对 | Owner |
| --- | --- | --- | --- | --- | --- |
| R1 | eBPF 单点，解析/聚合做不完 | 高/高 | Sprint 1 末 S1-8a <70%，或 Sprint 2 末 S1-8b 未完成 | 后端 B 结对；砍 IPv6/ICMP 细分与 S1-11b；W3 只保 IPv4 TCP 8080 丢包 | eBPF |
| R2 | veth/generic 与 native 差异被误读为“可上线” | 高/中 | 演示通过但无 native 数据 | 明示“未验证 native”；输出 native 准入清单，不作验收 | PO |
| R3 | 范围蔓延（4 项 spike + 完整闭环） | 高/高 | 出现非 Must 需求 | PO 只接受进入 Should/Out；严格执行砍单顺序 | PO |
| R4 | 3s 时延不达标 | 中/高 | P95 >3s | 通知并行化、减少重试等待、预热、分段测量 | 后端 B |
| R5 | 聚合 map 溢出或键基数爆炸 | 中/中 | `agg_overflow > 0` | 131072 键上限、前缀聚合、degraded 标志与告警 | eBPF |
| R6 | RBAC3 组合爆炸 | 中/中 | 策略测试覆盖不足 | 固定 4 角色 + 6 约束；矩阵测试；不做动态角色 | 后端 A |
| R7 | kind 内特权容器/网络不通 | 中/高 | 第 1 周环境起不来 | docker compose fallback；预构建镜像；hostNetwork 备选 | 前端/QA |
| R8 | 审计异步丢失 | 中/中 | 事件缺失 | outbox + at-least-once + request_id；接受重复 | 后端 A |
| R9 | 实际只有 4 人 | 中/高 | 第 1 周确认 | 立即砍 Should 1–2；Web 缩为 2 页；W5/W6 砍审计筛选 | PO |
| R10 | 4 项 spike 挤占交付 | 中/高 | Sprint 5 第 1 天无初稿 | 每项限 1–1.5 人日，先结论后细节；不写生产代码 | eBPF |

## 12. 完成定义（DoD）

### 12.1 功能

- G1–G5 达标；验收剧本连续 3 次通过。
- 6 个负向用例全部拒绝并产生审计。
- 机器唯一组、提案人 ≠ 发布人、token 5 分钟过期与 jti 防重放均有自动化测试。

### 12.2 代码与质量

- 代码评审通过；关键路径有单测（策略、解析、聚合、幂等）。
- eBPF 程序通过 verifier；CI 覆盖 fmt、lint、单测、集成、镜像构建。
- 管理↔控制↔agent 有契约测试；错误码统一。
- 无 P0；P1 有 workaround 或明确排除。

### 12.3 安全

- 浏览器不接触 control token；内部全 mTLS；密码 Argon2id；登录限流。
- 控制中心默认拒绝未识别协议、分片、封装、无配置流量。
- 无 secret 入日志/仓库；K8s Secret 注入。
- 审计只追加；break-glass 必须留原因。

### 12.4 可观测与运维

- 指标：`config_apply_latency`、`config_version`、`drop_total`、`pass_total`、`agg_overflow`、`late_dropped`、`sample_dropped`、`report_lag`、`pep_deny_total`。
- 结构化日志带 `request_id`、`agent_id`、`config_version`。
- 一条命令起环境、灌种子、重置；演示手册可复现。

### 12.5 文档

- ADR：主体模型、RBAC3、配置下发、聚合与上报。
- 部署、排障与部署回滚手顺。
- 4 份设计/spike 结论。
- 已知限制清单（native、性能、缓存、封装、分片、SAVI、泛洪、域间策略、转发）。

## 13. 假设与待确认

### 13.1 已采用的假设

- A1：控制中心的“两类主体”指用户/服务请求；agent 走独立 mTLS 设备通道。
- A2：管理员在管理面认证，CLI 携带 control-scoped token；控制中心不建独立用户库。
- A3：Train 0 仅 ingress；非本机转发与 MAC 重写不在范围。
- A4：分片与封装流量在 Train 0 默认丢弃，不尝试解析。
- A5：未加载配置时 fail-closed 全丢；正式 fail-fast 语义不在 Train 0。
- A6：组管理员在 Train 0 不管理组成员，成员变更由 platform_admin 执行。
- A7：运行环境为 kind/k3d + 特权 agent 容器，不验证 native。
- A8：建议 2026-11-09 启动；若不同，整体平移。

### 13.2 待确认（不阻塞 Sprint 1 启动）

- Q1：组管理员是否需要在 Train 0 管理组成员？当前不做。
- Q2：审计是否需要 hash 链/WORM？当前只做只追加 + 权限约束。
- Q3：break-glass 是否需要站外通知审计员？当前只做审计标记 + Web 告警。
