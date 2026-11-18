# ADR-0004 pending 状态能否拉取配置

- 状态：提议（待拍板：D5；未拍板前按 `control-center/docs/plan-control-plane-impl.md:373` 的建议执行）
- 日期：2026-11-18（回填；争议原始记录见实施计划 §11 待拍板表）
- 决策者：Train0 团队（缺 PO + 控制面负责人拍板）
- 影响面：契约（状态语义）、控制面鉴权中间件、agent 启动流程、S1 验收剧本
## 背景（为什么现在必须有决定）

agent 注册后不是立刻可用：要等管理员/准入流程把它置为 active。这段 pending 期间
它能不能调用 GetConfig，直接影响三件事：agent 启动逻辑（拿不到配置要不要重试）、
控制面鉴权（是否按 RPC 分状态放行）、验收剧本（"pending 的机器拿到配置但不许执行"
算什么结果）。

契约目前只有 `AGENT_STATUS_PENDING` 枚举值，没有任何一段写 pending 的 RPC 行为；
实施计划把它列为 D5 待拍板。不拍板就按建议执行，但建议只写在表里，没有落进契约，
实现者仍会各自猜。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：pending 的 agent 可以维持连接与上报，但拿不到配置内容。

1. pending 状态允许调用 `Register`、`Heartbeat`、`GetCursor`、`ReportWindows`。
   检查点：mock 用 pending agent 调这四个 RPC 均成功。
2. pending 状态调用 `GetConfig` 恒返回 `up_to_date=true`，不返回 `document`、不下发版本号。
   检查点：响应里无 document，`desired_version` 不推进。
3. 状态检查在服务端单点实现（一个鉴权/状态中间件），不在每个 RPC 里复制 if 判断。
   检查点：新增 RPC 默认继承同一状态策略。
4. 状态从 pending 变为 active 后，下一次 `GetConfig` 才可返回配置；不需要重连。
   检查点：同一流上状态翻转后首个 GetConfig 返回 document。
5. 契约必须新增"状态与 RPC 可用性"一节，把上述行为写成表；枚举值本身不构成语义。
   检查点：契约中存在该表，且与本 ADR 第 1–2 条逐行一致。
## 替代方案与被否原因（≥1 条，写清代价）

- **pending 可拉且可执行**：等于把准入当摆设，被吊销/待审的机器能立即生效配置；
  审计上也无法区分"已批准"和"尚未批准"。
- **pending 只能 Register**：心跳与上报被拒，已经采到的窗口数据只能堆在 agent 本地，
  队列上限一到就丢；控制面也失去 pending 机器的存活视图。
- **不引入 pending，注册即 active**：少一个状态，但准入/发证流程没有缓冲，
  证书签发失败或组未绑定的机器会直接拿到配置。
## 后果（正面/负面/需要配套的动作）

正面：准入语义清晰，agent 启动逻辑简单（拿不到就等），验收剧本可判定。

负面：pending 机器持续心跳会占用连接与限流配额；如果运维忘了激活，机器会"活着但没配置"，
需要在 Web 上给出显式提示。

配套动作：契约补状态表；mock 补 pending 用例；`control-center/README.md:130-131`
的六方法实现需按本 ADR 复核；不拍板会阻塞 S1 验收，因为验收剧本无法判定
"pending 拿到配置却不许执行"这一中间态算通过还是失败。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `control-center/docs/plan-control-plane-impl.md:366`：不拍板就按建议执行。
- `control-center/docs/plan-control-plane-impl.md:372`：D4（相邻条目，同一张待拍板表）。
- `control-center/docs/plan-control-plane-impl.md:373`：D5 建议与不拍板后果全文。
- `control-center/docs/grpc-control-plane.md:719`：D5 的 pending 语义记录。
- `docs/Control-Agent-API-v1.md:223`：`AGENT_STATUS_PENDING = 3` 枚举，无行为描述。
- `docs/Control-Agent-API-v1.md:292`：`pending_reports` 字段（上报队列视角的 pending）。
- `docs/STATUS.md:42`：D5 被列为最急待拍板 3 条之一。
- `control-center/README.md:130-131`：六方法已实现、Watch 未实现、缺证书签发。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0001（主体）、ADR-0002（RPC 集合）、ADR-0003（准入与 RBAC 的关系）。
- 实施计划 §11 D5；`docs/backlog.md` 的待拍板区。
- 计划 §2.3（配置链路）与 §12.5 DoD。
