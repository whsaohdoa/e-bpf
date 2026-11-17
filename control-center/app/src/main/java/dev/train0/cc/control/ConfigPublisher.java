package dev.train0.cc.control;

import io.grpc.Status;
import io.probe.control.agent.v1.ConfigDocument;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.HexFormat;
import java.util.UUID;
import org.slf4j.MDC;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Service;
import org.springframework.transaction.annotation.Transactional;

/**
 * Writes one immutable configuration version (design §7).
 *
 * <p>The protobuf document is encoded exactly once, before validation, so the bytes that are
 * hashed, stored and later served to agents cannot diverge. The {@code config_version} row and its
 * {@code audit_outbox} event are committed in the same transaction: a published version without an
 * audit row, or the reverse, must never be observable.
 *
 * <p>Both the gRPC server (M3) and the unauthenticated HTTP publish endpoint are thin wrappers: all
 * encoding, validation and hashing lives here.
 */
@Service
class ConfigPublisher {

    /** A configuration document must fit into one 64 KiB gRPC message (contract §9). */
    static final int MAX_DOCUMENT_BYTES = 65_536;

    static final String AUDIT_ACTION = "config.publish";

    private final ConfigDocumentValidator validator;
    private final JdbcTemplate jdbc;

    ConfigPublisher(ConfigDocumentValidator validator, JdbcTemplate jdbc) {
        this.validator = validator;
        this.jdbc = jdbc;
    }

    @Transactional
    PublishedConfig publish(String groupId, ConfigDocument doc, String publishedBy) {
        byte[] bytes = doc.toByteArray();
        if (bytes.length > MAX_DOCUMENT_BYTES) {
            throw new AgentRpcException(Status.Code.INVALID_ARGUMENT, "config.too_large",
                    "document is " + bytes.length + " bytes, maximum is " + MAX_DOCUMENT_BYTES);
        }
        validator.validate(doc);

        String contentSha256 = sha256Hex(bytes);
        Integer version = jdbc.queryForObject(
                "select nextval('control.config_version_seq')", Integer.class);
        jdbc.update("""
                insert into control.config_version
                    (version, group_id, content_sha256, schema_version, document, published_by)
                values (?, ?, ?, ?, ?, ?)
                """,
                version, groupId, contentSha256, doc.getSchemaVersion(), bytes, publishedBy);

        String payload = auditPayload(version, groupId, contentSha256, publishedBy);
        jdbc.update("insert into control.audit_outbox (request_id, payload) values (?, ?::jsonb)",
                requestId(), payload);

        return new PublishedConfig(version, contentSha256, bytes.length);
    }

    /**
     * The publish path must stay attributable even when it is reached from the unauthenticated HTTP
     * endpoint, so every audit row carries either the request-scoped {@code request_id} or a fresh
     * one for direct (test) calls.
     */
    private static String requestId() {
        String requestId = MDC.get("request_id");
        return requestId == null || requestId.isBlank()
                ? UUID.randomUUID().toString()
                : requestId;
    }

    static String sha256Hex(byte[] bytes) {
        try {
            return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(bytes));
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException("SHA-256 not available", e);
        }
    }

    private static String auditPayload(int version, String groupId, String contentSha256,
            String publishedBy) {
        return "{\"action\":\"" + AUDIT_ACTION + "\""
                + ",\"version\":" + version
                + ",\"group_id\":" + jsonString(groupId)
                + ",\"sha256\":\"" + contentSha256 + "\""
                + ",\"published_by\":" + jsonString(publishedBy)
                + "}";
    }

    private static String jsonString(String value) {
        if (value == null) {
            return "null";
        }
        StringBuilder out = new StringBuilder(value.length() + 2).append('"');
        for (int i = 0; i < value.length(); i++) {
            char c = value.charAt(i);
            switch (c) {
                case '"' -> out.append("\\\"");
                case '\\' -> out.append("\\\\");
                case '\n' -> out.append("\\n");
                case '\r' -> out.append("\\r");
                case '\t' -> out.append("\\t");
                default -> {
                    if (c < 0x20) {
                        out.append(String.format("\\u%04x", (int) c));
                    } else {
                        out.append(c);
                    }
                }
            }
        }
        return out.append('"').toString();
    }
}
