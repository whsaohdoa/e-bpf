package dev.train0.cc.control;

import com.google.protobuf.InvalidProtocolBufferException;
import com.google.protobuf.TextFormat;
import io.grpc.Status;
import io.probe.control.agent.v1.ConfigDocument;
import java.util.Base64;
import java.util.LinkedHashMap;
import java.util.Map;
import java.util.UUID;
import org.slf4j.MDC;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.http.HttpStatus;
import org.springframework.http.ResponseEntity;
import org.springframework.web.bind.annotation.PostMapping;
import org.springframework.web.bind.annotation.RequestBody;
import org.springframework.web.bind.annotation.RequestMapping;
import org.springframework.web.bind.annotation.RestController;

/**
 * Unauthenticated publish endpoint on the control role (design §12-D3, alternative branch).
 *
 * <p>The design's default is to ship only the {@link ConfigPublisher} bean and leave the HTTP
 * publish surface to S1-3, because 8082 has no authentication yet. Train 0 takes the other option on
 * purpose: {@code POST /internal/v1/config-versions} is the only way to seed ten successive versions
 * for G1 without shell access to the database. It is therefore unauthenticated and must be reachable
 * only inside the cluster network policy; {@code compose.yaml} publishes it on the host loopback for
 * demos, and the management role never registers it.
 *
 * <p>The endpoint is a thin adapter: encoding, validation, hashing and the audit transaction all
 * live in {@link ConfigPublisher}.
 */
@RestController
@RequestMapping("/internal/v1/config-versions")
@ConditionalOnProperty(name = "cc.role", havingValue = "control")
class ConfigPublishController {

    static final String REQUEST_ID_HEADER = "X-Request-Id";

    private final ConfigPublisher publisher;

    ConfigPublishController(ConfigPublisher publisher) {
        this.publisher = publisher;
    }

    @PostMapping
    ResponseEntity<Map<String, Object>> publish(@RequestBody Map<String, String> request) {
        String requestId = UUID.randomUUID().toString();
        MDC.put("request_id", requestId);
        try {
            String groupId = required(request, "group_id");
            String publishedBy = required(request, "published_by");
            PublishedConfig published = publisher.publish(groupId, parseDocument(request),
                    publishedBy);

            Map<String, Object> body = new LinkedHashMap<>();
            body.put("version", published.version());
            body.put("content_sha256", published.contentSha256());
            body.put("bytes", published.bytes());
            return ResponseEntity.ok().header(REQUEST_ID_HEADER, requestId).body(body);
        } catch (AgentRpcException e) {
            Map<String, Object> body = new LinkedHashMap<>();
            body.put("error_code", e.errorCode());
            body.put("detail", e.detail());
            return ResponseEntity.status(statusFor(e))
                    .header(REQUEST_ID_HEADER, requestId)
                    .body(body);
        } finally {
            MDC.remove("request_id");
        }
    }

    private static String required(Map<String, String> request, String field) {
        String value = request.get(field);
        if (value == null || value.isBlank()) {
            throw new AgentRpcException(Status.Code.INVALID_ARGUMENT, "config.invalid_request",
                    "missing field: " + field);
        }
        return value;
    }

    /**
     * Accepts the document either as canonical protobuf bytes (base64) or as textproto: the latter
     * keeps demo requests readable and reuses the contract's appendix A fixtures.
     */
    private static ConfigDocument parseDocument(Map<String, String> request) {
        String textproto = request.get("document_textproto");
        if (textproto != null && !textproto.isBlank()) {
            try {
                return TextFormat.parse(textproto, ConfigDocument.class);
            } catch (TextFormat.ParseException e) {
                throw new AgentRpcException(Status.Code.INVALID_ARGUMENT,
                        "config.invalid_document", "document_textproto: " + e.getMessage());
            }
        }

        String base64 = request.get("document_base64");
        if (base64 == null || base64.isBlank()) {
            throw new AgentRpcException(Status.Code.INVALID_ARGUMENT, "config.invalid_request",
                    "one of document_textproto or document_base64 is required");
        }
        try {
            return ConfigDocument.parseFrom(Base64.getDecoder().decode(base64));
        } catch (IllegalArgumentException | InvalidProtocolBufferException e) {
            throw new AgentRpcException(Status.Code.INVALID_ARGUMENT, "config.invalid_document",
                    "document_base64: " + e.getMessage());
        }
    }

    private static HttpStatus statusFor(AgentRpcException e) {
        return "config.too_large".equals(e.errorCode())
                ? HttpStatus.PAYLOAD_TOO_LARGE
                : HttpStatus.BAD_REQUEST;
    }
}
