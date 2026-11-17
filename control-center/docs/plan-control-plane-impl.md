# 控制面实现计划（cc-service gRPC 服务端）

> 本文件是**计划**，不是实现，也不是设计正本的替代。设计正本 `control-center/docs/grpc-control-plane.md`（737 行，下称"设计"）；契约 `../docs/Control-Agent-API-v1.md`（1013 行）。
> 基线：分支 `chore/boot-4.1.2-upgrade`，Boot 4.1.2 / Framework 7.0.10 / Jackson 3.1.7（`tools.jackson`）/ Liquibase 5.0.4 / spring-grpc 1.1.2 / grpc-java 1.84.1 / protobuf 4.36.2（`spring-boot-dependencies-4.1.2.pom`，见 §9 spike A）。
> 除本文件外不修改任何实现文件；工作区现有未提交改动不得 `git add/commit/checkout/stash`。

## 1. 目标 / 非目标

### 目标

1. `cc.role=control` 在 9090 上起原生 gRPC 服务（mTLS，`client-auth: require`）；`cc.role=mgmt` 不监听 9090。
2. `control-center/app/src/main/proto/control.proto` 与 `agent/proto/control.proto` 逐字节一致，漂移在 CI 挂掉。
3. 六个 RPC 按契约 §5 实现（Register/Heartbeat/GetConfig/AckConfig/ReportWindows/GetCursor），Watch 返回 `UNIMPLEMENTED`。
4. control 库 control-006…010 落地：agent 注册列、`document bytea`、report 三表、config_ack。
5. 错误码、`x-request-id`/`x-error-code`、每 RPC 一行结构化日志与设计 §4/§8 对齐。
6. 测试矩阵（单元 / 真 PG / 真 TLS）在容器化 Java 21 上可重复执行，负向用例有断言。
7. 部署清单（compose / k8s / netpol / smoke / README）与实现一致。
8. 上述每一项都有一个"能挂掉"的门（§8）。

### 非目标

1. **Watch 实现**——保持 `UNIMPLEMENTED`（契约 L4）。
2. **Rust `probe/` 侧任何改动**、E8 接线、真 `probe-agent` E2E（标为环境外门，§6）。
3. REST 管理面鉴权/RBAC、PEP（S1-3 的活）。
4. k3d 生产化（ingress、多副本、资源调优）。
5. 性能调优（连接池参数、缓存、批量写优化）；Train 0 只做正确性。
6. `audit_outbox` 的转发 consumer（`forwarded_at` 由 S1-13 填）。
7. config 发布 HTTP API（D3：S1-3 做，见 §11）。
8. 服务端解码/重编码 config、capability 裁剪（agent 侧职责）。

## 2. 现状基线

| 项 | 现状 | 路径（"将落"= 本计划新增） |
|---|---|---|
| Boot 4.1.2 / Java 21 | 已有 | `control-center/app/pom.xml:7-19` |
| gRPC starter | **缺失** | 将落 `control-center/app/pom.xml` `<dependencies>` |
| protobuf 代码生成 | **缺失**（无 `src/main/proto`、无插件） | 将落 `control-center/app/pom.xml` `<build><plugins>` |
| proto 副本 | **缺失** | 将落 `control-center/app/src/main/proto/control.proto` |
| proto 同源门 | **缺失** | 将落 `control-center/scripts/proto-check.sh` |
| CI 门禁 | 部分（`-DskipTests`，无 proto 门） | `probe/.github/workflows/control-center.yml:46-49,68-72` |
| gRPC/SSL 属性 | **缺失** | 将落 `control-center/app/src/main/resources/application.yml`（jackson 块 `:26-27` 之后） |
| 角色装配代码 | **缺失** | 将落 `control-center/app/src/main/java/dev/train0/cc/control/` |
| gRPC 服务实现 | **缺失**（仅 REST） | `.../java/dev/train0/cc/api/{HealthController,MachineController}.java` |
| 测试树 | **缺失**（无 `src/test`） | 将落 `control-center/app/src/test/java/dev/train0/cc/control/` |
| agent 注册列 | **缺失** | `control-center/app/src/main/resources/db/control/changelog.sql:3-14`（追加 006） |
| `config_version.document` | `jsonb` | `.../db/control/changelog.sql:24`（007 改 bytea） |
| report 表 | 旧形态（`payload jsonb`） | `.../db/control/changelog.sql:63-73`（008/009 重建） |
| `config_ack` | **缺失** | 010 新建 |
| 9090 端口（compose） | 已发布，无进程 | `control-center/compose.yaml:81-85` |
| 9090（k8s） | 已声明 | `control-center/deploy/k3d/k8s/40-control.yaml:13-17,44-45` |
| netpol agents | 已预留 `from: []` | `control-center/deploy/k3d/k8s/70-netpol.yaml` |
| smoke 9090 断言 | **缺失** | `control-center/scripts/smoke.sh:27` 后 |
| 测试证书 | **缺失** | 将落 `control-center/scripts/dev-certs.sh` → `control-center/.tmp/tls/` |

## 3. 里程碑切片

| 里程碑 | 范围 | 依赖 | 设计章节 | backlog | 人日 | 可并行 |
|---|---|---|---|---|---|---|
| **M0** 构建骨架 | gRPC/protobuf 依赖、proto 副本、生成、proto 门、CI | — | §2.1–2.3、§13.1 | S1-6（部分） | 1.5 | 与 M1 部分并行 |
| **M1** 字节与值域 | 向量测试、validator | M0 | §6.4、§7、§13.2 | S1-6（部分） | 1.0 | 与 M2 并行 |
| **M2** 库表 | control-006…010、结构校验、ConfigPublisher | M0 | §5、§7、§13.3 | S1-6（部分） | 1.5 | 与 M1 并行 |
| **M3** 服务端 | 装配/TLS、两个拦截器、六 RPC、TLS 负向、集成测试 | M1、M2 | §3、§4、§6、§13.4 | S1-6；mTLS 记 S2-2 | 3.5 | 3.1→(3.2,3.3)→(3.4,3.5) |
| **M4** 部署与验收 | compose/k8s/netpol、smoke、CI 去 skip、G1 轮询延迟 | M3 | §9、§13.5 | S1-6 验收尾巴 | 1.0 | 4.1 与 4.2 并行 |

**关键路径**：T0.1 → T0.2 → T0.3 →（M1.1 ∥ M2.1→M2.2→M2.3）→ T3.1 → T3.2 → T3.3 → T3.4/T3.5 → T4.2。M1 与 M2 无共享文件，可两人并行；M3.4（TLS 负向）与 M3.5（上报 RPC）互不阻塞。

设计 §13 五步与里程碑一一对应：1→M0、2→M1、3→M2、4→M3、5→M4；每步"能挂掉"的门分别是 T0.2（生成 55 个类）/T1.1（字节向量）/T2.1（Run: 5 且幂等 Run: 0）/T3.3（16 例含 trailer）/T4.2（smoke + P95）。

## 4. 任务清单

> 路径基准：**改动文件**一栏相对仓库根 `probe/`；**门禁命令**里的宿主路径相对工作区根 `D:/repos/rust/eBPF`（即 `probe/...` 前缀），`mvncc*` 的工作目录已固定在 `probe/control-center/app`。

### 4.0 门禁执行器（一次性，pwsh；`.tmp/` 在工作区外层，不入库）

