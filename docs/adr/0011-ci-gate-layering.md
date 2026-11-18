# ADR-0011 CI 与门禁分层：gate 恒执行，Docker 依赖不进合并门

- 状态：已接受（其中"CI 不跑真 PG"子条款待拍板：D7）
- 日期：2026-11-18（回填；workflow 结构见同日 ci.yml / control-center.yml）
- 决策者：Train0 团队（D7 缺控制面负责人拍板）
- 影响面：CI（workflow 结构）、分支保护、发布前检查、集成测试位置
## 背景（为什么现在必须有决定）

仓库有两个可独立运行的项目（Rust 侧与控制面），构建成本差异极大：Rust 侧要 nightly +
LLVM 才能编 BPF，控制面要 JDK 21 + Node + 四个镜像。如果所有检查都塞进一个 workflow，
每个 PR 都要等十几分钟；如果用 workflow 级 `paths:` 过滤，被跳过的 job 不会上报状态，
分支保护把它设成 required 就会永远 pending。

同时有些用例天然依赖 Docker/k3d 或真 Postgres，runner 上跑又慢又不稳。必须明确
"哪些检查是合并门、哪些只在本地做"，否则要么门禁失效，要么 PR 卡死。
## 决策（一句话 + 3–6 条具体条款，可判定）

一句话：PR 合并门只有恒执行的 `gate`，昂贵/依赖环境的检查分层放到独立 workflow 或本地。

1. `ci.yml` 用 `changes` + `fast` + `bpf` 三个 job 产出结果，由 `gate`（`if: always()`）
   汇总；gate 把 skipped 翻译成绿，分支保护只需要 required 这一个检查。
   检查点：gate job 在任意 PR 上都有结论（success/failure），不会 pending。
2. 改 `control-center/**` 走独立的 `control-center.yml`，不触发 Rust 的 L0–L3；
   改 Rust 不跑控制面的 Maven/Vite 校验。
   检查点：PR 的 checks 列表只出现相关 workflow（另一个因 paths 不匹配而不启动）。
3. 依赖 Docker/k3d/四镜像的检查不进 PR gate：`docker build`、`k3d-up.sh`、`smoke.sh`
   只在本地执行；CI 只做"不装 Docker 也能做"的校验（`mvn package`、`vite build`、
   compose/kustomize 静态校验、脚本语法）。
   检查点：`control-center.yml` 无 docker build/k3d step。
4. 镜像链路单独放 `image.yml`，只在 main 与手动触发时跑 `Dockerfile.ci` 的 check target，
   不参与 PR 合并门，也不推 registry（仓库无凭据）。
   检查点：`image.yml` 的 `on` 不含 `pull_request`。
5. CI 不跑真 Postgres：PG 集成测试用 `@EnabledIfEnvironmentVariable(CC_IT_PG)` 跳过，
   门在本地容器执行（D7）。
   检查点：未设 `CC_IT_PG` 时 PG 用例为 skipped，CI 仍绿。
6. CI 绿不等于栈能起：发布前必须本地跑 `scripts/up.sh` + `smoke.sh`，结论记入 STATUS。
   检查点：发布 PR 描述含本地 smoke 结果。
## 替代方案与被否原因（≥1 条，写清代价）

- **workflow 级 `paths:` 过滤**：被跳过的 job 不上报状态，required check 永远 pending；
  这正是 `ci.yml:12-18` 改用 changes+gate 的原因。
- **单个大 workflow 跑全部检查**：每个 PR 都付 Docker 构建与 k3d 启动成本（十几分钟），
  小团队等待成本高于收益；runner 上 k3d 稳定性也差。
- **CI 跑真 PG + docker compose**：覆盖最全，但需要 services/initdb 与镜像拉取，
  D7 估算需追加 0.5 人日，Train 0 时间盒内不划算；缺口用本地门 + 发布前 smoke 兜底。
## 后果（正面/负面/需要配套的动作）

正面：PR 反馈快；门禁语义单一（只看 gate）；控制面与 Rust 侧互不拖累。

负面：Docker/k3d/真 PG 路径没有 CI 覆盖，只能靠本地纪律；`gate` 把 skipped 视为绿，
意味着"该跑没跑"也会绿，需要 changes 规则保持准确。

配套动作：D7 拍板（拍"跑"则要加 `services: postgres` + initdb，0.5 人日）；
`gate` 的 required 设置在 GitHub 侧，仓库内无记录；发布前 smoke 结果写 `docs/STATUS.md`。
## 证据（file:line 列表，路径相对仓库根 probe/）

- `.github/workflows/ci.yml:3-18`：三 job + gate 的设计理由，以及不用 paths 过滤的原因。
- `.github/workflows/ci.yml:19-20`：e2e gate 留在 main，S3 才有 L3。
- `.github/workflows/ci.yml:207-210`：gate `if: always()`、needs changes/fast/bpf。
- `.github/workflows/ci.yml:213-229`：gate 汇总并把 skipped 视为绿。
- `.github/workflows/control-center.yml:3-13`：独立 workflow、只做不装 Docker 的校验、skipped 翻译为绿。
- `.github/workflows/control-center.yml:110-114`：control-center 的 gate job。
- `.github/workflows/image.yml:3-17`：镜像 check 的用途与触发（main + 手动）。
- `CONTRIBUTING.md:128-132`：CI 分工；分支保护只需 gate 一个检查。
- `control-center/README.md:155-157`：CI 只覆盖不装 Docker 的校验，CI 绿不等于栈能起。
- `control-center/docs/plan-control-plane-impl.md:375`：D7 建议、不拍板后果与 0.5 人日缺口。
- `docs/STATUS.md:37`：三个工作流与"结论未核实"的记录方式。
## 关联（相关 ADR、计划条目 D#/Q#、DoD 条款）

- ADR-0009（评审与 gate 的关系）、ADR-0010（文档变更也走 gate）。
- 实施计划 §11 D7、D10；`CONTRIBUTING.md` §3；DoD 的 CI 覆盖条款。
