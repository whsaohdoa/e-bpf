# 控制面 gRPC 服务端：实现细节（契约 §13.2 落地稿）

本文把 `docs/Control-Agent-API-v1.md`（下称「契约」）§13.2「Spring 侧待做」的六条，展开成可以照单开工的
决定：依赖版本、角色装配、mTLS 与身份、每个 RPC 的事务与 SQL、库表变更、测试矩阵、以及契约与现状之间
**必须现在拍板的偏差**。契约本身不改；本文只解释「怎么实现」和「哪些地方契约写不到、按什么口径落」。

阅读顺序建议：§1 结论与偏差 → §2 依赖 → §3 角色装配 → §5 数据模型 → §6 RPC → §11 测试 → §12 待定项。

---

## 1. 结论速览

1. **gRPC 技术路线不变**，基线先升到 **Spring Boot 4.1.2**（Framework 7.0.10 / Tomcat 11 / Jackson 3 /
   Liquibase 5）：Boot 3.5 的 OSS 支持已于 2026-06-30 结束，而 **4.1 起 gRPC 服务端自动配置已并入 Boot 自身**
   （`spring-boot-starter-grpc-server`，`spring-grpc` 版本由 Boot BOM 管到 1.1.2）。所以既不引
   `spring-grpc-spring-boot-starter`（它停在 1.0.3，1.1.x 之后没有该 starter），也不钉 0.12.0。
   升级本身只有三处改动，且已在本分支实测通过，见 §11-1。
2. 契约 §13.2 写的是 **Gradle** protobuf 插件，本工程是 **Maven**：换成 `io.github.ascopes:protobuf-maven-plugin`
   （自带 protoc 平台分类器，不需要 `os-maven-plugin`；备选 `org.xolstice:protobuf-maven-plugin:0.6.1`）。
3. 9090 只由 `cc.role=control` 的实例开；mgmt 实例不注册任何 `BindableService`，因此 spring-grpc 不建 server。
   必须在 `application.yml` 显式写 `spring.grpc.server.servlet.enabled=false`（本工程有 `starter-web`，
   不写就可能被 servlet 模式接管，违反契约 §1「不用 servlet 模式」）。
4. TLS 走 **属性式**：`spring.grpc.server.ssl.bundle` + `client-auth: require`，bundle 用
   `spring.ssl.bundle.pem.*`（PEM 证书链 / 私钥 / 客户端 CA），不再手写 `GrpcSslContexts.forServer(...)`。
   4 MiB 入站上限同样有属性（`spring.grpc.server.inbound.message.max-size`，默认就是 4 MiB，仍显式写死）。
5. 库表要动的地方比 §13.2 写的多：`document jsonb→bytea`（契约要求）、`report` 表结构（jsonb 存不下
   `key_hash` 二进制，也存不下 0 行的窗口 → 游标会丢）、`control.agent` 缺 `group_id`（注册报文里没有
   组字段，控制面又不许读 mgmt 库）、`machine_id NOT NULL` 与「首次绑定」冲突。见 §5。
6. **Watch 保持 `UNIMPLEMENTED`**，因此 `notify_outbox` 在 Train 0 不写入；真控制面的 G1 只能按契约 §9 的
   「纯轮询」口径量（落后态 ~1s 心跳、P95 ≤ 3s），不得引用 mock 的 22ms。
7. 六个 RPC 的行为要与 `agent/src/mock/mod.rs` **逐条对齐**（它是契约 §5 的可执行参照）；不一致时以契约
   正文为准，并把差异写进 §12。

---

## 2. 依赖与构建

### 2.1 `app/pom.xml`

基线已是 **Spring Boot 4.1.2**（§11-1 记录了这次升级的三处改动与实测结果）。版本全部由 parent 管，
工程里不写显式 `<version>`：

```xml
<dependencies>
  <!-- 已有：starter-web / starter-jdbc / starter-actuator / starter-liquibase / postgresql / starter-test -->
  <dependency>
    <groupId>org.springframework.boot</groupId>
    <artifactId>spring-boot-starter-grpc-server</artifactId>
  </dependency>
  <!-- 进程内传输，供 @SpringBootTest 用；不带进运行时镜像 -->
  <dependency>
    <groupId>org.springframework.boot</groupId>
    <artifactId>spring-boot-starter-grpc-server-test</artifactId>
    <scope>test</scope>
  </dependency>
</dependencies>
```

Boot 4.1.2 BOM 实际带入的传递依赖（`dependency:tree` 实测，**不要**再 import `spring-grpc-dependencies`）：

| 传递依赖 | 版本 | 用在哪 |
|---|---|---|
| `org.springframework.grpc:spring-grpc-core` | 1.1.2 | 拦截器与异常处理：`org.springframework.grpc.server.GlobalServerInterceptor`、`org.springframework.grpc.server.exception.GrpcExceptionHandler` |
| `io.grpc:grpc-*`（netty / stub / services） | 1.84.1 | 传输与生成代码运行时 |
| `com.google.protobuf:protobuf-java` | 4.36.2 | **protoc 必须同版**，见 §2.2 |

反射与 health 不再需要手写 `grpc-services` 依赖，Boot 有属性：`spring.grpc.server.reflection.enabled`
（默认 **true**，生产必须关）、`spring.grpc.server.health.enabled`。

校验点（本分支第一条门禁，先跑通再写代码）：

```bash
cd control-center/app
mvn -q -B dependency:tree -Dincludes=org.springframework:spring-core,org.springframework.boot,io.grpc
# 期望 spring-core 7.0.10、spring-boot 4.1.2、grpc-* 1.84.1；出现 6.2.x 说明 parent 没升上去
```

### 2.2 proto 生成

```xml
<properties>
  <!-- 与 Boot 4.1.2 管的 protobuf-java / grpc-java 同版，见 §2.1 表 -->
  <protobuf.version>4.36.2</protobuf.version>
  <protoc-gen-grpc-java.version>1.84.1</protoc-gen-grpc-java.version>
</properties>

<plugin>
  <groupId>io.github.ascopes</groupId>
  <artifactId>protobuf-maven-plugin</artifactId>
  <version>2.2.0</version>
  <configuration>
    <protoc>${protobuf.version}</protoc>
    <protocPlugins>
      <protocPlugin>
        <id>grpc-java</id>
        <groupId>io.grpc</groupId>
        <artifactId>protoc-gen-grpc-java</artifactId>
        <version>${protoc-gen-grpc-java.version}</version>
        <mainClass>io.grpc.protoc-gen-grpc-java</mainClass>
      </protocPlugin>
    </protocPlugins>
  </configuration>
  <executions>
    <execution>
      <goals><goal>generate</goal></goals>
    </execution>
  </executions>
</plugin>
```