```powershell
$M='docker.1ms.run/library/maven:3.9-eclipse-temurin-21'
function mvncc { docker run --rm -v D:/repos/rust/eBPF:/ws -w /ws/probe/control-center/app -v D:/repos/rust/eBPF/.tmp/m2:/root/.m2 $M mvn -B -ntp -s /ws/.tmp/settings.xml @args }
function mvnccdb { docker run --rm --network cc-train0_default -v D:/repos/rust/eBPF:/ws -w /ws/probe/control-center/app -v D:/repos/rust/eBPF/.tmp/m2:/root/.m2 -v D:/repos/rust/eBPF/probe/control-center/.tmp/tls:/tls -e CC_ROLE=control -e CC_DB_URL=jdbc:postgresql://cc-pg:5432/control -e CC_DB_SCHEMA=control -e CC_DB_USER=cc_app -e CC_DB_PASSWORD=cc_app -e CC_MIGRATOR_USER=cc_migrator -e CC_MIGRATOR_PASSWORD=cc_migrator -e CC_IT_PG=1 -e CC_TLS_DIR=/tls -e CC_TLS_CERT=/tls/server.crt -e CC_TLS_KEY=/tls/server.key -e CC_TLS_CA=/tls/ca.crt $M mvn -B -ntp -s /ws/.tmp/settings.xml @args }
function migctl { docker run --rm --network cc-train0_default -v D:/repos/rust/eBPF:/ws -w /ws/probe/control-center/app -v D:/repos/rust/eBPF/.tmp/m2:/root/.m2 $M mvn -B -ntp -s /ws/.tmp/settings.xml liquibase:update -Dliquibase.changeLogFile=src/main/resources/db/control/changelog.sql -Dliquibase.defaultSchemaName=control "-Dliquibase.url=jdbc:postgresql://cc-pg:5432/control?currentSchema=control" -Dliquibase.username=cc_migrator -Dliquibase.password=cc_migrator }
function migrb([int]$n) { docker run --rm --network cc-train0_default -v D:/repos/rust/eBPF:/ws -w /ws/probe/control-center/app -v D:/repos/rust/eBPF/.tmp/m2:/root/.m2 $M mvn -B -ntp -s /ws/.tmp/settings.xml liquibase:rollback -Dliquibase.rollbackCount=$n -Dliquibase.changeLogFile=src/main/resources/db/control/changelog.sql -Dliquibase.defaultSchemaName=control "-Dliquibase.url=jdbc:postgresql://cc-pg:5432/control?currentSchema=control" -Dliquibase.username=cc_migrator -Dliquibase.password=cc_migrator }
function psqlc { docker run --rm --network cc-train0_default -e PGPASSWORD=cc_app cc-pg:17 psql -h cc-pg -U cc_app -d control -v ON_ERROR_STOP=1 -tAc @args }
```

Maven 中央仓慢，`.tmp/settings.xml` 写 aliyun 镜像（`<mirrorOf>central</mirrorOf>`，T0.1 创建，**不得入库**）。PG 容器 `cc-train0-cc-pg-1` 未启动时先 `docker start cc-train0-cc-pg-1` 或 `docker compose -f D:/repos/rust/eBPF/probe/control-center/compose.yaml up -d cc-pg`。

### M0 构建骨架

#### T0.1 建立容器化 Maven 门禁执行器
- **改动文件**：`.tmp/settings.xml`（工作区外层，不入库）
- **做法要点**：写入 aliyun mirror settings；把 §4.0 的 5 个函数贴进当前 pwsh 会话；验证镜像内是 Maven 3.9.16 / Java 21。
- **验收门**：`mvncc -v`；期望关键行 `Apache Maven 3.9.16` 与 `Java version: 21.0.12`。`mvncc -B -ntp validate`；期望 `BUILD SUCCESS`。
- **失败退路**：`docker images docker.1ms.run/library/maven` 确认镜像在本地；mirror 写错表现为 401/超时，临时去掉 `-s` 直连重试；`/ws` 挂载失败查 Docker Desktop 的磁盘共享。
- **依赖 / 可并行**：无 / 不能并行（所有门都靠它）。
- **预估**：0.5

#### T0.2 接入 gRPC starter 与 protobuf 代码生成
- **改动文件**：
  `control-center/app/pom.xml`
  `control-center/app/src/main/proto/control.proto`（新建，逐字节复制 `probe/agent/proto/control.proto`）
- **做法要点**：`<properties>` 加 `protobuf.version=4.36.2`、`protoc-gen-grpc-java.version=1.84.1`；deps 加 `spring-boot-starter-grpc-server`（**不写版本**，BOM 管），test scope 加 `spring-boot-starter-grpc-server-test`；build 加 `io.github.ascopes:protobuf-maven-plugin`（**不写版本**，BOM 5.1.11），configuration 为 `<protoc>${protobuf.version}</protoc>` + `<plugins><plugin kind="binary-maven">io.grpc:protoc-gen-grpc-java:${protoc-gen-grpc-java.version}</plugin></plugins>`，execution `generate`；proto 原样复制，`java_package=io.probe.control.agent.v1`、`java_multiple_files=true`、`AgentControlProto` 不改。
- **验收门**：`mvncc -DskipTests package`，期望 `BUILD SUCCESS`；`Test-Path probe/control-center/app/target/generated-sources/protobuf/io/probe/control/agent/v1/AgentControlGrpc.java` 为 `True`；`(Get-ChildItem -Recurse probe/control-center/app/target/generated-sources/protobuf -Filter *.java).Count` 为 `55`。`mvncc dependency:tree "-Dincludes=org.springframework:spring-core,org.springframework.boot:spring-boot,io.grpc"`，期望 `spring-core:jar:7.0.10`、`spring-boot:jar:4.1.2`、`grpc-netty:jar:1.84.1`。
- **失败退路**：protoc 下载失败先确认 aliyun 有 `com.google.protobuf:protoc:4.36.2:linux-x86_64.exe`；生成目录为空查 plugin 的 `<protoc>` 是否被写成路径；`grpc-* 1.84.1` 变成 1.6x 说明 parent 没升到 4.1.2。回滚：**手工**删掉本任务在 `pom.xml` 里加的那几段（`pom.xml` 已有未提交改动，禁止 `git checkout`），再删 `app/src/main/proto/`。
- **依赖 / 可并行**：T0.1 / 可与 T1.1 并行（T1.1 只依赖生成的类）。
- **预估**：0.5

#### T0.3 建立 proto 同源门并接进 CI
- **改动文件**：
  `control-center/scripts/proto-check.sh`（新建）
  `.github/workflows/control-center.yml`
- **做法要点**：脚本用 `#!/usr/bin/env bash`、`set -euo pipefail`，比较 `agent/proto/control.proto` 与 `app/src/main/proto/control.proto`，不等则打印 `control.proto drift: <a> vs <b>` + `diff -u` 并 `exit 1`（设计 §2.3 原文）；CI 三处：`cc` paths-filter 加 `agent/proto/**`、build 前加 `bash control-center/scripts/proto-check.sh`、删掉 `-DskipTests`。
- **验收门**：`bash probe/control-center/scripts/proto-check.sh`（Git Bash）退码 0 且无输出。漂移演练：`Add-Content probe/control-center/app/src/main/proto/control.proto " "` 后重跑，期望输出含 `control.proto drift:` 且 `$LASTEXITCODE=1`；复原 `Copy-Item probe/agent/proto/control.proto probe/control-center/app/src/main/proto/control.proto -Force`。
- **失败退路**：宿主没有 `cmp` 时改 `git diff --no-index --exit-code "$a" "$b"`（Git Bash 自带）；CI 里 proto 门在 checkout 后需 `agent/` 在同一仓库——已满足（同 repo）。
- **依赖 / 可并行**：T0.2 / 与 M1 并行。
- **预估**：0.5

