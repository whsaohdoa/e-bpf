# ADR-0001 以 machine 为配置主体，agent 为受管 daemon

- 状态：已接受
- 日期：2026-11-18（回填；原始决策见计划 v1.0 2026-11-05 与契约 2026-11-14）
- 决策者：Train0 团队
- 影响面：契约（proto/身份）、控制面数据模型、agent 实现、RBAC
## 背景（为什么现在必须有决定）

控制面同时面对人和设备两类主体。如果把"agent 进程"当配置主体，进程重启就换主体，
配置、审计、聚合游标全部失去连续性；如果把"组"当主体，又无法表达单机差异。
计划 §2.1 已把 agent 定义为设备主体、用独立 mTLS 与人身份域分离，契约与 proto 也已按
machine 落字段，现在必须把口径固定下来，否则后面 RBAC、pending、上报都会各写一套。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：配置与审计的主体是 machine（`machine_id`），agent 只是该 machine 上受管的 daemon。

1. `RegisterRequest` 必须携带 `machine_id`（稳定主机标识，1..128 字符），它标识配置主体。
   检查点：proto 字段存在且非空校验生效（`agent/proto/control.proto:98`）。
2. 每台 machine 恰好属于一个组：`machine.group_id NOT NULL`，不建机器-组多对多表。
   检查点：schema 中无 machine↔group 关联表，group_id 非空。
3. agent 身份只来自 mTLS 证书 CN=agent_id；服务端不得信任请求体自报的 agent_id。
   检查点：CN 查不到 agent 时返回 PERMISSION_DENIED。
4. machine_id 首次 Register 绑定后不可变；不一致返回 FAILED_PRECONDITION +
   `agent.machine_conflict`，改归属只能走管理面，不能由 agent 自助改。
5. `group_id` 只由发证/准入流程写入，Register 不参与组的决定。
   检查点：Register 落库时 group_id 取库内值，请求体无 group 字段。
## 替代方案与被否原因（≥1 条，写清代价）

- **以 agent 进程为主体**：每次重启/重装产生新主体，配置版本、ack 历史、上报游标全部断链；
  想恢复只能引入"agent 身份映射表"，等于把 machine 概念换个名字再做一遍。
- **以 group 为主体统一下发**：所有机器共享一份配置，单机灰度与单机审计无法表达；
  要做例外就得引入覆盖层，规则数量与审批复杂度翻倍。
- **agent 自报 machine_id 且可改**：实现最省事，但一台机器可以冒充另一台领取配置，
  审计与配额失效，故要求绑定后不可变。
## 后果（正面/负面/需要配套的动作）

正面：配置随机器稳定；身份只有一个来源（证书），审计可按机器追；聚合游标可按 machine 持久化。

负面：依赖宿主稳定标识，容器化后 Pod 名重建即视为新机（`control-center/docs/grpc-control-plane.md:720`）；
换机/重装必须人工改归属，当前没有自助入口。

配套动作：管理面"改归属"接口尚未实现（`docs/STATUS.md:27` 只登记了 E8 接线阻塞）；
准入侧需要保证发证与 group 绑定同事务，证书签发仍在 S2（`control-center/README.md:131`）。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `docs/Train0-Plan-6x1.md:120`：agent 作为设备主体使用独立 mTLS。
- `docs/Train0-Plan-6x1.md:122-127`：主体表；Agent 行写明证书 CN=agent_id、只能访问自身配置与上报。
- `docs/Train0-Plan-6x1.md:135-141`：主体硬规则。
- `docs/Train0-Plan-6x1.md:154`：C1 每台机器恰好 1 组、无多对多表。
- `docs/Control-Agent-API-v1.md:255-256`：`machine_id` 语义与长度约束。
- `docs/Control-Agent-API-v1.md:504`：首次绑定不可变、冲突错误码、改归属走管理面。
- `agent/proto/control.proto:98`：`string machine_id = 1;`。
- `agent/proto/control.proto:108-109`：RegisterResponse 回带 agent_id / machine_id。
- `control-center/docs/grpc-control-plane.md:303-305`：group_id 只由发证/准入写。
- `control-center/docs/grpc-control-plane.md:720`：machine_id 绑定代价（Pod 名重建即变）。
- `control-center/docs/plan-control-plane-impl.md:370`：D2 待拍板项即 group_id 写入点。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0002（传输）、ADR-0003（RBAC3 的 C1 依赖本决策）、ADR-0004（pending 主体边界）。
- 计划 §2.1，DoD §12.5（`docs/Train0-Plan-6x1.md:695` 要求主体模型 ADR）。
- 待拍板 D2（group_id 写入点）与 D6（machine_id 绑定代价）。