- 源文件落在 `app/src/main/proto/control.proto`，**与 `agent/proto/control.proto` 逐字节相同**。
  唯一来源仍是 Rust 侧那份；同步方式：`cp agent/proto/control.proto control-center/app/src/main/proto/`。
- 生成物包名由 proto 的 `java_package` 决定：`io.probe.control.agent.v1`（`java_multiple_files=true`），
  产出 `AgentControlGrpc`、`RegisterRequest` … `WatchRequest`。`java_outer_classname` 在 multiple_files 下
  只用于 descriptor 容器，不要引它。
- 编译期禁用 `-Werror` 类严格检查前，先确认生成代码无警告噪音；protoc 必须与 `protobuf-java` 同版
  （4.36.2），且满足契约附录 A 的下限 3.21.12。
- `liquibase-maven-plugin` 在 Boot 4 下解析到 **Liquibase 5.0.4**（`${liquibase.version}` 仍由 BOM 提供）。
  5.x 只给 DDL 加 `defaultSchemaName` 前缀，记账 SQL 发的是**不带 schema 限定**的
  `SELECT MAX(ORDEREXECUTED) FROM databasechangelog`，因此在 `search_path = "$user", public` 的
  migrator 上会硬报 `relation "databasechangelog" does not exist`（4.31.1 是「当没跑过」静默 exit 0）。
  `scripts/migrate.sh` 的 JDBC URL 因此带 `?currentSchema=<db>`；`liquibase.liquibaseSchemaName` 在
  5.0 的 mojo 里也已改名 `changelogSchemaName`，单独加它并不能修这个错。实测见 §13-1。

### 2.3 proto 同源门禁

新增 `control-center/scripts/proto-check.sh`（CI 与本地共用）：

```bash
#!/usr/bin/env bash
set -euo pipefail
a="$(dirname "$0")/../../agent/proto/control.proto"
b="$(dirname "$0")/../app/src/main/proto/control.proto"
cmp -s "$a" "$b" || { echo "control.proto drift: $a vs $b" >&2; diff -u "$a" "$b" >&2 || true; exit 1; }
```

`.github/workflows/control-center.yml` 在 `mvn package` 之前加一步 `bash control-center/scripts/proto-check.sh`。
理由：两侧生成类型靠字段号对齐，proto 漂移必须在 CI 挂，不能等联调。

同一个 workflow 还有两处必须改，否则上面的门禁形同虚设：

1. `changes` 的 paths-filter 现在只有 `control-center/**` 与本文件——**只改 `agent/proto/control.proto`
   不会触发本 workflow**。加一条 `agent/proto/**`（它会连带跑一次 cc 构建，这是可接受的代价）。
2. `build` 里的 `mvn -B -ntp -DskipTests package` 要去掉 `-DskipTests`：本分支开始有 `src/test`，
   注释里「等 S1-2 补上再把它去掉」指的就是这一步。

---

## 3. 角色装配与端口

| 关注点 | 决定 |
|---|---|
| 何时起 server | 只有存在 `BindableService` bean 才起；Service 类挂 `@ConditionalOnProperty(name="cc.role", havingValue="control")` |
| 端口 | `spring.grpc.server.port: ${CC_GRPC_PORT:9090}`，只写在 `application.yml`（mgmt 无 service → 不监听） |
| servlet 模式 | `spring.grpc.server.servlet.enabled: false`（显式关，防未来有人加 `grpc-servlet-jakarta`） |
| 最大入站 | `spring.grpc.server.inbound.message.max-size: 4MB`（契约 §2 要求显式 4 MiB；Boot 默认值也是 4 MB，仍写死） |
| TLS | 属性式，`spring.grpc.server.ssl.bundle` + `client-auth: require`，见 §4.1 |
| 8080 角色 | 不变：actuator health 仍在 8080，readiness/liveness 沿用 HTTP（契约 §13.3） |

装配类（名字按仓库风格，包 `dev.train0.cc.control`）：

```java
@Configuration
@ConditionalOnProperty(name = "cc.role", havingValue = "control")
class ControlPlaneConfiguration {
    @Bean @GlobalServerInterceptor @Order(0)  ServerInterceptor requestIdInterceptor() { ... }
    @Bean @GlobalServerInterceptor @Order(10) ServerInterceptor agentIdentityInterceptor(...) { ... }
    @Bean GrpcExceptionHandler agentErrorHandler() { ... }
}
```

`AgentControlService extends AgentControlGrpc.AgentControlImplBase` 同样带 `@ConditionalOnProperty`。

**验证 mgmt 实例不监听 9090**（写进 `scripts/smoke.sh`）：`compose up` 后对 cc-mgmt 执行
`nc -z 127.0.0.1 9090` 必须失败；对 cc-control 必须成功。若 Boot 4.1 的自动配置在无 service 时仍起
server（`spring.grpc.server.enabled` 默认未关），退路是把 `spring.grpc.server.*` 挪进
`application-control.yml`，并在 compose/k8s 给 control 容器加 `SPRING_PROFILES_ACTIVE=control`
（不引入新的 `cc.role` 语义，只是分文件）；最后的硬办法是 `spring.grpc.server.enabled: false` 由 profile 打开。

---

## 4. mTLS、身份与错误码

### 4.1 服务端 TLS（属性式：PEM bundle + `client-auth: require`）

Boot 4.1 把 `spring.grpc.server.ssl.*` 收进自身（`org.springframework.boot.grpc.server.autoconfigure`），
TLS 与 mTLS 全部属性化，不再手写 `GrpcSslContexts.forServer(...)`：

```yaml
spring:
  ssl:
    bundle:
      pem:
        cc-agent-server:
          keystore:
            certificate: ${CC_TLS_CERT}      # 服务器证书链（PEM）
            private-key: ${CC_TLS_KEY}       # 私钥（PEM，PKCS#8）
          truststore:
            certificate: ${CC_TLS_CA}        # 签发 agent 证书的 CA
  grpc:
    server:
      port: ${CC_GRPC_PORT:9090}
      ssl:
        bundle: cc-agent-server
        client-auth: require                 # io.grpc.TlsServerCredentials$ClientAuth，默认 none
```