### M1 字节与值域

#### T1.1 固化 proto 字节向量测试
- **改动文件**：
  `control-center/app/src/test/resources/proto/doc_a.textproto`（新建）
  `control-center/app/src/test/resources/proto/doc_b.textproto`（新建）
  `control-center/app/src/test/java/dev/train0/cc/control/ProtoVectorTest.java`（新建）
- **做法要点**：用 `com.google.protobuf.TextFormat.parse(ConfigDocument.newBuilder(), reader)` 读 textproto（**不手写 Java builder**，设计 §6.4）；断言 doc_a `toByteArray()` 16 进制为 `080110904e18d00f20182840`、长度 12、SHA-256 `971d3944…253c05`；doc_b 长度 63、SHA-256 `c3e4eea7…b3d30f`。
- **验收门**：`mvncc test -Dtest=ProtoVectorTest`，期望 `Tests run: 2, Failures: 0, Errors: 0`。
- **失败退路**：sha 不符先比对 textproto 字段号与顺序（3=schema_version、4=window_ms、5=grace_ms、6=default_action），**不许**改常量迁就代码。
- **依赖 / 可并行**：T0.2 / 与 M2 并行。
- **预估**：0.5

#### T1.2 实现配置文档值域校验器
- **改动文件**：
  `control-center/app/src/main/java/dev/train0/cc/control/ConfigDocumentValidator.java`（新建）
  `control-center/app/src/test/java/dev/train0/cc/control/ConfigDocumentValidatorTest.java`（新建）
- **做法要点**：`@Component`，方法 `void validate(ConfigDocument doc)`，失败抛 `AgentRpcException(INVALID_ARGUMENT, code, detail)`；规则照设计 §7 表：`schema_version==1`（`config.schema_unsupported`）、`window_ms 1000..60000`、`grace_ms 0..window_ms`、prefix 位宽 `0..32/0..128`、`default_action` 只允许 `PASS`（`config.unsupported_default_action`）、rules ≤20、`rule_index==i+1`、`rule_id` 匹配 `^[a-z0-9][a-z0-9._-]{0,31}$` 且唯一、priority 1..1000 且唯一、dst_ports 1..65535，其余用 `config.invalid_document`，detail 带字段名。
- **验收门**：`mvncc test -Dtest=ConfigDocumentValidatorTest`，期望 `Tests run: 14, Failures: 0`（1 合法 + 13 反向：schema/window/grace/v4/v6/default_action/rules 上限/rule_index/rule_id 形状/rule_id 唯一/priority 范围/priority 唯一/dst_ports）。
- **失败退路**：错误码断言失败先看是否把细分码统一成了 `config.invalid_document`；`AgentRpcException` 未定义时先建 T3.2 的异常类（可提前只建该类）。
- **依赖 / 可并行**：T0.2 / 与 M2 并行。
- **预估**：0.5

### M2 库表

#### T2.1 编写 control-006…010 changeset
- **改动文件**：`control-center/app/src/main/resources/db/control/changelog.sql`
- **做法要点**：只在文件末尾追加 5 个 `--changeset train0:control-006-agent-registry` … `control-010-config-ack`，每块配 `--rollback`；006 加 `group_id/agent_version/boot_id/applied_version/applied_at/capabilities` 并 `ALTER machine_id DROP NOT NULL`；007 `document jsonb`→`bytea` + `CREATE SEQUENCE control.config_version_seq START WITH 1`；008 建 `report_window`；009 先 `DROP TABLE control.report` 再建 `report`（`key_hash bytea CHECK octet_length=32`、复合 FK）与 `report_sample`；010 建 `config_ack`（result CHECK 三值、sha CHECK 64 hex、`degraded_features text[]`）。
- **验收门**：`migctl`，期望 `Run: 5` 与 `BUILD SUCCESS`；`psqlc "select count(*) from control.databasechangelog"` 期望 `10`。再跑一次 `migctl`，期望 `Run: 0`（幂等）。
- **失败退路**：checksum 报错说明改了已应用的 changeset——只允许追加；失败后 `migrb 5` 回滚到 005，再修 SQL 重跑；`report` 有数据导致 009 的 DROP 失败属预期（Train 0 空表），不要加 `CASCADE`。
- **依赖 / 可并行**：T0.1、PG 容器 / 与 M1 并行。
- **预估**：0.5

#### T2.2 建立库表结构校验 SQL
- **改动文件**：`control-center/scripts/db-verify.sql`（新建）
- **做法要点**：一个 `DO $$ … $$` 块做 7 项断言（`report.key_hash` 是 bytea、`report_sample` 存在、`config_ack` 存在且 PK(agent_id,version)、`report_window` PK 复合、`config_version.document` bytea、`agent.group_id` 存在且 `machine_id.is_nullable='YES'`、序列 `config_version_seq` 存在），任一不符 `RAISE EXCEPTION`，全过 `RAISE NOTICE 'db-verify: OK (7 checks)'`。
- **验收门**：`Get-Content probe/control-center/scripts/db-verify.sql -Raw | docker run -i --rm --network cc-train0_default -e PGPASSWORD=cc_app cc-pg:17 psql -h cc-pg -U cc_app -d control -v ON_ERROR_STOP=1 -f -`，期望 `NOTICE:  db-verify: OK (7 checks)` 且退码 0。
- **失败退路**：`NOTICE` 在 stderr 属正常；断言失败会打印具体列名 → 回看 T2.1 对应 changeset；`-f -` 读不到内容时改用 `-c "$(Get-Content ... -Raw)"`。
- **依赖 / 可并行**：T2.1 / 与 T2.3 串行（都要 DDL 就绪）。
- **预估**：0.5

#### T2.3 实现 ConfigPublisher（编码一次 + 审计同事务）
- **改动文件**：
  `control-center/app/src/main/java/dev/train0/cc/control/ConfigPublisher.java`（新建）
  `control-center/app/src/test/java/dev/train0/cc/control/ConfigPublisherTest.java`（新建）
- **做法要点**：`@Service`，`PublishedConfig publish(String groupId, ConfigDocument doc, String publishedBy)`；`byte[] bytes = doc.toByteArray()` 只在此处编码一次；`>65536` 抛 `config.too_large`；先 `validator.validate`；`content_sha256` 为小写 hex；`@Transactional` 内 `select nextval('control.config_version_seq')`、insert `config_version`、insert `audit_outbox(payload)` 用 `?::jsonb`，`request_id` 取 MDC 否则 UUID；返回 version/sha/bytes；无 HTTP 入口（D3）。
- **验收门**：`mvnccdb test -Dtest=ConfigPublisherTest`，期望 `Tests run: 3, Failures: 0`；测试内断言 `select count(*) from control.audit_outbox where payload->>'action'='config.publish'` 为 1。
- **失败退路**：`nextval` 不存在 → 007 未跑；`?::jsonb` 类型报错 → 参数必须传 String 而非 PGobject；`@Transactional` 不生效 → 确认方法在 Spring 代理调用路径上（测试注入 bean 而非 new）。
- **依赖 / 可并行**：T2.1；T1.2 / 与 T2.2 串行。
- **预估**：0.5

