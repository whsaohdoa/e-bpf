# ADR-0005 聚合与上报：report 三表 + key_hash bytea(32)

- 状态：提议（待拍板：D4；未拍板前按 `control-center/docs/plan-control-plane-impl.md:372` 的建议执行）
- 日期：2026-11-18（回填；schema 变更见 `control-center/docs/grpc-control-plane.md` §5.3）
- 决策者：Train0 团队（缺控制面负责人 + 数据模型 owner 拍板）
- 影响面：数据库 schema（Liquibase）、上报契约、幂等语义、Web 查询、E8 验收
## 背景（为什么现在必须有决定）

agent 按窗口聚合，每个窗口最多上报 1000 条样本，每条样本的身份是 56 字节二进制 key
的哈希。早期 schema 把整包塞进 `payload jsonb`，既不能按 key 去重，也不能按游标增量查询。
控制面已经把 schema 改成 `report_window` / `report` / `report_sample` 并给 `key_hash`
加了 32 字节约束，但"是否拆三表、哈希是否用 bytea"仍是 D4 待拍板项。
再不固定，E8 接线上线后改表要重做回归。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：窗口元数据、样本行、原始批次分三表存，key 用 32 字节二进制哈希做幂等键。

1. 三表模型：`report_window`（每 agent 每窗口一行，含 `complete`/`truncated`/`row_count`）、
   `report`（样本行，含 `key_hash`）、`report_sample`（原始批次，用于排障与重放）。
   检查点：schema 中三表存在，`report_window` 主键为 `(agent_id, window_id)`。
2. `key_hash` 类型为 `bytea`，并带 `CHECK (octet_length(key_hash) = 32)`。
   检查点：`\d control.report` 显示 bytea + CHECK。
3. 幂等键为 `(agent_id, window_id, key_hash)`；重复上报走 upsert，不产生第二行。
   检查点：同窗口同 key 上报两次，行数不变。
4. 0 行窗口也必须写 `report_window`（`row_count=0`、`complete` 按实际），否则"这段时间
   没有流量"与"这段时间没上报"无法区分。
   检查点：空窗口上报后 `report_window` 有行、`report` 无行。
5. 游标由控制面权威推进，agent 只能读到控制面给的游标；不得由 agent 自报进度覆盖。
   检查点：GetCursor 返回值与库内游标一致。
## 替代方案与被否原因（≥1 条，写清代价）

- **保持 `payload jsonb`**：写入最简单，但无法按 key 去重（同一 key 会在多批次重复），
  无法做游标查询，Web 端每次都要解 JSON；D4 的"不拍板后果"即此，E8 回归要重做表。
- **单表扁平存全部字段**：窗口元数据在每行重复，1000 行窗口放大存储；0 行窗口无处落。
- **key_hash 用 text（hex/base64）**：32 字节变 64+ 字符，索引变大；且两侧文本格式
  可能不一致（IPv6 文本表示差异，见契约 §7.1），二进制是唯一稳定表示。
## 后果（正面/负面/需要配套的动作）

正面：按 key 去重与增量游标都成立；空窗口可判定；Web 查询不必解析 jsonb。

负面：三表写入需要事务边界（窗口行与样本行同事务）；`report_sample` 会放大存储，
需要保留策略，否则 Train 0 之后要补清理任务。

配套动作：契约需补"CSV 16 列 ↔ 表字段"的映射（当前只有 CSV 列定义，
见 `README.md:307`）；`control-008/009` 迁移已写好但 D4 未拍板，属"代码先行、决策待补"；
拍板前不得基于该 schema 写对外承诺。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `control-center/docs/plan-control-plane-impl.md:372`：D4 建议与不拍板后果。
- `control-center/docs/grpc-control-plane.md:328-342`：`control-008` report_window DDL。
- `control-center/docs/grpc-control-plane.md:344-349`：`control-009` report 表与 bytea CHECK。
- `control-center/docs/grpc-control-plane.md:372-373`：key_hash 用 bytea 的理由。
- `docs/Control-Agent-API-v1.md:665-669`：二进制 key_hash 的理由（IPv6 文本格式不一致）。
- `docs/Control-Agent-API-v1.md:671-697`：56 字节 key 布局。
- `docs/Control-Agent-API-v1.md:91`：ReportWindows 幂等键 `(agent_id, window_id, key_hash)`。
- `docs/Train0-Plan-6x1.md:241-249`：每 key 每窗口 1 样本、上报与幂等 upsert。
- `control-center/README.md:108`：幂等键已建在 `control.report` 上。
- `README.md:307`：聚合 CSV 16 列。
- `verification/TEST-PLAN.md:205-206`：CSV schema 断言（7 列/16 列）。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0001（agent 主体）、ADR-0002（上报走 gRPC）、ADR-0004（pending 期间的上报）。
- 实施计划 §11 D4；计划 §2.5（聚合与上报）。
- DoD：聚合验收（`verification/REPORT-aggregation.md`）与 E8 回归。
