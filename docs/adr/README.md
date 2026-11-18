# ADR 索引（probe/docs/adr）

本目录存放 Train0 的架构决策记录（Architecture Decision Record）。一条 ADR 记录一个已经发生
或必须发生的决定：当时有哪些选项、为什么选这个、代价是什么、以后什么条件下推翻它。

ADR 不写进度。能力"有没有实现、实现到哪"只看 `docs/STATUS.md` 与
`control-center/README.md` §6（见 ADR-0007）。

## 何时必须写

满足任一条就必须先落 ADR，再改代码或契约：

- **不可逆**：一旦发布就难回退，例如 proto 字段号、CSV 列序、数据库主键、证书信任模型。
- **影响契约**：改 `agent/proto/control.proto`、`control-center/.../control.proto`、
  `Control-Agent-API-v1.md`、`probe-common/src/lib.rs` 的 ABI、CSV schema。
- **影响多人协作**：分支/合并/评审/CI 门禁/文档入口这类所有人每天都踩的规则。
- **推翻既定**：改已接受 ADR 的结论，或改计划里已冻结的范围与日期。

只改实现细节、不改上述四类的，不写 ADR，写提交说明即可。

## 编号与文件命名

- 文件名 `NNNN-kebab-title.md`，编号 4 位、从 0001 起严格递增。
- 编号**不复用**：ADR 被取代或废弃后，编号与文件都保留，状态改为"被 ADR-00XX 取代"。
- 标题用中文，kebab 部分用英文小写，例如 `0003-rbac-three-tier.md`。
- 一个决定一份 ADR；不要把"主体模型 + 上报格式"塞进同一份。

## 状态机

```
提议 ──拍板──> 已接受 ──推翻──> 被 ADR-00XX 取代
  │                │
  └──不采纳──> 废弃 <┘
```

- **提议**：已写清选项与代价，但缺指定的人拍板。文件开头必须点名缺谁（D#/Q#）与不拍板会阻塞什么。
- **已接受**：已生效，代码/契约按此执行。改结论必须走新 ADR，不得就地编辑。
- **被取代**：保留原文，只在状态行加指针；新 ADR 的"关联"里回指。
- **废弃**：决定不再执行且无替代，写明废弃日期与原因。

## 模板

```
# ADR-NNNN <标题>
- 状态：已接受 | 提议（待拍板：<D#/Q#>） | 被 ADR-00XX 取代
- 日期：YYYY-MM-DD（回填时注明；原始决策见证据）
- 决策者：Train0 团队
- 影响面：<代码/契约/流程/文档>
## 背景（为什么现在必须有决定）
## 决策（一句话 + 3–6 条具体条款，可判定）
## 替代方案与被否原因（≥1 条，写清代价）
## 后果（正面/负面/需要配套的动作）
## 证据（file:line 列表）
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）
```

"可判定"指能用是/否回答，或用一条命令验证。写"要保证安全"不合格，写"Watch 未实现时
必须返回 UNIMPLEMENTED"合格。

与 `docs/PROCESS.md` §7 的用词对应：`接受` = 已接受，`被取代（by NNNN）` = 被 ADR-00XX 取代。
状态行只能出现一个值（`docs/PROCESS.md:113-114`）；本目录的证据路径均相对仓库根 `probe/`。

## 索引

| 编号 | 标题 | 状态 | 日期 | 一句话 |
|---|---|---|---|---|
| [0001](0001-machine-as-config-subject.md) | 以 machine 为配置主体，agent 为受管 daemon | 已接受 | 2026-11-18 | 配置跟机器走，agent 身份只来自证书 CN |
| [0002](0002-grpc-over-rest.md) | 契约传输选 gRPC 而非 REST/JSON | 已接受 | 2026-11-18 | 单一 proto schema + 服务端推送，2026-11-14 同日改判 |
| [0003](0003-rbac-three-tier.md) | RBAC 三档模型（RBAC3） | 已接受 | 2026-11-18 | 四角色、组内继承、每机器恰好一组 |
| [0004](0004-pending-config-pull.md) | pending 状态能否拉取配置 | 提议（D5） | 2026-11-18 | pending 可心跳/上报，但 GetConfig 恒 up_to_date |
| [0005](0005-report-tables-and-key-hash.md) | 聚合上报三表与 key_hash bytea(32) | 提议（D4） | 2026-11-18 | 窗口/样本分表，key 用 32 字节二进制去重 |
| [0006](0006-six-week-cadence-baseline.md) | 6×1 周、W1 起 2026-11-09 的节奏基线 | 已接受 | 2026-11-18 | 6 个一周 Sprint，计划冻结后不回写状态 |
| [0007](0007-status-single-source.md) | 状态口径单一来源 docs/STATUS.md | 已接受 | 2026-11-18 | 计划只写计划，状态只写 STATUS.md |
| [0008](0008-branch-and-merge-policy.md) | 短分支 + squash + 合并删分支 + demo tag | 已接受 | 2026-11-18 | main 线性，合并即删分支，每周 Demo 打 tag |
| [0009](0009-review-lightweight-gate.md) | 评审降配但 gate 必绿 | 已接受 | 2026-11-18 | ≥1 条实质 review，不强制 approve，gate 兜底 |
| [0010](0010-docs-single-entry.md) | 文档单入口 probe/docs/ | 已接受 | 2026-11-18 | 文档入库评审，外层 docs 副本废弃 |
| [0011](0011-ci-gate-layering.md) | CI 与门禁分层 | 已接受（D7 待拍板） | 2026-11-18 | gate 恒执行，Docker 依赖用例不阻塞，CI 不跑真 PG |

## 维护规则

- 新增 ADR 时同步本表；表格行与文件必须一一对应，提交前自查编号连续。
- 改状态只改状态行与本表状态列，不改正文结论；结论变了就写新 ADR。
- 证据一律写仓库内 `file:line`。引用外部事实（GitHub PR 数据、分支保护）必须在证据里
  标注"仓库内不可复核"。
- 本目录自身遵循 ADR-0010：文档在 `probe/docs/` 入库，外层 `docs/` 不再新增副本。