### M3 gRPC 服务端

#### T3.1 装配控制面：属性、证书脚本、服务壳
- **改动文件**：
  `control-center/app/src/main/resources/application.yml`
  `control-center/app/src/main/java/dev/train0/cc/control/ControlPlaneConfiguration.java`（新建）
  `control-center/app/src/main/java/dev/train0/cc/control/AgentControlService.java`（新建）
  `control-center/scripts/dev-certs.sh`（新建）
  `control-center/scripts/up.sh`
  `control-center/compose.yaml`
- **做法要点**：`application.yml` 加 `spring.grpc.server.port=${CC_GRPC_PORT:9090}`、`servlet.enabled=false`、`inbound.message.max-size=4MB`、`ssl.bundle=cc-agent-server`、`ssl.client-auth=require`，以及 `spring.ssl.bundle.pem.cc-agent-server.{keystore.certificate:${CC_TLS_CERT:/etc/cc/tls/server.crt},keystore.private-key:${CC_TLS_KEY:/etc/cc/tls/server.key},truststore.certificate:${CC_TLS_CA:/etc/cc/tls/ca.crt}}`——**必须带默认路径**，不能用无默认的 `${CC_TLS_CERT}`（`SslProperties` 绑定时会解析占位符，mgmt 没挂证书会直接启动失败）；`ControlPlaneConfiguration` 为 `@Configuration @ConditionalOnProperty(name="cc.role", havingValue="control")`，`AgentControlService extends AgentControlGrpc.AgentControlImplBase` 标 `@GrpcService` + 同一条件，六个方法先返回 `UNIMPLEMENTED`；`dev-certs.sh` 用 `docker run --rm -v "$ROOT/.tmp/tls:/out" $MAVEN_IMAGE` 跑 openssl 生成 `ca.crt/ca.key`、`server.crt/server.key`（SAN `DNS:localhost,IP:127.0.0.1,DNS:cc-control`）、`client.crt/client.key`（CN `agent-m1`），外加 T3.4 用的 `rogue-ca.crt/rogue-ca.key` 与 `rogue-client.crt/rogue-client.key`，全部 644 私钥不加密；`up.sh` 在 compose up 前调它（已存在则跳过）；compose 挂 `${CC_TLS_DIR:-./.tmp/tls}:/etc/cc/tls:ro` 并设 `CC_TLS_CERT=/etc/cc/tls/server.crt`、`CC_TLS_KEY=/etc/cc/tls/server.key`、`CC_TLS_CA=/etc/cc/tls/ca.crt`。
- **验收门**：先 `docker compose -f D:/repos/rust/eBPF/probe/control-center/compose.yaml up -d --build cc-mgmt cc-control`；再
  `docker compose -f D:/repos/rust/eBPF/probe/control-center/compose.yaml exec -T cc-mgmt sh -c '! grep -q ":2382" /proc/net/tcp'` → 退码 0（mgmt 无 9090）；
  `docker compose -f D:/repos/rust/eBPF/probe/control-center/compose.yaml exec -T cc-control sh -c 'grep -q ":2382" /proc/net/tcp'` → 退码 0（control 在听）；
  `curl.exe -fsS http://127.0.0.1:8082/actuator/health/liveness` → JSON 含 `"status":"UP"`。
- **失败退路**：control 起不来先看 `docker compose logs cc-control | Select-String 'ssl|bundle|PEM'`（证书路径/权限，容器用户 10001 需可读 644）；mgmt 起不来且报 `Could not resolve placeholder 'CC_TLS_CERT'`，说明写成了无默认值的占位符；mgmt 也开了 9090 说明条件失效，查 `@ConditionalOnBean(BindableService)` 是否被别的 `BindableService` 满足；端口占用于本机改 `CC_CONTROL_GRPC_PORT`。回滚：删 `application.yml` 的 `grpc:` 块并 `docker compose up -d --build`。
- **依赖 / 可并行**：T0.2 / M3 内不可并行（其余都依赖它）。
- **预估**：1.0

#### T3.2 实现两个拦截器与错误映射
- **改动文件**：
  `control-center/app/src/main/java/dev/train0/cc/control/AgentRpcException.java`（新建）
  `control-center/app/src/main/java/dev/train0/cc/control/RequestIdInterceptor.java`（新建）
  `control-center/app/src/main/java/dev/train0/cc/control/AgentIdentityInterceptor.java`（新建）
  `control-center/app/src/main/java/dev/train0/cc/control/AgentPrincipal.java`（新建）
  `control-center/app/src/main/java/dev/train0/cc/control/AgentErrorHandler.java`（新建）
  `control-center/app/src/test/java/dev/train0/cc/control/AgentIdentityTest.java`（新建）
- **做法要点**：`RequestIdInterceptor` `@Order(0)` 读 metadata `x-request-id`（缺省 UUIDv4）→ MDC `request_id`，`finally` 清理，`close()` 里补 `x-request-id` 与 `x-error-code` trailer；`AgentIdentityInterceptor` `@Order(10)` 取 `Grpc.TRANSPORT_ATTR_SSL_SESSION`，null→`UNAUTHENTICATED agent.unauthenticated`，否则从 peer 证书取 CN（`LdapName` 取第一个 CN，大小写不敏感）与 SHA-256 小写 hex，查 `control.agent`，缺失/指纹不符→`PERMISSION_DENIED agent.identity_mismatch`（同一码，不泄漏存在性），`revoked`→`FAILED_PRECONDITION agent.revoked`，`Context` 挂 `AgentPrincipal` 并在 `finally` detach；`AgentErrorHandler` 实现 `GrpcExceptionHandler`（`org.springframework.grpc.server.exception`），把 `AgentRpcException` 映射成 `StatusException` + `x-error-code`（必要时 `x-retry-after-ms`），其他 `Throwable`→`INTERNAL server.error` 且只记日志。
- **验收门**：`mvncc test -Dtest=AgentIdentityTest`，期望 `Tests run: 7, Failures: 0`（CN 含转义逗号、CN 大小写、指纹大写输入、未知 CN 与指纹不符同码、revoked、无 session）。
- **失败退路**：取不到 `SSLSession` 先确认 server 端 `client-auth` 生效；`LdapName` 解析异常用 `X500Principal.getName(X500Principal.RFC2253)` 兜底再解析；`GrpcExceptionHandler` 导错包（`...server.advice`）会编译失败。
- **依赖 / 可并行**：T3.1 / 与 T3.3 串行（3.3 复用异常类）。
- **预估**：0.5

#### T3.3 实现 Register / Heartbeat / GetConfig / AckConfig
- **改动文件**：
  `control-center/app/src/main/java/dev/train0/cc/control/AgentControlService.java`（修改）
  `control-center/app/src/main/java/dev/train0/cc/control/repo/AgentRepository.java`（新建）
  `control-center/app/src/main/java/dev/train0/cc/control/repo/ConfigRepository.java`（新建）
  `control-center/app/src/main/java/dev/train0/cc/control/repo/AckRepository.java`（新建）
  `control-center/app/src/test/java/dev/train0/cc/control/TestFixtures.java`（新建：读 `CC_TLS_DIR/client.crt` 算指纹、插/删 `control.agent` 行）
  `control-center/app/src/test/java/dev/train0/cc/control/AgentControlHappyPathTest.java`（新建）
  `control-center/app/src/test/java/dev/train0/cc/control/AgentControlErrorTest.java`（新建）