- 属性名以 Boot 4.1.2 的 `spring-configuration-metadata.json` 为准：`ssl.bundle` / `ssl.client-auth`
  （类型就是 grpc 的 `ClientAuth`，取值 `none|optional|require`）/ `ssl.enabled` / `ssl.secure`；
  PEM bundle 的字段来自 `PemSslBundleProperties`（`keystore`/`truststore` 各含
  `certificate`、`private-key`、`private-key-password`、`verify-keys`）。
- 只接受 PEM：agent 证书与 CA 由 rcgen / `step-ca` 生成，JKS 会在运维侧凭空多一步转换。
- **不设 `secure: false`**（那是给「证书已在部署层终止」的例外留的门）；ALPN `h2` 由 Netty TLS 通道提供，
  不需要 `ApplicationProtocolConfig`。
- **不热加载**：换证书 = 重启容器（Train 0 写进限制；定时 reload 会在 G1 测量窗口里抖）。轮换顺序写进
  README：先换 `control.agent.cert_fingerprint`，再滚 agent，最后滚控制面证书。
- 客户端用 `tonic` + `rustls`，`domain_name` 需要匹配；运维必须保证 server cert 的 SAN 含 agent 使用的
  endpoint 主机名（`agent/src/client/tonic.rs` 的 `TlsConfig.domain_name`），否则 rustls 报
  `InvalidCertificate(NotValidForName)`，而 grpc-java 侧只看到连接被拒。
- 门禁（`MutualTlsTest` 与 `scripts/smoke.sh` 各一份）：`openssl s_client -alpn h2 -connect host:9090`
  无客户端证书时必须握手失败；带 `-cert/-key` 时成功且协商到 `h2`。这同时验证了 ALPN 与 REQUIRE 两件事。
- 真的需要调密码套件/协议版本时再回到 `ServerBuilderCustomizer<NettyServerBuilder>`；但不要与
  `spring.grpc.server.ssl.*` 同时设置 sslContext —— 两者会互相覆盖。
- 测试证书由测试自己生成（Java `KeyPairGenerator` 自签或 `rcgen` 固定夹具）；**私钥不进仓库**。

### 4.2 `AgentIdentityInterceptor`（`@Order(10)`）

```
SSLSession ssl = call.getAttributes().get(Grpc.TRANSPORT_ATTR_SSL_SESSION);
if (ssl == null)                      -> UNAUTHENTICATED  agent.unauthenticated
X509Certificate peer = ssl.getPeerCertificates()[0];
cn            = first RDN of type CN   (LdapName#getRdns, 大小写不敏感, 处理转义逗号)
fingerprint   = HexFormat.of().formatHex(SHA-256(peer.getEncoded()))   // 小写 hex，64 字符
row = SELECT * FROM control.agent WHERE agent_id = cn
row == null || row.cert_fingerprint != fingerprint -> PERMISSION_DENIED agent.identity_mismatch
row.status == 'revoked'               -> FAILED_PRECONDITION agent.revoked
Context 放 AgentPrincipal(agentId, fingerprint, groupId, status)，finally 里 detach
```

要点：

- `agent_id` **只**来自证书 CN；报文里没有该字段，任何「报文声明身份」都要被忽略（契约 §1）。
- 指纹比对用**握手证书重新算**的值，不读任何 header（契约明说 agent 不上报）。
- 指纹不符与 CN 查不到**同码** `agent.identity_mismatch`：不泄露「这个 CN 存在但指纹错」。代价是轮换
  只能「先改库再重启 agent」，运维步骤写进 README：`UPDATE control.agent SET cert_fingerprint=... WHERE
  agent_id=...` + 重启 agent；Train 0 无 CRL/OCSP（契约 L5）。
- `pending` 语义契约没定；本文口径（§12-D5）：`pending` 允许 Register/Heartbeat/GetCursor/ReportWindows，
  GetConfig 一律回 `up_to_date=true`（等价无配置），不做拒绝——否则设备会卡死在「注册成功但拿不到配置」。
- 拦截器只做认证，不做授权；吊销后的 `agent.revoked` 对所有 RPC 生效（含 Register），契约 §1。

### 4.3 `RequestIdInterceptor`（`@Order(0)`，最外层）

- 读 metadata `x-request-id`（`Metadata.Key.of("x-request-id", ASCII_STRING_MARSHALLER)`）；缺失则生成 UUIDv4。
- 放进 MDC `request_id`（`application.yml` 的 pattern 已是 `%X{request_id:-}`，键名必须一致），finally 清理。
- 包装 `ServerCall`，在 `close(status, trailers)` 里把 `x-request-id` **和** `x-error-code` 写进 trailers：
  错误路径必须也带 request id，否则 agent 侧日志对不上。
- 放最外层的原因：身份校验失败（`agent.identity_mismatch`）也要回显 request id。

### 4.4 错误映射

```java
final class AgentRpcException extends RuntimeException {
    final Status.Code code; final String errorCode; final String detail; final Integer retryAfterMs;
}
```

`GrpcExceptionHandler` 把 `AgentRpcException` 转成 `StatusException`，trailers 带 `x-error-code`
（必要时 `x-retry-after-ms`）；`description` 只放人读的一句话，不带 SQL、不带堆栈。
非 `AgentRpcException` 的 `RuntimeException` 一律 `INTERNAL` + `server.error`，原始异常只进日志。

`RESOURCE_EXHAUSTED` 何时带 `x-retry-after-ms`：`server.busy`（数据库连接池耗尽、`connection-timeout=3000`
超时）带 1000；`report.batch_too_large` / `request.too_large` 不带（拆批就能过，退避没意义）。

---

## 5. 数据模型变更（Liquibase，全部追加在 `app/src/main/resources/db/control/changelog.sql`）

现库是空库（无 agent、无 config_version 行），但变更集仍写成可回滚、注释写清「为什么」。控制库与 mgmt 库
**不同库、不同角色**，不要写跨库 SQL。

### 5.1 `control-006-agent-enrollment`

