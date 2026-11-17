package dev.train0.cc.control;

import io.grpc.Status;
import io.probe.control.agent.v1.ConfigDocument;
import io.probe.control.agent.v1.DefaultAction;
import io.probe.control.agent.v1.Rule;
import io.probe.control.agent.v1.RuleMatch;
import java.util.HashSet;
import java.util.Set;
import java.util.regex.Pattern;
import org.springframework.stereotype.Component;

/**
 * Contract §6.2 checks that proto cannot express, following the design §7 table. Every
 * rejection is {@code INVALID_ARGUMENT} with a stable {@code config.*} code, and the detail names
 * the offending field so an agent operator can fix the document without diffing protos.
 *
 * <p>Order is intentional: document-level fields first, then rules in declaration order. The
 * first violation wins, which keeps the error deterministic for identical input.
 */
@Component
class ConfigDocumentValidator {

    static final int MAX_RULES = 20;
    static final int MIN_WINDOW_MS = 1_000;
    static final int MAX_WINDOW_MS = 60_000;
    static final int MAX_SRC_PREFIX_V4 = 32;
    static final int MAX_SRC_PREFIX_V6 = 128;
    static final int MIN_PRIORITY = 1;
    static final int MAX_PRIORITY = 1_000;

    private static final Pattern RULE_ID = Pattern.compile("^[a-z0-9][a-z0-9._-]{0,31}$");

    void validate(ConfigDocument doc) {
        if (doc.getSchemaVersion() != 1) {
            // Only schema 1 exists in Train 0. A newer document must be rejected instead of
            // being interpreted with the old semantics (contract appendix B).
            throw reject("config.schema_unsupported",
                    "schema_version must be 1 but was " + doc.getSchemaVersion());
        }
        if (doc.getWindowMs() < MIN_WINDOW_MS || doc.getWindowMs() > MAX_WINDOW_MS) {
            throw reject("config.invalid_document",
                    "window_ms must be within 1000..60000 but was " + doc.getWindowMs());
        }
        if (doc.getGraceMs() > doc.getWindowMs()) {
            throw reject("config.invalid_document",
                    "grace_ms must be <= window_ms (" + doc.getWindowMs() + ") but was "
                            + doc.getGraceMs());
        }
        if (doc.getSrcPrefixV4() > MAX_SRC_PREFIX_V4) {
            throw reject("config.invalid_document",
                    "src_prefix_v4 must be within 0..32 but was " + doc.getSrcPrefixV4());
        }
        if (doc.getSrcPrefixV6() > MAX_SRC_PREFIX_V6) {
            throw reject("config.invalid_document",
                    "src_prefix_v6 must be within 0..128 but was " + doc.getSrcPrefixV6());
        }
        if (doc.getDefaultAction() != DefaultAction.DEFAULT_ACTION_PASS) {
            // Train 0 only ships allow-by-default; a DROP default would silently blackhole
            // traffic when an agent loses its config, so it stays unsupported for now.
            throw reject("config.unsupported_default_action",
                    "default_action must be DEFAULT_ACTION_PASS but was " + doc.getDefaultAction());
        }
        if (doc.getRulesCount() > MAX_RULES) {
            throw reject("config.invalid_document",
                    "rules count must be <= " + MAX_RULES + " but was " + doc.getRulesCount());
        }

        Set<String> seenRuleIds = new HashSet<>();
        Set<Integer> seenPriorities = new HashSet<>();
        for (int i = 0; i < doc.getRulesCount(); i++) {
            Rule rule = doc.getRules(i);
            String path = "rules[" + i + "].";
            if (rule.getRuleIndex() != i + 1) {
                throw reject("config.invalid_document",
                        path + "rule_index must be " + (i + 1) + " but was " + rule.getRuleIndex());
            }
            if (!RULE_ID.matcher(rule.getRuleId()).matches()) {
                throw reject("config.invalid_document",
                        path + "rule_id must match ^[a-z0-9][a-z0-9._-]{0,31}$ but was \""
                                + rule.getRuleId() + "\"");
            }
            if (!seenRuleIds.add(rule.getRuleId())) {
                throw reject("config.invalid_document",
                        path + "rule_id is already used by an earlier rule: " + rule.getRuleId());
            }
            if (rule.getPriority() < MIN_PRIORITY || rule.getPriority() > MAX_PRIORITY) {
                throw reject("config.invalid_document",
                        path + "priority must be within 1..1000 but was " + rule.getPriority());
            }
            if (!seenPriorities.add(rule.getPriority())) {
                throw reject("config.invalid_document",
                        path + "priority is already used by an earlier rule: " + rule.getPriority());
            }
            RuleMatch match = rule.getMatch();
            for (int j = 0; j < match.getDstPortsCount(); j++) {
                int port = match.getDstPorts(j);
                if (port < 1 || port > 65_535) {
                    throw reject("config.invalid_document",
                            path + "match.dst_ports[" + j + "] must be within 1..65535 but was "
                                    + port);
                }
            }
        }
    }

    private static AgentRpcException reject(String errorCode, String detail) {
        return new AgentRpcException(Status.Code.INVALID_ARGUMENT, errorCode, detail);
    }
}