- **做法要点**：每个写 RPC 一个 `@Transactional`，事务内只做 JDBC，不跨 await；Register 先 `SELECT … FOR UPDATE`，`machine_id` 空→`request.invalid`，已有非空且不同→`agent.machine_conflict`，`UPDATE machine_id=COALESCE(machine_id,?)` 并写 version/boot_id/capabilities/last_heartbeat；Heartbeat `applied_version=GREATEST(...)`、按 `desired.version > current` 返回 1000/5000 两个间隔；GetConfig 在 `desired==null || desired.version<=current` 时 `up_to_date=true`，否则原样回填 `document` 字节；AckConfig `UNSPECIFIED`→`request.invalid`、`version>desired`→`ack.version_unknown`，`INSERT … ON CONFLICT(agent_id,version) DO UPDATE` 且 `applied_at=COALESCE(ack.applied_at, excluded.applied_at)`、`received_at` 不覆盖；`server_time` 统一 `Timestamps.fromMillis(System.currentTimeMillis())`。
- **验收门**：`mvnccdb test "-Dtest=AgentControlHappyPathTest,AgentControlErrorTest"`，期望 `Tests run: 16, Failures: 0`（HappyPath 6：首次绑定/二次幂等/lagging 1000/steady 5000/GetConfig 字节与 sha/Ack 回写 applied_version；Error 10：machine_conflict、ack.version_unknown、ack UNSPECIFIED、pending up_to_date、跨组 up_to_date、无配置、trailer 双头、>4MiB RESOURCE_EXHAUSTED、空 machine_id、revoked）。
- **失败退路**：`up_to_date` 断言失败先看 `current > desired` 的回滚分支；`ON CONFLICT` 语法错说明 010 未落；事务内 `FOR UPDATE` 与外层查询不在同一连接时锁无效（必须同一 `@Transactional`）。
- **依赖 / 可并行**：T3.2、T2.1 / 与 T3.4 并行（不同文件）。
- **预估**：1.0

#### T3.4 验证 mTLS 三条负向
- **改动文件**：`control-center/app/src/test/java/dev/train0/cc/control/MutualTlsTest.java`（新建）
- **做法要点**：`@SpringBootTest(webEnvironment=RANDOM_PORT, properties=...)` 用 `spring.grpc.server.port=0` + `@LocalGrpcServerPort` 取端口，证书读 `CC_TLS_DIR`；用 `NettyChannelBuilder` + `GrpcSslContexts.forClient()` 组三种 channel：无客户端证书、CA 不匹配（用另一张自签 CA）、合法证书；断言前两者 `Status.UNAVAILABLE`/`SSLHandshakeException`，第三者 `GetCursor` 返回 OK。
- **验收门**：`mvnccdb test -Dtest=MutualTlsTest`，期望 `Tests run: 3, Failures: 0`；日志含 `clientAuth=REQUIRE` 与握手失败行。
- **失败退路**：三种都通过说明 server 没启用 TLS（查 `ssl.bundle` 是否被 env 覆盖为空）；合法证书也失败看 SAN 是否含 `localhost` 与 `127.0.0.1`；`CC_TLS_DIR` 未挂载时测试应 `Assumptions.assumeTrue` 跳过而不是报错（本地门会先跑 `dev-certs.sh`）。
- **依赖 / 可并行**：T3.1 / 与 T3.3、T3.5 并行。
- **预估**：0.5

#### T3.5 实现 ReportWindows / GetCursor / Watch
- **改动文件**：
  `control-center/app/src/main/java/dev/train0/cc/control/AgentControlService.java`（修改）
  `control-center/app/src/main/java/dev/train0/cc/control/repo/ReportRepository.java`（新建）
  `control-center/app/src/test/java/dev/train0/cc/control/ReportIdempotencyTest.java`（新建）
  `control-center/app/src/test/java/dev/train0/cc/control/ReportCursorTest.java`（新建）
- **做法要点**：校验顺序固定——`windows>1000`→`RESOURCE_EXHAUSTED report.batch_too_large`；`window_id<=0 || window_ms<=0` 或非严格递增→`INVALID_ARGUMENT report.malformed`；游标 `SELECT COALESCE(max(window_id),0)`，`window_id+1000<cursor` 记入 `rejected_windows`（`report.window_too_old`）且不写库；`report_window`/`report`/`report_sample` 三条 `INSERT … ON CONFLICT DO NOTHING` 批量写，全部 0 行→`duplicates++` 否则 `accepted++` 且 `row_count+=inserted`；提交后 `last_window_id=max`，并保证 `accepted+duplicates+rejected==request.windows.size()`；GetCursor 无行时**不设** `last_window_id`；Watch 回 `UNIMPLEMENTED`。
- **验收门**：`mvnccdb test "-Dtest=ReportIdempotencyTest,ReportCursorTest"`，期望 `Tests run: 8, Failures: 0`（重放全 duplicates、部分重复 accepted、超 1000 拒绝、乱序 malformed、游标缺失字段）。
- **失败退路**：`accepted` 计数不对先看 batch 返回值之和（`ON CONFLICT DO NOTHING` 返回 0 行）；FK 报错说明窗口头没先插；`last_window_id` 出现 0 说明用了 `getLastWindowId()` 而非 `hasLastWindowId()`。
- **依赖 / 可并行**：T3.3（复用异常类与 repo 风格）/ 与 T3.4 并行。
- **预估**：0.5

### M4 部署与验收

#### T4.1 更新部署清单（compose / k8s / netpol / README / Dockerfile）
- **改动文件**：
  `control-center/compose.yaml`（T3.1 已改挂载，这里补注释与 healthcheck 说明）
  `control-center/deploy/k3d/k8s/40-control.yaml`
  `control-center/deploy/k3d/k8s/70-netpol.yaml`
  `control-center/deploy/images/cc-java/Dockerfile`
  `control-center/README.md`
- **做法要点**：k8s 给 control 加 `control-plane-tls` secret 卷挂到 `/etc/cc/tls` 并设三个 env（`readOnly: true`），就绪探针仍走 8080；netpol 把 `allow-control-grpc-from-agents` 的注释从"暂无服务"改为"Train 0 仅 compose 验证，k3d 的 agent 命名空间接线在 E8"；Dockerfile `EXPOSE 8080 9090`；README §2/§6 把"9090 预留、无 gRPC、document jsonb"改成已实现并注明 Watch 仍是 UNIMPLEMENTED。
- **验收门**：`kubectl kustomize probe/control-center/deploy/k3d/k8s | Select-String 'containerPort: 9090'` 有匹配；`kubectl kustomize probe/control-center/deploy/k3d/k8s | Select-String 'control-plane-tls'` 有匹配；`docker compose -f probe/control-center/compose.yaml config -q` 退码 0；`Select-String -Path probe/control-center/README.md -Pattern '还没有的' -Context 0,16` 里不再出现 "gRPC 未实现"。
- **失败退路**：kustomize 报 secret 缺失是预期的（secret 由 `kubectl create secret generic control-plane-tls --from-file=...` 在集群侧创建，不入库）；`compose config` 报插值失败查 `.env` 是否有 `CC_TLS_DIR`。
- **依赖 / 可并行**：T3.1 / 与 T4.2 并行。
- **预估**：0.5

