# Train 0 控制中心骨架（control-center）

`docs/Train0-Plan-6x1.md` 里控制中心的**可运行投影**：管理面 + 控制面 + Postgres + SPA，
一套 compose 就能起，一套 k3d 清单就能上集群。目标不是把 S1–S3 的功能写完，而是先把
**镜像边界、数据库边界、网络边界**三件事定死——后面的功能都长在这三条边界上。

本目录属于 `probe` 仓库，因此改动在分支 `feat/control-center-skeleton` 上。

## 1. 四个镜像，各装什么

| 镜像 | 基础镜像 | 装了什么 | 为什么是这些 |
|---|---|---|---|
| `cc-java:dev` | `maven:3.9-eclipse-temurin-21` → `eclipse-temurin:21-jre` | JDK 21（构建阶段）、Maven、JRE 21、curl | 构建与运行分离：运行镜像没有编译器/包管理器，攻击面和体积都小；curl 是 healthcheck 和排障的唯一依赖 |
| `cc-pg:17` | `postgres:17-bookworm` | `psql`/`pg_dump`/`pg_restore`/`pg_isready`/`pgbench`、contrib（pgcrypto、pg_stat_statements、pg_trgm、uuid-ossp） | 官方镜像已自带，不额外装；客户端大版本与服务器一致，`\d` 输出才可对照 |
| `cc-web:dev` | `node:22-alpine` → `nginx:1.27-alpine` | Node（构建期）、npm、nginx、wget | 构建期需要 Node，运行期只需要静态文件 + 反向代理；`wget` 是 alpine 自带，用于 `/healthz` |
| `cc-toolbox:dev` | `postgres:17-bookworm` | psql 17、`curl`、`jq`、`openssl`、`jose`、`make`、`git`、`dig`、`ip` | **唯一**同时具备"造负向 token + 查库 + 算时间"的镜像；基础选 postgres 是为了 psql 17 与服务端同版本 |

关键取舍（第一轮问答的结论，已落到 Dockerfile）：

- **不放 docker CLI / docker.sock 进服务镜像**。Testcontainers 在 k8s 里本来就没有 daemon，
  放进去只会让"集成测试通过"变成假象；集成测试连 compose 的真 Postgres。
- **不放 Redis / Kafka / ELK**。计划明确"不做缓存回退"（`docs/Train0-Plan-6x1.md:41`）、
  outbox 是**表**（`:174-179`）、防重放用**进程内存**（`:139`）。
- **JRE 里没有 keytool 之外的 PKCS12 工具也无所谓**：Spring SSL bundle 用 keytool 生成的
  keystore 即可；需要 PEM ↔ PKCS12 互转时用 cc-toolbox 的 openssl。
- **`jose` 只进 toolbox**：它是造"过期 token / 错 aud / 重放 jti"这些负向用例的工具，
  服务端只需要公钥验签，不需要签发 CLI。
- **uid 事实**：`postgres:17-bookworm` 是 `uid=999`（alpine 版才是 70），k8s 清单里的
  `runAsUser/fsGroup` 按 999 写；`cc-java` 自建 `uid=10001` 非 root 用户。

## 2. 拓扑与端口

```
浏览器 ──:8080──> cc-web (nginx)
                    ├── /            → SPA 静态文件
                    └── /api/        → cc-mgmt:8080
cc-mgmt    :8081──> 管理面（会话、机器、提案、审计查询）
cc-control :8082──> 控制面（PEP、版本、agent 通道）   ← 仅 host loopback / 仅 mgmt 可达
cc-pg      :55432─> mgmt 库 + control 库（同实例，不同库、不同角色）
```

k3d 下同样的名字走 Traefik Ingress，`/api` → `cc-mgmt`，`/` → `cc-web`；
**`/internal/` 与 `/agent/` 不在 Ingress 里**，控制面只接受 `app=cc-mgmt` 的入站流量
（`deploy/k3d/k8s/70-netpol.yaml`），对应验收步骤 15「bob 不能直连控制中心」。

## 3. 快速开始（compose）

```bash
cp .env.example .env          # 可选，默认值即可跑
bash scripts/up.sh            # 构建 + 起栈 + 等 readiness
bash scripts/smoke.sh         # 代理、双角色、种子数据
bash scripts/audit-check.sh   # 审计表 append-only 证据
bash scripts/p95.sh 50        # 顺序采样，G1 预算 3s
bash scripts/migrate.sh mgmt 1   # 迁移 down 演示（回滚 1 个 changeset）
bash scripts/down.sh --wipe   # 停栈并删数据卷（重跑 initdb 的唯一方式）
```

`make up / smoke / p95 / audit-check / migrate DB=mgmt N=1 / psql DB=mgmt / k3d-up` 是同一批脚本
的别名；Windows 没有 make 时直接用 `bash scripts/*.sh`。

## 4. k3d

```bash
bash scripts/down.sh          # 两个栈都占 8080，先停 compose
bash scripts/k3d-up.sh        # 建集群 + 导入镜像 + apply + 等 rollout
bash scripts/k3d-down.sh
```

