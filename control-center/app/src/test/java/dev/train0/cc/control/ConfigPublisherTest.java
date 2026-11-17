package dev.train0.cc.control;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.catchThrowableOfType;

import com.google.protobuf.TextFormat;
import io.grpc.Status;
import io.probe.control.agent.v1.ConfigDocument;
import io.probe.control.agent.v1.Rule;
import io.probe.control.agent.v1.RuleMatch;
import java.io.IOException;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.Map;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.jdbc.core.JdbcTemplate;

/**
 * Exercises {@link ConfigPublisher} against a real PostgreSQL schema (design §7): the version row
 * and its audit event must be committed together, so the test asserts the database, not mocks.
 *
 * <p>Skipped unless {@code CC_IT_PG=1}; the CI pipeline runs it through {@code mvnccdb}.
 */
@SpringBootTest
@EnabledIfEnvironmentVariable(named = "CC_IT_PG", matches = "1")
class ConfigPublisherTest {

    private static final String DOC_A_SHA256 =
            "971d3944b510ab8ea6cbc469ccae565c9ad0a60f27801e9cad052adbe4253c05";

    @Autowired
    private ConfigPublisher publisher;

    @Autowired
    private JdbcTemplate jdbc;

    @BeforeEach
    void clearPublishState() {
        jdbc.update("delete from control.audit_outbox where payload->>'action' = ?",
                ConfigPublisher.AUDIT_ACTION);
        jdbc.update("delete from control.config_version");
    }

    @Test
    void publishPersistsVersionAndAuditRow() throws IOException {
        ConfigDocument doc = loadDocument("proto/doc_a.textproto");

        PublishedConfig published = publisher.publish("g1", doc, "alice");

        assertThat(published.version()).isPositive();
        assertThat(published.contentSha256()).isEqualTo(DOC_A_SHA256);
        assertThat(published.bytes()).isEqualTo(12);

        Map<String, Object> versionRow = jdbc.queryForMap("""
                select content_sha256, schema_version, octet_length(document) as document_bytes
                from control.config_version where version = ?
                """, published.version());
        assertThat(versionRow.get("content_sha256")).isEqualTo(DOC_A_SHA256);
        assertThat(versionRow.get("schema_version")).isEqualTo(1);
        assertThat(versionRow.get("document_bytes")).isEqualTo(12);

        Map<String, Object> auditRow = jdbc.queryForMap("""
                select request_id, payload->>'group_id' as group_id,
                       payload->>'sha256' as sha256, payload->>'published_by' as published_by
                from control.audit_outbox where payload->>'action' = ?
                """, ConfigPublisher.AUDIT_ACTION);
        assertThat((String) auditRow.get("request_id")).isNotBlank();
        assertThat(auditRow.get("group_id")).isEqualTo("g1");
        assertThat(auditRow.get("sha256")).isEqualTo(DOC_A_SHA256);
        assertThat(auditRow.get("published_by")).isEqualTo("alice");
        assertThat(publishAuditRows()).isEqualTo(1);
    }

    @Test
    void rejectsDocumentLargerThan64KiB() {
        ConfigDocument oversized = ConfigDocument.newBuilder()
                .setSchemaVersion(1)
                .setWindowMs(1000)
                .addRules(Rule.newBuilder()
                        .setRuleIndex(1)
                        .setRuleId("r-1")
                        .setPriority(1)
                        .setMatch(RuleMatch.newBuilder().setDstAddr("a".repeat(70_000))))
                .build();
        assertThat(oversized.toByteArray().length)
                .isGreaterThan(ConfigPublisher.MAX_DOCUMENT_BYTES);

        AgentRpcException rejection = catchThrowableOfType(
                () -> publisher.publish("g1", oversized, "alice"), AgentRpcException.class);

        assertThat(rejection).isNotNull();
        assertThat(rejection.code()).isEqualTo(Status.Code.INVALID_ARGUMENT);
        assertThat(rejection.errorCode()).isEqualTo("config.too_large");
        assertThat(publishedVersions()).isZero();
        assertThat(publishAuditRows()).isZero();
    }

    @Test
    void rejectsInvalidDocumentWithoutWrites() throws IOException {
        ConfigDocument invalid = loadDocument("proto/doc_a.textproto").toBuilder()
                .setWindowMs(999)
                .build();

        AgentRpcException rejection = catchThrowableOfType(
                () -> publisher.publish("g1", invalid, "alice"), AgentRpcException.class);

        assertThat(rejection).isNotNull();
        assertThat(rejection.code()).isEqualTo(Status.Code.INVALID_ARGUMENT);
        assertThat(rejection.errorCode()).isEqualTo("config.invalid_document");
        assertThat(rejection.detail()).contains("window_ms");
        assertThat(publishedVersions()).isZero();
        assertThat(publishAuditRows()).isZero();
    }

    private int publishedVersions() {
        Integer count = jdbc.queryForObject(
                "select count(*) from control.config_version", Integer.class);
        return count == null ? 0 : count;
    }

    private int publishAuditRows() {
        Integer count = jdbc.queryForObject(
                "select count(*) from control.audit_outbox where payload->>'action' = ?",
                Integer.class, ConfigPublisher.AUDIT_ACTION);
        return count == null ? 0 : count;
    }

    private ConfigDocument loadDocument(String resource) throws IOException {
        try (InputStream in = getClass().getClassLoader().getResourceAsStream(resource)) {
            assertThat(in).as("classpath resource %s", resource).isNotNull();
            return TextFormat.parse(new String(in.readAllBytes(), StandardCharsets.UTF_8),
                    ConfigDocument.class);
        }
    }
}