#### T4.2 扩展 smoke 与 CI，跑 G1 轮询延迟门
- **改动文件**：
  `control-center/scripts/smoke.sh`
  `.github/workflows/control-center.yml`
  `control-center/app/src/test/java/dev/train0/cc/control/PollingLatencyTest.java`（新建）
- **做法要点**：smoke 加两条断言（mgmt 无 `:2382` 监听、control 有）；CI 已在 T0.3 去掉 `-DskipTests`，这里只给 build step 加 `timeout-minutes: 20`，并不设 `CC_IT_PG`（PG 用例按 D7 自动 skip）；`PollingLatencyTest` 在 `@EnabledIfEnvironmentVariable(named="CC_IT_PG", matches="1")` 下循环 10 次 publish→Heartbeat(lagging)→GetConfig，记录 ack 到下次拉到的时间，断言 P95 ≤ 3000ms 并打印 `g1_p95_ms=`。
- **验收门**：`bash probe/control-center/scripts/smoke.sh` 退码 0 且最后一行含 `smoke passed`；`mvnccdb test -Dtest=PollingLatencyTest` 输出含 `g1_p95_ms=` 且 `Tests run: 1, Failures: 0`。
- **失败退路**：smoke 的 `:2382` 断言在容器重启后立即跑会假阴性（等 readiness 后再跑）；延迟门抖动大时确认心跳间隔配置为 1000/5000（`cc.heartbeat.*` 未落则用常量）；CI 因 PG 缺失失败说明 `@EnabledIfEnvironmentVariable` 没生效。
- **依赖 / 可并行**：T3.3、T3.5、T4.1 / 与 T4.1 并行。
- **预估**：0.5

## 5. 关键落点

- **包结构**：`dev.train0.cc.control`（`ControlPlaneConfiguration`、`AgentControlService`、两个拦截器、`AgentPrincipal`、`AgentRpcException`、`AgentErrorHandler`、`ConfigPublisher`、`ConfigDocumentValidator`）、`dev.train0.cc.control.repo`（`AgentRepository`/`ConfigRepository`/`AckRepository`/`ReportRepository`，只用 `JdbcTemplate`/`NamedParameterJdbcTemplate`）。REST 留在 `dev.train0.cc.api`，不互相 import。
- **bean 装配与条件化**：`@GrpcService` + `@ConditionalOnProperty(name="cc.role", havingValue="control")`；拦截器用 `@Bean @GlobalServerInterceptor @Order(0|10)` 暴露（注解 target 含 METHOD，已验）；错误处理 bean 类型为 `org.springframework.grpc.server.exception.GrpcExceptionHandler`，Boot 会把 `List<GrpcExceptionHandler>` 合成 `CompositeGrpcExceptionHandler`。mgmt 不开 9090 有双保险：自身条件 + `GrpcServerAutoConfiguration` 的 `@ConditionalOnBean(BindableService)`（spike C）。
- **事务与幂等**：Register/AckConfig/ReportWindows 各自一个 `@Transactional`；Ack 用 `ON CONFLICT(agent_id,version) DO UPDATE` 且 `received_at`/`applied_at` 首写优先；Report 用三表 `ON CONFLICT DO NOTHING` + 行数判定 accepted/duplicates；ConfigPublisher 的 `nextval` + 两条 insert 在同一事务。
- **gRPC 状态码映射**：

  | 场景 | Code | `x-error-code` | `x-retry-after-ms` |
  |---|---|---|---|
  | 无客户端证书 / 无 SSL session | UNAUTHENTICATED | `agent.unauthenticated` | — |
  | CN 无行 / 指纹不符 | PERMISSION_DENIED | `agent.identity_mismatch` | — |
  | revoked | FAILED_PRECONDITION | `agent.revoked` | — |
  | machine 冲突 / ack 版本超前 | FAILED_PRECONDITION | `agent.machine_conflict` / `ack.version_unknown` | — |
  | 参数非法 / 批内乱序 | INVALID_ARGUMENT | `request.invalid` / `report.malformed` | — |
  | 窗口数 >1000 | RESOURCE_EXHAUSTED | `report.batch_too_large` | — |
  | 报文 >4MiB（传输层） | RESOURCE_EXHAUSTED | 无（Netty 直接拒） | — |
  | 连接池耗尽 | RESOURCE_EXHAUSTED | `server.busy` | 1000 |
  | 未预期异常 | INTERNAL | `server.error` | — |
  | Watch | UNIMPLEMENTED | 无 | — |

- **request_id 与审计**：`RequestIdInterceptor` 最外层（`@Order(0)`）保证 UNAUTHENTICATED 响应也带 `x-request-id`；MDC key `request_id` 已被 `application.yml:51` 的日志格式消费；审计只写 `audit_outbox`（publish 一条，`request_id` 取自 MDC），`notify_outbox` Train 0 不写。
- **Jackson 3**：任何新代码用 `tools.jackson.databind.*`，禁止 `com.fasterxml.jackson.*`；`audit_outbox.payload` 优先 `?::jsonb` 字符串，避免为一个字段引 `ObjectMapper`。
- **jsonb→bytea 读写**：写 `jdbc.update("insert … (document) values (?)", ps -> ps.setBytes(1, bytes))`；读 `rs.getBytes("document")` 直接进 `ConfigEnvelope.document`（`ByteString.copyFrom`）；`content_sha256` 库里是 64 位 hex 文本，进 proto 前 `HexFormat.of().parseHex()`；proto 里 `desired_sha256`/`content_sha256` 是 **bytes**，不是字符串。
- **protobuf 运行时 vs protoc**：运行时 `protobuf-java 4.36.2`（BOM 管），生成用 `<protoc>4.36.2</protoc>`，`protoc-gen-grpc-java:1.84.1` 与 `grpc-java` 同版本；三者任一漂移会在 `ProtoVectorTest` 或启动时 `NoSuchMethodError` 暴露。

## 6. 测试矩阵

| 测试文件 | 用例 | 运行方式 | 门 |
|---|---|---|---|
| `ProtoVectorTest` | doc_a 12B/hex、doc_b 63B、两条 sha256 | `mvncc test`（无 DB/TLS） | T1.1 |
| `ConfigDocumentValidatorTest` | 合法 1 + 13 条规则反向 | `mvncc test` | T1.2 |
| `ConfigPublisherTest` | 发布成功/超 64KiB/非法文档；审计行同事务 | `mvnccdb test`（真 PG） | T2.3 |
| `AgentIdentityTest` | CN 转义逗号、大小写、指纹规范化、未知 CN 与不符同码、revoked、无 session | `mvncc test`（mock `SSLSession`） | T3.2 |
| `AgentControlHappyPathTest` | 6 例：首次绑定/二次幂等/1000 与 5000 间隔/GetConfig 字节与 sha/Ack 回写 | `mvnccdb test`（真 TLS + 真 PG） | T3.3 |
| `AgentControlErrorTest` | 10 例：machine_conflict、ack.version_unknown、UNSPECIFIED、pending、跨组、无配置、trailer、>4MiB、空 machine_id、revoked | 同上 | T3.3 |
| `MutualTlsTest` | 无证书、CA 不匹配、合法证书 | `mvnccdb test`（真 Netty 端口） | T3.4 |
| `ReportIdempotencyTest` | 重放全 duplicates、部分重复 accepted、超 1000、乱序 | `mvnccdb test` | T3.5 |
| `ReportCursorTest` | 无行为空、max(window_id)、跨 1000 旧窗口 rejected | `mvnccdb test` | T3.5 |
| `PollingLatencyTest` | 10 次发布→拉取，P95≤3000ms | `mvnccdb test`（`CC_IT_PG=1`） | T4.2 |
| `smoke.sh` | mgmt 无 9090 / control 有 9090 / 既有 REST 断言 | 宿主 `bash` + compose | T4.2 |