集群名 `cc-train0`，`deploy/k3d/cluster.yaml` 把 8080/8443 映射到 Traefik。
选 k3d 而不是 kind 的原因：k3s 自带 NetworkPolicy 控制器，kind 默认 CNI 不实现 netpol，
而"拒绝 bob 直连控制中心"这条验收必须真的能拒绝。

## 5. 数据库与迁移

`deploy/images/cc-pg/initdb/` 只做**一次**、且只做**权限形状**的事（superuser 才能做）：

| 文件 | 内容 |
|---|---|
| `00-roles.sh` | `cc_migrator` / `cc_app` / `cc_audit` / `cc_ro` 四个登录角色，密码来自环境变量 |
| `10-databases.sql` | `mgmt`、`control` 两个库，owner 都是 `cc_migrator` |
| `20-schemas.sql` | 两个库各自的 schema、`DEFAULT PRIVILEGES`（建表即授权）、pgcrypto |

因此 **Liquibase 永远不需要 superuser**：迁移用 `cc_migrator`，运行用 `cc_app`。

- 迁移 **up**：服务启动时自动执行（`spring.liquibase.*`）；也可 `scripts/migrate.sh <db> up`。
- 迁移 **down**：`scripts/migrate.sh <db> <n>`，用同一个 changelog 的 `--rollback` 段，
  在一次性 Maven 容器里跑 `liquibase:rollback`，Liquibase 版本由 Boot BOM 管，与运行期一致。
  实测回滚 1 个 changeset 后 `databasechangelog` 4→3、种子行 5→0，再 `up` 回到 4→5。
  两个必须显式给的参数：`-Dliquibase.defaultSchemaName=<db>`（否则 Liquibase 在 `public`
  里找 `DATABASECHANGELOG`，判定“0 changesets rolled back”却仍以 0 退出）与 changelog 头部的
  `logicalFilePath`（让服务走 classpath、脚本走文件系统时记录同一个 changeset 身份）。
- 审计表 `mgmt.audit` 双保险：`BEFORE UPDATE OR DELETE` 触发器 + 对 `cc_app`
  `REVOKE UPDATE, DELETE, TRUNCATE`。`scripts/audit-check.sh` 对 UPDATE/DELETE/TRUNCATE 三条都断言失败。
  触发器的 plpgsql 函数体放在 initdb（`20-schemas.sql`），因为 Liquibase 的 formatted SQL
  切分器会把 `$$ ... $$` 截断成 "Unterminated dollar quote"；changelog 里只留单条
  `CREATE TRIGGER`，不含内嵌分号。
- 上报幂等键 `(agent_id, window_id, key_hash)` 已建在 `control.report` 上；防重放按计划留在内存，
  不建表。

## 6. 已经有的 vs 还没有的

已有：两个角色共用一个 jar（`CC_ROLE=mgmt|control`）、健康/就绪探针、SPA 只读视图、
种子数据（admin/aud/alice/bob/carol、g1/g2、m1/m2）、审计只追加、outbox 表结构、
两库物理隔离、compose + k3d 两套部署、全程 UTC。

还没有（对应计划后续切片，不假装完成）：

- 登录/会话/JWT 签发与校验、jti 内存防重放（S1-2）
- `/api/proposals`、发布、`/agent/v1/*`、`/internal/v1/audit/events` 的实际实现（S1-3）
- mTLS 设备证书签发与 CN 绑定（S2）
- 6 个负向验收用例（`docs/Train0-Plan-6x1.md:36`）目前只有 `/api/machines` 一条形状断言
- 种子里的 `password_hash` 是 `!pending` 占位，S1-2 换 Argon2id，不允许手写 hash

## 7. 目录

```
app/                     Maven 单模块 cc-service（Boot 3.5.16 / Java 21）
  src/main/resources/db/{mgmt,control}/changelog.sql   Liquibase formatted SQL
web/                     Vite + React SPA（dev 代理 /api → 127.0.0.1:8081）
deploy/images/           四个镜像的 Dockerfile + cc-pg initdb + nginx.conf
deploy/k3d/              cluster.yaml + k8s/ 清单（kustomize）
scripts/                 up/down/smoke/p95/audit-check/migrate/psql/k3d-up/k3d-down
compose.yaml             cc-pg / cc-mgmt / cc-control / cc-web / cc-toolbox(profile tools)
```

## 8. 已知限制

- 控制面在 compose 下暴露 `127.0.0.1:8082`，只为本地排障；k3d 下没有任何对外路径。
- 单实例假设原样保留（内存防重放、单副本 StatefulSet），这是计划明确的范围。
- `cc-toolbox` 需要 `--profile tools` 才起，且不挂 docker.sock：它查库、造 token，不编排容器。
- 首次 `migrate.sh` 会在容器里下载 Liquibase 插件，几分钟；之后有 Maven 缓存则很快。