```sql
--changeset train0:control-006-agent-enrollment
ALTER TABLE control.agent ALTER COLUMN machine_id DROP NOT NULL;   -- 首次 Register 才绑定
ALTER TABLE control.agent
    ADD COLUMN group_id        text,        -- 组归属，注册时不可改；见 §12-D2
    ADD COLUMN agent_version   text,
    ADD COLUMN boot_id         text,
    ADD COLUMN config_state    text,        -- MISMATCH 等：只做观测，不参与判定
    ADD COLUMN capabilities    jsonb,       -- 原样存 RegisterRequest.capabilities
    ADD COLUMN applied_version integer,
    ADD COLUMN applied_at      timestamptz,
    ADD COLUMN registered_at   timestamptz;
--rollback ALTER TABLE control.agent DROP COLUMN group_id, ... ;
```

理由：

- 契约 §5 要求「machine_id 首次绑定后不可变」。`machine_id NOT NULL` + 无种子行 = 永远注册不上，
  所以列先放开，Register 里 `machine_id = COALESCE(machine_id, ?)`，之后不一致才 `agent.machine_conflict`。
- `group_id` 是 `desired_version` 的唯一输入（`WHERE group_id=?`），而 `RegisterRequest` 没有组字段，
  控制面又不许读 mgmt 库（`CcServiceApplication` 的注释）→ 只能在**发证/准入**时写进 `control.agent`。
- `capabilities` 落库是为了 Heartbeat 不必再带能力、以及审计「这台机器声明了什么」。

### 5.2 `control-007-config-document-bytes`

```sql
--changeset train0:control-007-config-document-bytes
-- Contract §6.4: the control plane encodes once and stores the bytes; reads never re-derive them.
ALTER TABLE control.config_version DROP COLUMN document;
ALTER TABLE control.config_version ADD COLUMN document bytea NOT NULL;
CREATE SEQUENCE control.config_version_seq START WITH 1;
--rollback DROP SEQUENCE control.config_version_seq;
--rollback ALTER TABLE control.config_version DROP COLUMN document;
--rollback ALTER TABLE control.config_version ADD COLUMN document jsonb NOT NULL;
```

- 顺序很重要：**先编码、先算 sha，再落库**；读路径 `SELECT document` 直接塞进
  `ConfigEnvelope.document`，不重新序列化（契约 §6.4「职责不对称」）。
- 版本号用序列而不用 `max+1`：发布是并发写，序列天然全局、天然允许组间跳号（契约 §6 明确接受 7→12）。
- 空库才允许 `DROP COLUMN`；若届时已有行，改成「加 `document_bytes bytea` → 离线回填 → 删旧列」三个
  变更集，不要在迁移里跑 Java 编码逻辑。

### 5.3 `control-008-report-window` / `control-009-report-rows`

```sql
--changeset train0:control-008-report-window
CREATE TABLE control.report_window (
    agent_id     text   NOT NULL REFERENCES control.agent (agent_id),
    window_id    bigint NOT NULL,
    window_ms    integer NOT NULL,
    complete     boolean NOT NULL,
    window_start timestamptz,
    truncated    boolean NOT NULL DEFAULT false,
    row_count    integer NOT NULL DEFAULT 0,
    received_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, window_id)
);

--changeset train0:control-009-report-rows
DROP TABLE control.report;   -- Train 0 空表；jsonb 存不下 32 字节 key_hash 也无法承载 0 行窗口
CREATE TABLE control.report (
    agent_id             text   NOT NULL,
    window_id            bigint NOT NULL,
    key_hash             bytea  NOT NULL CHECK (octet_length(key_hash) = 32),
    packets              bigint NOT NULL,
    first_seen_offset_ns bigint NOT NULL,
    last_seen_offset_ns  bigint NOT NULL,
    sample_dropped       boolean NOT NULL DEFAULT false,
    degraded             boolean NOT NULL DEFAULT false,
    received_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, window_id, key_hash),
    FOREIGN KEY (agent_id, window_id) REFERENCES control.report_window (agent_id, window_id)
);
CREATE TABLE control.report_sample (
    agent_id     text   NOT NULL,
    window_id    bigint NOT NULL,
    key_hash     bytea  NOT NULL,
    five_tuple   text   NOT NULL,
    ts_offset_ns bigint NOT NULL,
    PRIMARY KEY (agent_id, window_id, key_hash, ts_offset_ns)
);
```

- 为什么要 `report_window`：契约 §5 把游标定义为「已完整收下的最大 window_id」，一个窗口可以
  **聚合行为 0**（无流量），只按 `report` 求 `max(window_id)` 会永远追不上；而且 `complete` /
  `truncated` / `window_ms` 这些窗口级字段现在无处落。
- `key_hash` 用 `bytea(32)`：契约 §7 说它是 32 字节二进制、agent 原样上报不重算；用 `text` 存 hex 会在
  Rust/Java 任一侧多一次编解码，且在 SQL 里没法按前缀/范围查（S1-3 要做面板）。
- `report.agent_id` 保留但**不**加外键到 `agent`：一批报告可以在 Register 之前的窗口产生吗？不能——但
  加外键会让 `report_window` 先插入的顺序变成硬约束，靠 window FK 已经够了。
- `control.agent.reported_cursor` 保留但降级为「观测用缓存」，`GetCursor` 读 `max(window_id)`。

### 5.4 `control-010-config-ack`

```sql
--changeset train0:control-010-config-ack
CREATE TABLE control.config_ack (
    agent_id          text   NOT NULL REFERENCES control.agent (agent_id),
    version           integer NOT NULL,
    content_sha256    text   NOT NULL CHECK (content_sha256 ~ '^[0-9a-f]{64}$'),
    result            text   NOT NULL CHECK (result IN ('applied','rejected','failed')),
    applied_at        timestamptz,
    apply_ms          integer,
    degraded_features text[] NOT NULL DEFAULT '{}',
    error_code        text,
    error_detail      text,
    received_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, version)
);
```

契约 §13.2 只要求在 `control.agent` 上加 `applied_version/applied_at`，但 G1 和「谁拒了哪版、为什么拒」
需要逐版本记录：`control.agent` 上的两列只是最新值的反规范化。`received_at` 是控制面时钟（契约 §5/§9），
**首次写入后不再覆盖**（`COALESCE`），否则重试会把 G1 的终点往后推。

### 5.5 与既有表的其余关系

- `notify_outbox`：Train 0 不写入（Watch = UNIMPLEMENTED，没有投递方）。表留着，注释里写明「发布路径
  在 S1-3 接 Watch 时才开始写」。不要为了「表不空」而造 pending 行——那会让未来的 dispatcher 一上线就
  补发一堆过时通知。
- `audit_outbox`：发布时**写一行**（`{action:"config.publish", version, group_id, sha256, published_by}`），
  与 `config_version` 同事务。转发器（`/internal/v1/audit/events`）是 S1-3，本地行就是重试缓冲。