**负向用例单列**：重放（重复 AckConfig 与整窗重发 → duplicates，不报错）；过期（`ack.version_unknown`、`report.window_too_old`）；跨组（g1 agent 拿不到 g2 的 config，只能 `up_to_date`）；直连（不带客户端证书连 9090 → 握手失败，非 200/OK）；超限（构造 >4MiB 报文 → 客户端收 `RESOURCE_EXHAUSTED`，无 `x-error-code`）。

**环境外门**：k3d netpol "agent 直连 control 被拒"（需要 k3d 集群 + agent 命名空间，E8 才接线）与真 `probe-agent` E2E；替代门 = `MutualTlsTest` 三条 + compose 9090 监听断言 + `kubectl kustomize` 静态渲染。

## 7. 迁移剧本（control-006…010）

前置：`docker start cc-train0-cc-pg-1`（或 `docker compose -f probe/control-center/compose.yaml up -d cc-pg`）。当前基线 `psqlc "select count(*) from control.databasechangelog"` = **5**（2026-11-18 实测）。

全量：`migctl` → `Run: 5`，计数 **10**；再 `migctl` → `Run: 0`（幂等）。逐条演练在**全量之后**做：`migrb 1` 回退最新一条 → `psqlc` 计数 → `migctl` 前滚 → 期望 `Run: 1` 且计数回到 10。

| changeset | down 后计数 | down 校验点 |
|---|---|---|
| control-010 | 9 | `config_ack` 表消失 |
| control-009 | 8 | `report` 回 jsonb 形态、`report_sample` 消失 |
| control-008 | 7 | `report_window` 消失 |
| control-007 | 6 | `document` 回 jsonb、`config_version_seq` 消失 |
| control-006 | 5 | `group_id`/`capabilities` 消失、`machine_id` 回 NOT NULL |

全回退：`migrb 5` → 计数 5；再 `migctl` → `Run: 5`、计数 10。**007/009 的 down 只在空库成立**（有行时 jsonb NOT NULL 无法回填），Train 0 空库；有数据时按设计 §5.2 拆成"加列→离线回填→删列"，不在迁移里编码。

## 8. 门禁总表

| 任务 | 命令 | 期望 | 耗时 |
|---|---|---|---|
| T0.1 | `mvncc -v` / `mvncc validate` | `Maven 3.9.16` + `BUILD SUCCESS` | 1 min |
| T0.2 | `mvncc -DskipTests package`；计数生成文件 | `BUILD SUCCESS`；55 个 java | 3–6 min（首次下载） |
| T0.3 | `bash probe/control-center/scripts/proto-check.sh`；改 1 字节后重跑 | 0；`control.proto drift:` + 1 | 10 s |
| T1.1 | `mvncc test -Dtest=ProtoVectorTest` | `Tests run: 2` | 1 min |
| T1.2 | `mvncc test -Dtest=ConfigDocumentValidatorTest` | `Tests run: 14` | 1 min |
| T2.1 | `migctl` ×2；`psqlc "select count(*)…"` | `Run: 5`/`Run: 0`；`10` | 1–2 min |
| T2.2 | `psql -f - < scripts/db-verify.sql` | `db-verify: OK (7 checks)` | 10 s |
| T2.3 | `mvnccdb test -Dtest=ConfigPublisherTest` | `Tests run: 3` | 2 min |
| T3.1 | compose up + `:2382` 断言 ×2 | mgmt 无 / control 有 | 3–6 min |
| T3.2 | `mvncc test -Dtest=AgentIdentityTest` | `Tests run: 7` | 1 min |
| T3.3 | `mvnccdb test -Dtest=...HappyPath,...Error` | `Tests run: 16` | 3 min |
| T3.4 | `mvnccdb test -Dtest=MutualTlsTest` | `Tests run: 3` | 2 min |
| T3.5 | `mvnccdb test -Dtest=...Idempotency,...Cursor` | `Tests run: 8` | 2 min |
| T4.1 | `kubectl kustomize …`；`compose config -q` | 含 `9090`/`control-plane-tls`；退码 0 | 10 s |
| T4.2 | `bash probe/control-center/scripts/smoke.sh`；`mvnccdb test -Dtest=PollingLatencyTest` | `smoke passed`；`g1_p95_ms=` ≤3000 | 3 min |

## 9. 风险与 spike 结论

- **S-A 依赖版本（实跑）**：`spring-boot-starter-parent:4.1.2` + `spring-boot-starter-grpc-server`，`dependency:tree` 得 spring-core 7.0.10、spring-boot 4.1.2、grpc-netty 1.84.1、spring-grpc-core 1.1.2、protobuf-java 4.36.2。BOM 另管 `io.github.ascopes:protobuf-maven-plugin` = **5.1.11**（不是设计写的 2.2.0）。
- **S-B 属性名（jar metadata 实读）**：`spring.grpc.server.{enabled,port,address,factory.enabled,servlet.enabled,inbound.message.max-size,inbound.metadata.max-size,ssl.bundle,ssl.client-auth,ssl.enabled,ssl.secure,reflection.enabled,health.enabled,keepalive.*,netty.transport,shutdown.grace-period}`；`servlet.enabled` 默认 **false**，`inbound.message.max-size` 默认 **4194304B**，`ssl.client-auth` 默认 **none**，`reflection.enabled` 默认 **true**（生产要关）。
- **S-C 角色门（字节码实读）**：`GrpcServerAutoConfiguration` 带 `@ConditionalOnBean(BindableService)`，mgmt 没有该 bean 时整个自动配置退避，9090 不会开。所以 mgmt 安全不依赖我们的 `@ConditionalOnProperty`，但两者都保留。
- **S-D 异常链（实读）**：Boot 的 `grpcGlobalExceptionHandlerInterceptor(List<GrpcExceptionHandler>)` 用 `CompositeGrpcExceptionHandler` 聚合；接口是 `org.springframework.grpc.server.exception.GrpcExceptionHandler#handleException(Throwable) → StatusException`。`@GlobalServerInterceptor` 的 `@Target` 含 `METHOD`。
- **S-E protoc 生成（实跑）**：5.1.11 的参数是 `<protoc>`（版本号）+ `<plugins><plugin kind="binary-maven">`，没有 2.x 的 `<protocVersion>/<protocPlugin>`；生成成功，`AgentControlGrpc.AgentControlImplBase` 方法名 `register/heartbeat/getConfig/ackConfig/reportWindows/getCursor/watch`，产物落在 `target/generated-sources/protobuf/io/probe/control/agent/v1/`（**无 `/java` 段**）。
- **S-F 测试支持**：`spring-boot-starter-grpc-server-test:4.1.2` 存在，注解是 `@AutoConfigureTestGrpcTransport` 与 `@LocalGrpcServerPort`——设计 §10 的 `@AutoConfigureInProcessTransport` 在 4.1.2 **不存在**。
- **S-G PEM 属性**：`spring.ssl.bundle.pem.<name>.keystore.{certificate,private-key,private-key-password,verify-keys}`、`.truststore.certificate`（`PemSslBundleProperties$Store` 字段实读）。
- **S-H Jackson 3**：`jackson-databind-3.1.7.jar` 里是 `tools/jackson/databind/*`，没有 `com.fasterxml`。
- **风险 R1**：Dockerfile 构建用 Maven Central（无 aliyun 镜像），首次加 gRPC 依赖可能 3–8 分钟；`/root/.m2` 是 BuildKit cache mount，第二次起快。**R2**：测试证书由 `openssl`（maven 镜像内 3.0.13）生成，Windows 宿主不直接跑 openssl。**R3**：`PollingLatencyTest` 的 P95 在冷启动首次会偏高，测试先 warmup 1 次。**R4**：`agent.machine_id` 可空后，Register 的 `COALESCE` 是唯一写入点，任何别处 UPDATE 都会绕过冲突检查。

