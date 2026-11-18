# ADR-0003 RBAC 三档模型（RBAC3）

- 状态：已接受
- 日期：2026-11-18（回填；原始决策见计划 v1.0 §2.2）
- 决策者：Train0 团队
- 影响面：契约（鉴权错误码）、控制面数据模型与 API、审计、验收用例
## 背景（为什么现在必须有决定）

Train 0 有 20 个组、100 台机器的设计规模，组管理员要在组内自治（提案、发布），
但平台管理员要能管组与机器、审计员要全量只读。若只做 admin/user 两级，
组内自治只能靠"人人都是 admin"，审计与越权无法约束。
计划 §2.2 已给出四角色、继承与 C1–C6 约束，但计划不是可执行口径，
DoD §12.5 明确要求单独一条 ADR 把角色边界固定下来。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：固定四角色、继承只在组内、每台机器恰好一个组，越权一律拒绝并留审计。

1. 角色只有四个：`platform_admin`（全局，继承 group_admin）、`group_admin`（组内，继承 member）、
   `member`（组内）、`auditor`（全局只读，不继承任何写权限）。
   检查点：角色枚举只有这 4 个值，新增角色必须新写 ADR。
2. 继承只在同一组内生效；`group_admin` 不能操作其它组；`platform_admin` 的全局继承不表示
   可以绕过 C3（提案人≠发布人）。
   检查点：跨组调用返回 403，且审计记录包含 actor/group/target。
3. C1：每台 machine 恰好属于 1 个组，`machine.group_id NOT NULL`，不存在机器-组多对多表。
   检查点：schema 无关联表，group_id 非空。
4. C3：提案人 ≠ 发布人；`platform_admin` 可 break-glass，但必须填写原因并产生高优先级审计。
   检查点：同人发布被拒；break-glass 缺原因被拒。
5. 组管理员不管理组成员；成员变更由平台管理员执行。
   检查点：组管理员调用成员管理 API 返回 403。
6. C5：审计只追加，不提供 UPDATE/DELETE API。
   检查点：审计表无更新/删除端点，数据库层只授予 INSERT/SELECT。
## 替代方案与被否原因（≥1 条，写清代价）

- **admin/user 两级**：实现便宜，但组内自治要么放开成 admin（越权），要么全部收归平台
  （瓶颈）；审计员无法只读。
- **ABAC/策略引擎（OPA 等）**：能表达任意规则，但 Train 0 只有四类角色、规则量极小，
  引入引擎要多一个进程、一套策略语言和一轮学习，收益不抵成本。
- **多对多机器-组**：一台机器可属多组时，"这台机器的配置属于谁"没有唯一答案，
  与 ADR-0001 的单组主体模型冲突。
## 后果（正面/负面/需要配套的动作）

正面：最小权限可判定；审计员只读满足合规叙事；组内自治不用平台介入。

负面：权限矩阵要逐端点测试，成本真实存在；负向用例目前只有 `/api/machines` 一条形状断言
（`control-center/README.md:134`），矩阵基本未验证。

配套动作：登录/会话/JWT 与 `/api/proposals` 尚未实现（`control-center/README.md:122-123`），
因此本 ADR 当前无法端到端验证；实现落地时按本文件补负向用例，
并在 `docs/STATUS.md` 更新进度（不写回计划）。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `docs/Train0-Plan-6x1.md:143-150`：四角色与继承关系。
- `docs/Train0-Plan-6x1.md:152-159`：C1–C6 约束（C1 :154、C3 :156、C5 :158）。
- `docs/Train0-Plan-6x1.md:161-168`：角色×动作权限矩阵。
- `docs/Train0-Plan-6x1.md:170`：组管理员不管理成员。
- `docs/Train0-Plan-6x1.md:695`：DoD 要求 RBAC3 必须有 ADR。
- `docs/STATUS.md:16`：S1-3 RBAC3 策略引擎未开始。
- `control-center/README.md:122-123`：登录/JWT、提案 API 未实现。
- `control-center/README.md:134`：6 个负向用例只有 1 条形状断言。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0001（C1 的单组模型）、ADR-0007（状态口径）。
- 计划 §2.2、§12.5；`docs/backlog.md` 的 S1-3 条目。
- 负向用例编号见 `verification/TEST-PLAN.md`（验收剧本 `docs/Train0-Plan-6x1.md:36`）。