- `cc_app` 的权限已经够（initdb 里 `SELECT,INSERT,UPDATE,DELETE` + 序列权限），新表靠 `ALTER DEFAULT
  PRIVILEGES` 自动继承，无需改 `20-schemas.sql`。

---

## 6. 六个 RPC 的实现

公共约定：

- 时间一律 `Instant.now()` → `Timestamp`（毫秒精度：`ms = instant.toEpochMilli(); seconds=ms/1000;
  nanos=(int)(ms%1000)*1_000_000`）。不要直接用 `Timestamps.fromMillis(System.currentTimeMillis())` 两次，
  一个响应里只有一个 `server_time`。
- 所有写操作在**一个** `@Transactional` 方法里；gRPC 回调线程不持有事务（先查后写不要跨 await）。
- 每个响应都填 `server_time`（`GetCursorResponse`、`HeartbeatResponse` … 契约 §2）。

### 6.1 Register

```
行来自拦截器（已按 CN + 指纹校验过）。校验与写入同一事务：
1) machine_id 空 → INVALID_ARGUMENT / request.invalid
2) SELECT ... FOR UPDATE control.agent WHERE agent_id=?
3) 若 row.machine_id 非空且 != req.machine_id → FAILED_PRECONDITION / agent.machine_conflict
4) UPDATE control.agent SET machine_id=COALESCE(machine_id, ?), agent_version=?, boot_id=?,
       capabilities=?::jsonb, registered_at=now(), last_heartbeat=now()
5) desired = latestConfig(row.group_id)     -- §6.7
6) response: agent_id, machine_id, group_id(可空 → 空串), status, desired_version, desired_sha256,
   heartbeat_interval_ms, idle_heartbeat_interval_ms, server_time, effective_capabilities
```

`effective_capabilities` = 服务端裁剪结果：

```
config_schema  = [s for s in req.config_schema  if s == 1]        // Train 0 只支持 schema 1
default_action = [a for a in req.default_action if a == PASS]     // 只有 PASS 能兑现
rules_execution = false                                           // 数据面不执行规则（契约 L2）
```

返回 `rules_execution=false` 是**故意**的：它让 agent 在规则降级时走 §6.3 的「降级接受」路径，而不是以为
自己能执行。`Register` 不是发证（契约 §5）：不创建行、不改指纹、不提升 status。

### 6.2 Heartbeat

```
1) UPDATE control.agent SET last_heartbeat=now(), config_state=?, applied_version=GREATEST(applied_version, ?)
   -- applied_version 只取 req.current_version，不信任 ack 之外的信息
2) desired = latestConfig(group_id)
3) lagging = desired.version > req.current_version
   heartbeat_interval_ms      = lagging ? cc.heartbeat.lagging-ms(1000) : cc.heartbeat.steady-ms(5000)
   idle_heartbeat_interval_ms = cc.heartbeat.idle-ms(5000)
4) response: desired_version, desired_sha256, 两个间隔, server_time
```

- `desired_sha256` 必须回：agent 用它做「同内容跳过下载」（契约 §5）。
- 心跳只更新 `last_heartbeat/config_state`，**不**写 ack 表——否则 G1 的 `received_at` 会被心跳污染。
- `pending` 状态的 agent 照常回心跳，desired 按组算；`revoked` 走拦截器，到不了这里。

### 6.3 GetConfig

```
desired = latestConfig(agent.group_id)
if desired == null || desired.version <= req.current_version
      → GetConfigResponse{up_to_date:true}     // 组内无配置也走这条，不是错误（契约 §5）
else  → GetConfigResponse{up_to_date:false,
          config: ConfigEnvelope{version, schema_version, content_sha256, document(bytes), published_at}}
```

- **不要**在服务端做「能力裁剪后的拒绝」。契约把校验放在 agent（六步固定顺序），服务端只负责给出
  已发布字节。能力不足由 agent ack `REJECTED/config.schema_unsupported`，控制面记日志 + 指标。
- `req.current_version > desired`（agent 比控制面还新，通常是回滚）→ 同样回 `up_to_date=true`；
  不要在这里报错，`ack.version_unknown` 只在 AckConfig 判。
- `document` 直接来自 `bytea` 列，不做任何解码/再编码。

### 6.4 AckConfig

```
1) req.result == ACK_RESULT_UNSPECIFIED → INVALID_ARGUMENT / request.invalid（不得当 APPLIED）
2) desired = latestConfig(group_id)
   req.version > desired.version → FAILED_PRECONDITION / ack.version_unknown
3) INSERT INTO control.config_ack (...) VALUES (...)
   ON CONFLICT (agent_id, version) DO UPDATE SET
       result=EXCLUDED.result, content_sha256=EXCLUDED.content_sha256,
       applied_at=COALESCE(control.config_ack.applied_at, EXCLUDED.applied_at),
       apply_ms=EXCLUDED.apply_ms, degraded_features=EXCLUDED.degraded_features,
       error_code=EXCLUDED.error_code, error_detail=EXCLUDED.error_detail,
       received_at=control.config_ack.received_at        -- 保留首次
4) result==APPLIED 时 UPDATE control.agent SET applied_version=GREATEST(applied_version,?),
       applied_at=COALESCE(applied_at, ?)                 -- 保留首次
5) response: recorded=true, desired_version, received_at, server_time
```

- `applied_at` 来自 agent（它的时钟，只给人看）；`received_at` 是控制面时钟，G1 用它（契约 §5/§9）。
- `degraded_features` Train 0 只接受 `"rules"`；其它值照收但打 WARN（不要 500，前端版本不一致时不值得炸）。
  用 `text[]`，PostgreSQL JDBC 传 `String[]` 由 `createArrayOf("text", ...)` 或 `setArray` 完成。
- `error_detail` 截断到 512 字符再落库（契约 §12.3）。
- 幂等：同一 `(agent_id, version)` 重复 ack 覆盖结果但保留两个「首次」时间；这就是契约的「覆盖，保留首次
  `applied_at`」。

### 6.5 ReportWindows

