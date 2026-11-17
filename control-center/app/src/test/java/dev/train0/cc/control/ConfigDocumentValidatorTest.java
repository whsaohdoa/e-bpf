package dev.train0.cc.control;

import static org.assertj.core.api.Assertions.assertThat;

import com.google.protobuf.TextFormat;
import io.grpc.Status;
import io.probe.control.agent.v1.ConfigDocument;
import org.junit.jupiter.api.Test;

/**
 * One accepting case plus one rejection per row of the design §7 validation table. Documents are
 * written as textproto so a test cannot drift from the wire format the validator actually sees.
 */
class ConfigDocumentValidatorTest {

    private static final String VALID_DOCUMENT = """
            schema_version: 1
            window_ms: 10000
            grace_ms: 2000
            src_prefix_v4: 24
            src_prefix_v6: 64
            default_action: DEFAULT_ACTION_PASS
            rules {
              rule_index: 1
              rule_id: "r-8080"
              priority: 10
              enabled: true
              match {
                family: FAMILY_IPV4
                protocol: L4_PROTOCOL_TCP
                src_prefix: "10.0.1.0/24"
                dst_addr: "10.0.2.5"
                dst_ports: 8080
              }
              action: ACTION_BLOCK
            }
            """;

    private final ConfigDocumentValidator validator = new ConfigDocumentValidator();

    @Test
    void acceptsTheContractVector() {
        validator.validate(document(VALID_DOCUMENT));
    }

    @Test
    void rejectsUnknownSchemaVersion() {
        assertRejected(VALID_DOCUMENT.replace("schema_version: 1", "schema_version: 2"),
                "config.schema_unsupported", "schema_version");
    }

    @Test
    void rejectsWindowOutsideRange() {
        assertRejected(VALID_DOCUMENT.replace("window_ms: 10000", "window_ms: 999"),
                "config.invalid_document", "window_ms");
    }

    @Test
    void rejectsGraceLargerThanWindow() {
        assertRejected(VALID_DOCUMENT
                        .replace("window_ms: 10000", "window_ms: 1000")
                        .replace("grace_ms: 2000", "grace_ms: 1001"),
                "config.invalid_document", "grace_ms");
    }

    @Test
    void rejectsIpv4PrefixTooLong() {
        assertRejected(VALID_DOCUMENT.replace("src_prefix_v4: 24", "src_prefix_v4: 33"),
                "config.invalid_document", "src_prefix_v4");
    }

    @Test
    void rejectsIpv6PrefixTooLong() {
        assertRejected(VALID_DOCUMENT.replace("src_prefix_v6: 64", "src_prefix_v6: 129"),
                "config.invalid_document", "src_prefix_v6");
    }

    @Test
    void rejectsDropDefaultAction() {
        assertRejected(VALID_DOCUMENT.replace("DEFAULT_ACTION_PASS", "DEFAULT_ACTION_DROP"),
                "config.unsupported_default_action", "default_action");
    }

    @Test
    void rejectsMoreThanTwentyRules() {
        assertRejected(documentWithRules(21), "config.invalid_document", "rules count");
    }

    @Test
    void rejectsWrongRuleIndex() {
        assertRejected(documentWithRules(2).replace("rule_index: 2", "rule_index: 3"),
                "config.invalid_document", "rule_index");
    }

    @Test
    void rejectsMalformedRuleId() {
        assertRejected(documentWithRules(1).replace("\"r-1\"", "\"R-1\""),
                "config.invalid_document", "rule_id");
    }

    @Test
    void rejectsDuplicateRuleId() {
        assertRejected(documentWithRules(2).replace("\"r-2\"", "\"r-1\""),
                "config.invalid_document", "rule_id");
    }

    @Test
    void rejectsPriorityOutsideRange() {
        assertRejected(documentWithRules(1).replace("priority: 1", "priority: 0"),
                "config.invalid_document", "priority");
    }

    @Test
    void rejectsDuplicatePriority() {
        assertRejected(documentWithRules(2).replace("priority: 2", "priority: 1"),
                "config.invalid_document", "priority");
    }

    @Test
    void rejectsDstPortOutsideRange() {
        assertRejected(VALID_DOCUMENT.replace("dst_ports: 8080", "dst_ports: 65536"),
                "config.invalid_document", "dst_ports");
    }

    private static void assertRejected(String textproto, String errorCode, String detailContains) {
        AgentRpcException failure = null;
        try {
            new ConfigDocumentValidator().validate(document(textproto));
        } catch (AgentRpcException e) {
            failure = e;
        }
        assertThat(failure)
                .as("expected %s mentioning %s", errorCode, detailContains)
                .isNotNull();
        assertThat(failure.code()).isEqualTo(Status.Code.INVALID_ARGUMENT);
        assertThat(failure.errorCode()).isEqualTo(errorCode);
        assertThat(failure.detail()).contains(detailContains);
    }

    private static ConfigDocument document(String textproto) {
        try {
            return TextFormat.parse(textproto, ConfigDocument.class);
        } catch (TextFormat.ParseException e) {
            throw new IllegalArgumentException("test fixture is not valid textproto", e);
        }
    }

    private static String documentWithRules(int ruleCount) {
        StringBuilder text = new StringBuilder("""
                schema_version: 1
                window_ms: 10000
                grace_ms: 2000
                src_prefix_v4: 24
                src_prefix_v6: 64
                default_action: DEFAULT_ACTION_PASS
                """);
        for (int i = 0; i < ruleCount; i++) {
            text.append("rules {\n")
                    .append("  rule_index: ").append(i + 1).append('\n')
                    .append("  rule_id: \"r-").append(i + 1).append("\"\n")
                    .append("  priority: ").append(i + 1).append('\n')
                    .append("  match { family: FAMILY_IPV4 protocol: L4_PROTOCOL_TCP "
                            + "dst_ports: 8080 }\n")
                    .append("}\n");
        }
        return text.toString();
    }
}