## 10. 设计文档勘误

1. **§2.2 插件坐标/配置形态过时**：`io.github.ascopes:protobuf-maven-plugin` 在 Boot 4.1.2 BOM 里是 **5.1.11**，配置是 `<protoc>${protobuf.version}</protoc>` + `<plugins><plugin kind="binary-maven">…`，且 **不写 `<version>`**；设计里的 `2.2.0` + `<protocPlugin>` 写法在 5.x 编译不过（spike E）。
2. **§10 `@AutoConfigureInProcessTransport` 不存在**：4.1.2 是 `@AutoConfigureTestGrpcTransport`（spike F）；本计划测试统一走真端口（`port=0` + `@LocalGrpcServerPort`），TLS 与 4MiB 上限才是真的。
3. **§13 第 1 步的门不可直接复制**：`mvn -q -B -DskipTests package` 在宿主跑不了（JDK 17、无 mvn 21 环境），必须走 `docker run … maven:3.9-eclipse-temurin-21`；`cmp` 在纯 pwsh 也没有，proto 门要么 Git Bash 要么容器内。
4. **§5.2 的 007 回滚只在空表成立**（设计已注明），但 §13 第 3 步"回滚路径手跑一遍"没写前提；本计划把它限定在空库并在 §7 写明。
5. **§3 未写明 mgmt 安全的来源**：真正兜底的是 `@ConditionalOnBean(BindableService)`（spike C），不是我们的条件注解；建议设计补一句，避免以后有人把服务改成非 bean 注册。
6. **T3.1 的 yml 不能平铺在默认文档里**：Boot 4 的 `SslMetricsAutoConfiguration`/`SslMeterBinder` 会在启动时**急切物化所有已配置的 SSL bundle**，于是没有挂证书目录的 mgmt 进程也会因 `FileNotFoundException: /etc/cc/tls/server.crt` 启动失败（实测 `cc-mgmt` exit 1）。落地做法：`spring.grpc.*` 与 `spring.ssl.bundle.*` 放进 `on-profile: control` 的文档，并令 `spring.profiles.active: ${CC_ROLE:mgmt}`，让角色名同时充当 profile——只有 control 读证书，mgmt 连 bundle 都不注册。
7. **§4.2 的 `attach()` + `finally detach()` 写法在拦截器里不成立**：`Context` 的 attach 只作用于当前线程，gRPC 在 `onHalfClose` 的线程上下文里读不到，服务端会报 `agent identity interceptor did not run`。必须用 `io.grpc.Contexts.interceptCall(context, call, headers, next)`（库内部做 attach/detach）。

## 11. 待拍板项

D1（基线 Boot 4.1.2）已在设计 §12 定案，不再列。其余每项都给默认建议与不决策的后果；**不拍板就按"建议"执行**。

| # | 问题 | 建议 | 不拍板的后果 |
|---|---|---|---|
| D2 | `control.agent.group_id` 谁写？ | 准入/签发证书时写入，控制面永不读 mgmt 库（设计 §12-D2） | Register 无组可回，GetConfig 只能空；越权读 mgmt 破坏"两库分离" |
| D3 | Train 0 的发布入口？ | 只留 `ConfigPublisher` bean + 测试 fixture；HTTP 发布 API 归 S1-3 | 要么现在做未鉴权 8082 发布接口（安全洞），要么 §13 第 5 步无法造 10 次发布 |
| D4 | report 是否拆三表、`key_hash` 是否 bytea(32)？ | 按设计：`report_window`/`report`/`report_sample`，`bytea CHECK octet_length=32` | 保持旧 `payload jsonb` 则无法按 key 去重/游标查询，E8 回归要重做表 |
| D5 | `pending` agent 能不能拉配置？ | 允许 Register/Heartbeat/GetCursor/ReportWindows；GetConfig 恒 `up_to_date=true`（设计 §12-D5） | 二选一都行，但不写死会出现"pending 拿到配置却不许执行"的中间态，验收剧本对不上 |
| D6 | `machine_id` 冲突怎么处理？ | 保留 `agent.machine_conflict`，要求显式 `PROBE_AGENT_MACHINE_ID`（默认 HOSTNAME=Pod 名） | 静默覆盖会让同一 agent 在两台机上来回跳，reported_cursor 错乱 |
| D7 | CI 是否跑真 PG 集成测试？ | Train 0 不跑：PG 用例用 `@EnabledIfEnvironmentVariable(CC_IT_PG)` 跳过，门在本地容器执行 | DoD "CI 覆盖 integration" 留缺口，需在 S1-6 追加 0.5 人日加 `services: postgres` + initdb |
| D8 | 测试证书从哪来？ | `scripts/dev-certs.sh`（容器内 openssl）→ `control-center/.tmp/tls`，测试读文件、缺文件则 skip；不引 BouncyCastle | 引 BC 会多一个 8MB 依赖并让 CI 变慢；不生成则 `MutualTlsTest` 永远 skip，mTLS 无门 |
| D9 | Watch 的错误码？ | `UNIMPLEMENTED`（设计 §6.8），不给 `x-retry-after-ms` | 返回 `UNAVAILABLE` 会让 agent 进入重试风暴（契约 L4） |
| D10 | `proto-check.sh` 在宿主怎么跑？ | 门禁统一在容器/Git Bash 里跑；宿主 pwsh 用 `docker run … bash -lc` 包一层 | 只写 `bash` 会在没有 Git Bash 的机器上直接失败，门等于没有 |

## 12. 工作量汇总

| 里程碑 | 任务 | 人日 |
|---|---|---|
| M0 | T0.1 / T0.2 / T0.3 | 1.5 |
| M1 | T1.1 / T1.2 | 1.0 |
| M2 | T2.1 / T2.2 / T2.3 | 1.5 |
| M3 | T3.1 / T3.2 / T3.3 / T3.4 / T3.5 | 3.5 |
| M4 | T4.1 / T4.2 | 1.0 |
| **合计** | 15 个任务 | **8.5** |

对齐 backlog：S1-6 记 **7.5**（M0 1.5 + M1 1.0 + M2 1.5 + T3.1 1.0 + T3.2 0.5 + T3.3 1.0 + T3.5 0.5 + T4.1 0.5），S2-2 记 **0.5**（T3.4 的 mTLS 负向），新建 **0.5**（T4.2 的 G1 轮询门）。S1-6 比 5 人日预算多 2.5：测试矩阵 1.0、部署清单 0.5、门禁/CI 0.5、缓冲 0.5。若要压回 5 人日，按序砍：PollingLatencyTest（0.5）→ MutualTlsTest（0.5）→ 反向用例减半（0.5）→ 部署清单并入 T3.1（0.5），其余不可省。