```
0) 批级校验：windows 数 > 1000 → RESOURCE_EXHAUSTED / report.batch_too_large（4 MiB 由 `inbound.message.max-size` 拦）
   任一 window.window_id <= 0 或 window_ms <= 0 → INVALID_ARGUMENT / report.malformed（整批不落）
   批内 window_id 非严格递增 → INVALID_ARGUMENT / report.malformed
1) cursor = SELECT COALESCE(max(window_id), 0) FROM control.report_window WHERE agent_id=?
2) 逐窗口：
   if cursor > 0 && window_id + 1000 < cursor
        → rejected++, rejected_windows += RejectedWindow{window_id, "report.window_too_old"}；continue
   INSERT INTO control.report_window (...) ON CONFLICT DO NOTHING
      → 影响 0 行 ⇒ 该窗口头已存在，dupWindow=true
   逐 aggregate：INSERT INTO control.report ... ON CONFLICT DO NOTHING；统计 inserted
   逐 sample：同上（report_sample）
   若全部 inserted == 0 ⇒ duplicates++（整窗重复，与 mock 一致）
   否则 accepted++ 且 UPDATE control.report_window SET row_count=row_count+inserted
   UPDATE control.agent SET reported_cursor=GREATEST(reported_cursor, window_id)
3) 提交后 last_window_id = SELECT COALESCE(max(window_id),0) FROM control.report_window WHERE agent_id=?
4) debug_assert(accepted + duplicates + rejected == windows.size())
5) response: accepted, duplicates, rejected, rejected_windows, last_window_id, server_time
```

关键点：

- 「整窗重复才算 duplicates、只要有一行新就算 accepted」这个口径抄 `agent/src/mock/mod.rs`；契约 §5 的
  `duplicates+1` 描述的是行级幂等，两者在这里必须选一个并写进测试——选 mock 的窗口级口径，因为它让
  `accepted+duplicates+rejected == 窗口数` 这个不变量成立。
- `report.window_too_old` **不是** RPC 级错误：整批照收，只是把窗口放进 `rejected_windows`（契约 §12.2）。
- 一批报告可能跨多个 `window_id`；事务粒度 = 一次 RPC，`accepted` 计数只在提交后有意义（先写后提交，别
  在循环里发响应）。
- `key_hash` 直接 `setBytes(ByteString)` → `bytea`，**不校验内容、不重算**（契约 §5）。
- 窗口头先插、行后插：FK 天然保证顺序，也保证 `max(window_id)` 只在窗口真正收下后才前进。

### 6.6 GetCursor

```
SELECT max(window_id) FROM control.report_window WHERE agent_id = ?
无行 → 不设 last_window_id（optional 语义：表示「从未上报」，与「上报过 0 号」不同）
有行 → set_last_window_id(max)
server_time 必填
```

不要用 `control.agent.reported_cursor` 当权威（它会因为一次 UPDATE 失败而落后）；`max(window_id)` 是唯一
真相源，PK `(agent_id, window_id)` 让这个 max 走索引尾扫。

### 6.7 `latestConfig(groupId)`（被 Register/Heartbeat/GetConfig/Ack 复用）

```sql
SELECT version, content_sha256, schema_version, document, published_at
FROM control.config_version
WHERE group_id = ?
ORDER BY version DESC
LIMIT 1
```

- 走 `config_version_group_idx (group_id, version DESC)`，单行，无需缓存。
- `groupId` 为空（未准入）→ 返回 null，`desired_version = 0`，`desired_sha256` 为空 bytes。
- 心跳是 1 QPS/agent：N=100 也就是 100 次单行索引查询，Train 0 不需要缓存；真要缓存就给
  `cc.desired-cache-ttl`（默认 0，即关），不要提前引入。

### 6.8 Watch

```java
responseObserver.onError(Status.UNIMPLEMENTED
    .withDescription("no push channel: Watch is not implemented by this control plane")
    .asRuntimeException());
```

必须是 `UNIMPLEMENTED`：agent 据此**停止**重试 Watch、改走心跳轮询、60s 后再试（契约 §3/§8）。返回
`UNAVAILABLE` 会让 agent 进入退避风暴，关连接会变成「传输错误」。

---

## 7. 发布路径（`ConfigPublisher`）

契约 §13.2 第 5 条：编码一次、算 sha、原样落库。实现成 `@Service`，与 gRPC 无关，便于测试：

```
publish(groupId, ConfigDocument doc, String publishedBy):
  bytes = doc.toByteArray()                       // 唯一的规范化点
  if bytes.length > 65536            → AgentRpcException(INVALID_ARGUMENT, "config.too_large")
  validate(doc)                      → AgentRpcException(INVALID_ARGUMENT, "config.invalid_document")
  sha  = lowercaseHex(sha256(bytes))
  version = nextval('control.config_version_seq')
  tx:
    INSERT config_version(version, group_id, sha, doc.schema_version, bytes, published_by, now())
    INSERT audit_outbox(request_id=uuid, payload={"action":"config.publish", ...})
```

`validate`（契约 §6.2 的代码校验部分，proto 表达不了的都在这里）：

| 项 | 规则 | 错误码 |
|---|---|---|
| schema_version | 必须 == 1（Train 0） | `config.schema_unsupported` |
| window_ms | 1000–60000 | `config.invalid_document` |
| grace_ms | 0 ≤ grace_ms ≤ window_ms | `config.invalid_document` |
| src_prefix_v4 / v6 | 0–32 / 0–128 | `config.invalid_document` |
| default_action | Train 0 只允许 PASS | `config.unsupported_default_action` |
| rules 数 | ≤ 20 | `config.invalid_document` |
| rule_index | 必须 == 下标+1 | `config.invalid_document` |
| rule_id | `^[a-z0-9][a-z0-9._-]{0,31}$`，文档内唯一 | `config.invalid_document` |
| priority | 1–1000，文档内唯一 | `config.invalid_document` |
| dst_ports | 每项 1–65535，空 = 不限 | `config.invalid_document` |

**测试向量是控制面的义务**（契约 §6.4）：`ProtoVectorTest` 必须断言

```
doc_a.toByteArray() == hex("080110904e18d00f20182840")            // 12 字节
sha256Hex              == "971d3944b510ab8ea6cbc469ccae565c9ad0a60f27801e9cad052adbe4253c05"
doc_b.toByteArray() == hex("080110904e18d00f201828403a31...3001")  // 63 字节
sha256Hex              == "c3e4eea7f545643b966956fbea32fbbc3fb44acc019999dd5ed9552940b3d30f"
```

doc_a / doc_b 的文本形态见契约附录 A；用 `com.google.protobuf.TextFormat.parse(ConfigDocument.newBuilder(), reader)`
从测试资源里的 `.txt` 解析，**不要手写 Java builder**——手写等于把「两侧规范化不一致」的失效模式重新引入。

