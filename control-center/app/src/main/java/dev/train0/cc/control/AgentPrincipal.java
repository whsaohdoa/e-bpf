package dev.train0.cc.control;

import io.grpc.Context;

/**
 * Authenticated agent identity derived from the mTLS peer certificate and the
 * {@code control.agent} row it is bound to.
 *
 * <p>The transport interceptor puts one instance into the gRPC {@code Context};
 * write RPCs read it instead of trusting anything carried in the request message.
 *
 * @param agentId the certificate CN, which is the only source of the agent id
 * @param fingerprint lowercase SHA-256 hex of the peer certificate (64 chars)
 * @param groupId the configured group, or {@code null} for a pending agent
 * @param status one of {@code pending}, {@code active}, {@code revoked}
 */
public record AgentPrincipal(String agentId, String fingerprint, String groupId, String status) {

    /** Context key used by the interceptor and read by every RPC implementation. */
    public static final Context.Key<AgentPrincipal> CONTEXT_KEY = Context.key("cc.agent-principal");

    public static final String STATUS_PENDING = "pending";
    public static final String STATUS_ACTIVE = "active";
    public static final String STATUS_REVOKED = "revoked";

    public boolean pending() {
        return STATUS_PENDING.equals(status);
    }

    public boolean revoked() {
        return STATUS_REVOKED.equals(status);
    }
}
