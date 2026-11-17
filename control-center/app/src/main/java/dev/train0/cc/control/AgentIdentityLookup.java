package dev.train0.cc.control;

import java.util.Optional;

/**
 * Read-only lookup used by {@link AgentIdentityInterceptor} to bind a peer
 * certificate to a registered agent.
 *
 * <p>It is a separate functional interface so the interceptor can be unit tested
 * without a database and so the JDBC repository stays the only SQL owner.
 */
@FunctionalInterface
public interface AgentIdentityLookup {

    /**
     * @param agentId certificate CN
     * @return the agent row as a principal, or empty when the CN is unknown
     */
    Optional<AgentPrincipal> find(String agentId);

    /** Lookup that never finds anything, for wiring tests. */
    static AgentIdentityLookup none() {
        return agentId -> Optional.empty();
    }
}