发布入口（HTTP）不在本分支：`/api/proposals`、发布 API 是 S1-3，且 S1-2 之前 8082 上没有鉴权，
不要提前开一个能写配置的 HTTP 面。见 §12-D3。

---

## 8. 可观测性

- **日志**（一条 RPC 一行，INFO）：`rpc=GetConfig agent=agent-m1 code=OK dur_ms=1 desired=12`；
  身份失败/吊销用 WARN；`ack.result=rejected` 用 INFO 但把 `error_code` 打出来（排障第一现场）。
  所有日志自动带 MDC `request_id`。
- **指标**（Micrometer，spring-grpc 的 actuator 集成本身已给 `grpc.server.*`）：
  `cc.config.publish{group}`、`cc.config.ack{result}`、`cc.report.windows{outcome=accepted|duplicate|rejected}`、
  `cc.report.rows{outcome=inserted|duplicate}`、`cc.heartbeat.lagging`（gauge：落后 agent 数）。
  够用于 Train 0 的 G1 观测即可，不要上 exemplar/tracing。
- **trailers**：`x-request-id`（全部）、`x-error-code`（仅错误）、`x-retry-after-ms`（仅 server.busy）。

---

## 9. 部署与配置变更清单

| 文件 | 变更 |
|---|---|
| `app/pom.xml` | §2 的依赖 + 插件（Boot 4.1.2 parent、`spring-boot-starter-grpc-server`、ascopes protoc 插件）；无 `<version>`、无 `spring-grpc-dependencies` |
| `app/src/main/proto/control.proto` | 新增（从 `agent/proto/` 拷，逐字节相同） |
| `app/src/main/resources/application.yml` | `spring.grpc.server.port/servlet.enabled/inbound.message.max-size`；`spring.grpc.server.ssl.bundle` + `client-auth`；`spring.ssl.bundle.pem.*`；`control-plane.*`（heartbeat、limits） |
| `app/src/main/java/dev/train0/cc/control/**` | Service、拦截器、Publisher、Validator、仓库层 |
| `db/control/changelog.sql` | control-006 … control-010 |
| `compose.yaml` | control 容器加 `${CC_TLS_DIR}:/etc/cc/tls:ro` 与 `CC_TLS_CERT/KEY/CA`；9090 已发布，无需改 |
| `deploy/k3d/k8s/40-control.yaml` | 为 control Deployment 加 TLS secret 挂载（`control-plane-tls`）与同名 env；Service 已有 grpc/9090 |
| `deploy/k3d/k8s/70-netpol.yaml` | 无需改（`allow-control-grpc-from-agents` 已存在）；把「9090 还没有服务端」的注释改掉 |
| `scripts/smoke.sh` | 加两条断言：mgmt 上 9090 不监听；control 上 9090 监听且无证书连接被拒 |
| `.github/workflows/control-center.yml` | 加 `proto-check.sh` 步骤；`changes` 过滤器加 `agent/proto/**`；`mvn package` 去掉 `-DskipTests` |
| `README.md` | §6「还没有的」删掉 gRPC 服务端与 `document→bytea` 两项，补「gRPC 服务端已就绪、Watch 仍 UNIMPLEMENTED、发布无 HTTP 入口」 |

`CC_TLS_*` 的取值来源（谁签证书）是 S2 的事；本分支只需要「运维能把三份 PEM 挂进来」。测试环境用
`scripts/dev-certs.sh`（openssl 或 Java 自签）生成 CA + server + client，输出到 `.tmp/tls/`，不进 git。

---

## 10. 测试矩阵

| 层 | 用例 | 断言 |
|---|---|---|
| 单元 | `ProtoVectorTest` | doc_a 12 字节 / doc_b 63 字节 / 两条 sha256（§7） |
| 单元 | `ConfigDocumentValidatorTest` | 上表每一行的正反例，尤其 `grace_ms > window_ms`、`priority` 重复、`rule_index` 不从 1 开始、rules=21 |
| 单元 | `AgentIdentityTest` | CN 提取（含转义逗号、大小写）；指纹小写 64 hex；未知 CN / 指纹不符同码；revoked |
| 单元 | `AckIdempotencyTest` | 同版本二次 ack 覆盖结果但保留首次 `applied_at` 与 `received_at` |
| 集成（in-process） | `AgentControlHappyPathTest` | Register→GetConfig(up_to_date→新版)→Ack(APPLIED)→Report→GetCursor 全链路；`desired_version` 按组隔离、跳号 7→12 |
| 集成 | `AgentControlErrorTest` | `ack.version_unknown`、`agent.machine_conflict`、UNSPECIFIED ack → `request.invalid`、`report.window_too_old` 进 `rejected_windows`、Watch → `UNIMPLEMENTED`、未 Register 就 Report → `agent.not_registered` |
| 集成 | `ReportIdempotencyTest` | 同一批重发 → `duplicates == 窗口数`、`last_window_id` 不变；「一窗部分新」→ accepted 且行不重复 |
| 集成 | `ReportCursorTest` | 0 行窗口（无流量）也推进游标；`GetCursor` 首次返回 absent |
| TLS | `MutualTlsTest` | 无客户端证书握手失败；CA 不在信任链的证书握手失败；正确证书通过 |
| 跨语言 | `verification/run_control_demo.sh` 变体 | 真 `probe-agent` 连 Spring 9090：10 次发布全部 APPLIED、退出码 0；`heartbeat_interval_ms` 落后 1000 / 追平 5000 |
| 部署 | `scripts/smoke.sh` | 角色端口断言（§3）、health 仍走 8080 |

集成测试用 `@SpringBootTest` + `@AutoConfigureInProcessTransport`（`spring-grpc-test`），不要去绑真端口；
TLS 单独用 Netty 真端口测一次（进程内传输不含 TLS 层）。

---

## 11. 契约与现状的偏差（必须在 §12 或 README 里落字）

