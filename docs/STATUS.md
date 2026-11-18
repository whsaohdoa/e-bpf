# 项目状态（STATUS）

更新 2026-11-18（W2 末）｜**本文件是当前进度的唯一权威**；计划文档只写计划，不写状态。
更新触发：每次合并到 main、每次 demo 后 24h 内；其余时间不维护。

## 1. 口径仲裁
| 争议 | 冲突双方 | 裁决 |
|---|---|---|
| gRPC 服务端是否已实现 | `README.md:8,509`（原文）：七个方法都还没写 ↔ `control-center/README.md:126-131`：六个方法已实现，仅 Watch `UNIMPLEMENTED` | 以 `control-center/README.md` §6 为准；`probe/README.md` 与契约的过时表述已在本分支修正（见 `docs/README.md` §3） |
| E6/E8 是否完成 | `probe/README.md:541-544`：E6 未实现、E8 只完成 Rust 侧 ↔ `control-center/README.md:157-163`：两端就绪但未接线 | 两者均未完成，E8 只差接线；以控制面 README 为准 |
| 计划文档能否当进度看 | Train0 全文 0 个复选框；`TEST-PLAN.md:382-392` 进度是带日期的散段，§10 仅 1/6 勾选 | Train0 保持计划性质不改写；TEST-PLAN §10 只作 DoD；进度只看本文件 |

## 2. 本周（W2）计划 vs 实际
| 计划项（Train0 §5.2） | 实际 | 证据 |
|---|---|---|
| S1-3 RBAC3 策略引擎 | 未开始 | `control-center/README.md:120`：登录/会话/JWT 还没有 |
| S1-5 提案/发布 API | 未开始 | `control-center/README.md:121`：`/api/proposals` 未实现 |
| S1-6 控制面版本/注册/通知 | 部分：gRPC 六方法与库表已合并，发布入口未定 | `control-center/README.md:126-131`；PR #10 / `97ea0d8` |
| S1-8b E6 规则匹配 | 未开始 | `probe/README.md:541`：action 恒 pass、rule_id=0 |
| 实际投入 | 全部落在 M0–M4，不在本表计划项内 | `control-center/docs/plan-control-plane-impl.md:55-67` |

## 3. 进行中
- M4 部署与验收：`smoke.sh` 已断言 9090 角色门，`PollingLatencyTest` 已打印 `g1_p95_ms`（`control-center/scripts/smoke.sh:96-106`、`control-center/app/src/test/java/dev/train0/cc/control/PollingLatencyTest.java:96-99`）；通过性未核实。
- 文档基线：本文件、`docs/README.md`、`docs/backlog.md` 与并行的 PROCESS/adr/runbooks 一起落地（入口见 `docs/README.md`）。

## 4. 阻塞
- E8 接线：k3d 的 agent 命名空间与 `control-plane-tls` secret 仍需手工（`control-center/README.md:157-163`）。
- E6 规则匹配：`rules` 只降级不执行（`probe/README.md:541-544`）。
- fail-open：数据面恒 `XDP_PASS`，与计划相反，未决策（`probe/README.md:518`、`Train0-Plan-6x1.md:190`）。
- 6 个负向用例只有 `/api/machines` 一条形状断言（`control-center/README.md:133`、`Train0-Plan-6x1.md:36`）。
- Docker 环境 CI：四镜像 build、k3d、smoke 仍只在本地（`control-center/README.md:154-156`）。

## 5. 下周（W3）候选
- 首选三条：E8 接线、E6 规则匹配、6 个负向用例；完整 12 条见 `docs/backlog.md`——**未经 Planning 不承诺**。

## 6. 最近门禁
`ci.yml` / `control-center.yml` / `image.yml` 三个工作流；最近一次结论**未核实**（shell 不可用，需恢复后查 Actions 记录）。

## 7. 待拍板（最急 3 条；D2–D10 全表与契约 §15 Q1–Q7 见 `docs/backlog.md`）
- D2：`control.agent.group_id` 由准入/签发证书时写入（`control-center/docs/plan-control-plane-impl.md:370`）。
- D3：Train 0 发布入口只留 `ConfigPublisher`，HTTP 发布 API 归 S1-3（同上 :371）。
- D5：`pending` agent 可 Register/Heartbeat/GetCursor/ReportWindows，GetConfig 恒 `up_to_date`（同上 :373）。

## 8. 指向
`CONTRIBUTING.md`、`docs/README.md`、`docs/backlog.md`、`docs/PROCESS.md`、`docs/adr/`。
DoD §12.5 要求的四份决策见 `docs/adr/0001`（主体模型）、`0003`（RBAC3）、`0004`（配置下发，状态“提议”）、`0005`（聚合上报，状态“提议”）——后两份随 §7 的 D5、D4 拍板转“已接受”。
