# ADR-0002 契约传输选 gRPC 而非 REST/JSON

- 状态：已接受
- 日期：2026-11-18（回填；原始改判 2026-11-14，见契约 §14）
- 决策者：Train0 团队
- 影响面：契约（wire protocol）、proto 双副本、agent 与控制面构建链、调试工具
## 背景（为什么现在必须有决定）

契约首版（2026-11-14 早些时候）写成 `/agent/v1/*` HTTP + JSON，同一天改判为 gRPC。
改判的窗口很窄：Spring 侧当时尚未开始实现，换传输没有返工成本；一旦两侧都写起来，
再换就要同时改 Rust agent、Spring 服务端、mock 与全部验收脚本。
现在契约、proto、agent 与 mock 都按 gRPC 落地，必须把"为什么"和"何时可以退回"写死，
避免后来者凭直觉加一条 REST 旁路。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：agent ↔ 控制面固定为 gRPC over HTTP/2 + TLS1.3 双向认证，单一 `control.proto` schema。

1. 传输固定 gRPC，服务端口 9090 只在 `CC_ROLE=control` 实例上监听，启用 ALPN h2 与
   `clientAuth=REQUIRE`；不得在 8080 控制面上暴露 agent RPC。
   检查点：`curl` 9090 无 HTTP/1.1 正常响应；未带客户端证书握手失败。
2. 契约结构以 `agent/proto/control.proto` 为准；仓库内两份 proto 必须逐字节相同，
   CI 做 diff，禁止手改其中一份。
   检查点：`diff agent/proto/control.proto control-center/app/src/main/proto/control.proto` 为空。
3. `AgentControl` 固定 7 个 RPC；`Watch` 在服务端未实现时必须返回 `UNIMPLEMENTED`，
   不得返回空流或静默降级成轮询。
   检查点：gRPC 客户端调用 Watch 得到 `UNIMPLEMENTED`。
4. proto 字段号一旦发布不得复用；删字段要 `reserved`。
   检查点：契约与 review 检查字段号连续性。
5. 只有 §14 列出的改判条件成立才允许回退 REST：spring-grpc 与现有 Boot/MVC 栈冲突到
   必须改整站 TLS，或 protoc 无法进 CI。回退必须新写 ADR 取代本文件。
   检查点：改判 PR 必须引用本节并附冲突证据。
## 替代方案与被否原因（≥1 条，写清代价）

- **REST + JSON**：要自己定义并实现 RFC 8785 规范化才能对配置文档求稳定哈希，两份 schema
  （proto 与 HTTP 绑定）长期漂移，且没有服务端推送，只能轮询。
- **REST + 手工推送回调**：计划原方案。需要 agent 暴露回调端口并处理 NAT/防火墙，
  推送失败要有重试队列，控制面复杂度高于一条 server stream。
- **gRPC-Web 直连浏览器**：控制面 SPA 走 BFF 即可，引入 gRPC-Web 只增加一层代理与调试成本。
## 后果（正面/负面/需要配套的动作）

正面：删掉配置文档规范化（`docs/Control-Agent-API-v1.md:901`）；Watch 推送延迟实测 5–22ms，
纯轮询 934–977ms（`:789`）；单一 schema 消除双侧漂移；报文体量约为 JSON 的 1/4（`:904`）。

负面：protoc 进入 Rust 与 Maven 两条构建链（`:908`）；调试要 grpcurl（`:909`）；
gRPC 状态码需要翻译成契约错误码（`:910`）；新人学习成本（`:911`）。

配套动作：`agent/proto` 与 `control-center/.../proto` 的同源校验必须留在 CI；
Watch 在真控制面仍是 `UNIMPLEMENTED`（`control-center/README.md:130-131`），
客户端必须保留轮询回退路径；D9（Watch 错误码）待拍板。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `docs/Control-Agent-API-v1.md:4`：同日首版 REST+JSON、后改判 gRPC，依据 §14。
- `docs/Control-Agent-API-v1.md:788`：`/agent/v1/*` 到 gRPC 9090 的差异条目。
- `docs/Control-Agent-API-v1.md:897`：改判前提（Spring 侧未开始实现，无返工成本）。
- `docs/Control-Agent-API-v1.md:895-915`：§14 收益、代价、前置、改判条件。
- `docs/Control-Agent-API-v1.md:32-34`：9090 仅 control、机器身份域与人身份域分离理由。
- `docs/Control-Agent-API-v1.md:156`：两份 proto 必须逐字节相同。
- `docs/Control-Agent-API-v1.md:140-142`：Watch 未实现必须 UNIMPLEMENTED。
- `docs/Control-Agent-API-v1.md:62`：proto3 字段号不可复用。
- `agent/proto/control.proto:14-21`：service 与 7 个 rpc。
- `control-center/docs/grpc-control-plane.md:111-112`：两份 proto 同源、以 Rust 侧为准。
- `.github/workflows/control-center.yml:7-9`：CI 只做不装 Docker 的校验（protoc 在构建链内）。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0001（主体）、ADR-0004（pending 语义依赖 RPC 集合）、ADR-0011（CI 分层）。
- 计划 §2.3（配置链路）、契约 §11/§14，DoD §12.5（`docs/Train0-Plan-6x1.md:695`）。
- 待拍板 D9（Watch 错误码）、D10（门禁在容器/Git Bash）。