1. **基线升到 Spring Boot 4.1.2**（契约 §13.2 只写「spring-grpc starter」，没写版本）：从 4.1 起
   服务端自动配置并入 Boot 自身，`spring-grpc` 由 Boot BOM 管到 1.1.2，因此**不引**
   `spring-grpc-spring-boot-starter`（它停在 1.0.3）。升级代价实测为三处，已在分支
   `chore/boot-4.1.2-upgrade` 落地：
   1. parent `3.5.16 → 4.1.2`；
   2. `org.liquibase:liquibase-core` → `org.springframework.boot:spring-boot-starter-liquibase`——
      Boot 4 把自动配置按技术拆成独立模块，只带 `liquibase-core` 时迁移**静默不执行**
      （应用照常启动、health 照常 UP、日志无 warning、表一张没建）；
   3. 删掉 `spring.jackson.serialization.write-dates-as-timestamps`——Jackson 3 已无该
      `SerializationFeature`，留着直接启动失败（`No enum constant ...write-dates-as-timestamps`）。

   附带事实：Framework 7.0.10 / Tomcat 11.0.27（Servlet 6.1）/ Jackson **3.1.7**（包名 `tools.jackson`，
   写 jsonb payload 时要用新包）/ Liquibase **5.0.4**；actuator 多一个 `ssl` health indicator（对 §4 有用）。
   实测（真 PG，容器内 `mvn spring-boot:run`）：4 个 mgmt changeset 全部应用、`/api/machines` JSON 正常、
   health `UP`。
2. **Gradle → Maven**：契约写 Gradle protobuf 插件，实现用 `io.github.ascopes:protobuf-maven-plugin`。
3. **`group_id` 不在注册报文里**：契约 §5 的 `desired_version` 依赖组，但 `RegisterRequest` 没有组字段，
   控制面又不读 mgmt 库。实现上加 `control.agent.group_id`，并明确它由准入流程写入。
4. **`report` 表**：契约只说「保留 (agent_id,window_id,key_hash)」，实际要拆成 `report_window`（窗口头，
   承载游标）+ `report`（行，bytea key_hash）+ `report_sample`。
5. **`duplicates` 口径**：行级 vs 窗口级。实现取窗口级（与 mock 一致），并在测试里钉住不变量
   `accepted+duplicates+rejected == 窗口数`。
6. **`pending` 状态**：契约只定义枚举值，没定义准入语义。本实现允许 pending 拉配置（`up_to_date=true`）。
7. **`Watch`**：Train 0 恒 `UNIMPLEMENTED` → `notify_outbox` 不写、G1 只能按纯轮询口径量（契约 §9 已写明）。
8. **`pending`/`revoked` 之外的中间态**（如租约过期、离线判定）契约没有，Train 0 不做。

---

## 12. 需要拍板的决定（D1–D6）

| # | 问题 | 建议 | 影响 |
|---|---|---|---|
| D1 | 基线版本？ | **已决定（2026-11-18）：升 Spring Boot 4.1.2**，服务端用 Boot 自带的 gRPC 自动配置（`spring-boot-starter-grpc-server`），mTLS 走 `spring.grpc.server.ssl.client-auth=require`。理由：3.5 的 OSS 支持 2026-06-30 已结束、4.0 于 2026-12-31 EOL，4.1 支持到 2027-07-31；且属性式 mTLS 省掉一份迟早要删的 `ServerBuilderCustomizer` | 已在本分支落地（parent / liquibase starter / jackson 属性三处）；后续 gRPC 依赖只加 Boot 的 starter，**不** import `spring-grpc-dependencies` |
| D2 | `group_id` 从哪来？ | 准入/发证时写 `control.agent.group_id`（与 `cert_fingerprint` 同一步）；**不**让控制面读 mgmt 库 | 决定 S2 的证书签发脚本要不要带组参数 |
| D3 | Train 0 的发布入口？ | 只做 `ConfigPublisher` bean + 测试夹具；HTTP 发布 API 留 S1-3（8082 无鉴权，别开写面） | 决定 k3d 能否端到端演示 G1；若要演示，加 `/internal/v1/config-versions` 并靠 netpol 限制 |
| D4 | `report` 落库形态？ | 三表（`report_window` / `report` / `report_sample`），`key_hash` 用 `bytea(32)` | 决定 S1-3 面板能不能直接聚合；jsonb 方案省一张表但读不动 |
| D5 | `pending` 语义？ | 允许 Register/Heartbeat/GetCursor/ReportWindows；GetConfig 返回 `up_to_date=true` | 若要求「未批准不发配置且报错」，agent 侧要新增错误处理分支 |
| D6 | `machine_id` 绑定的代价 | 保留契约的 `agent.machine_conflict`，但要求 agent 显式给 `PROBE_AGENT_MACHINE_ID`（默认取 `HOSTNAME`，在 k8s Deployment 里就是 Pod 名，重建即变） | 不改代码，但要写进 README 的部署注意事项；否则第一次 Pod 重建就注册失败 |

---

## 13. 开工顺序（每步一个能挂掉的门）

1. **构建骨架**：pom 依赖 + protoc 插件 + proto-check + CI 三处改动 → 门：`mvn -q -B -DskipTests package`
   成功，`dependency:tree` 显示 spring-core 7.0.10 / boot 4.1.2 / grpc-* 1.84.1；`scripts/migrate.sh`
   在 Liquibase 5.x 上跑通（2026-11-18 已验：mgmt `Run: 4`、control `Run: 5`、重复 up 幂等
   `Run: 0 / Previously run: 4`、`rollbackCount=1` 后再 up `Run: 1`）；`proto-check.sh` 在改一个字节后失败。
2. **字节与值域**：`ProtoVectorTest` + `ConfigDocumentValidatorTest` + `ConfigPublisher`（不接 HTTP）→
   门：两条 sha256 与 12/63 字节断言全绿。这一步不碰 gRPC，也就能在没有证书的情况下先钉死契约最容易
   漂移的部分（§6.4）。
3. **库表**：control-006…010 → 门：`scripts/migrate.sh` 在空库上跑通，`bash scripts/psql.sh control` 里
   `\d control.report` 是 bytea 版；回滚路径手跑一遍。
4. **服务端**：身份/request-id 拦截器 + TLS customizer + 六个 RPC + 集成测试（in-process）→ 门：
   `AgentControlHappyPathTest` / `ErrorTest` / `ReportIdempotencyTest` 全绿；`MutualTlsTest` 三条断言成立。
5. **部署与跨语言**：compose/k8s 挂 TLS、smoke 角色断言、真 `probe-agent` 连 9090 → 门：10 次发布全
   APPLIED 退出码 0，且**只**按纯轮询口径报 G1（落后 ~1s、P95 ≤ 3s），报告里注明 Watch 仍是
   `UNIMPLEMENTED`（契约 L4 / §9）。

