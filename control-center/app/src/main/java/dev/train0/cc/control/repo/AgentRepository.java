package dev.train0.cc.control.repo;

import dev.train0.cc.control.AgentIdentityLookup;
import dev.train0.cc.control.AgentPrincipal;
import java.util.List;
import java.util.Optional;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.annotation.Transactional;

/**
 * JDBC access to {@code control.agent}.
 *
 * <p>Implements {@link AgentIdentityLookup} so the mTLS interceptor can resolve a certificate CN
 * without knowing about SQL. The mutating RPCs use the same repository: {@code Register} binds the
 * machine id under a row lock, {@code Heartbeat} refreshes the denormalised "latest known state".
 *
 * <p>Row-level locking is confined to {@code Register}: it is the only RPC where two concurrent
 * calls for the same agent could disagree about the bound machine, and the lock has to be taken
 * inside the same transaction as the update to be effective.
 */
@Repository
public class AgentRepository implements AgentIdentityLookup {

    private final JdbcTemplate jdbc;
    private final ConfigRepository configs;

    public AgentRepository(JdbcTemplate jdbc, ConfigRepository configs) {
        this.jdbc = jdbc;
        this.configs = configs;
    }

    @Override
    public Optional<AgentPrincipal> find(String agentId) {
        return jdbc.query("""
                select agent_id, cert_fingerprint, group_id, status
                from control.agent
                where agent_id = ?
                """,
                (rs, rowNum) -> new AgentPrincipal(
                        rs.getString("agent_id"),
                        rs.getString("cert_fingerprint"),
                        rs.getString("group_id"),
                        rs.getString("status")),
                agentId)
                .stream()
                .findFirst();
    }

    /**
     * Binds the machine id on first registration and refreshes the enrollment facts afterwards.
     *
     * <p>The row is locked {@code FOR UPDATE} before the conflict decision, so two agents racing
     * with different machine ids cannot both pass the check. Re-registering with the same machine
     * id is idempotent: the columns are overwritten and the response is derived from the committed
     * row, not from the request.
     *
     * @param capabilitiesJson JSON object for {@code capabilities::jsonb}
     * @return {@code null} when the agent row disappeared between authentication and this call
     */
    @Transactional
    public RegisterOutcome register(String agentId, String machineId, String agentVersion,
            String bootId, String capabilitiesJson) {
        List<RegistrationRow> rows = jdbc.query("""
                select machine_id, group_id, status
                from control.agent
                where agent_id = ?
                for update
                """,
                (rs, rowNum) -> new RegistrationRow(
                        rs.getString("machine_id"),
                        rs.getString("group_id"),
                        rs.getString("status")),
                agentId);
        if (rows.isEmpty()) {
            return null;
        }
        RegistrationRow row = rows.get(0);
        if (row.machineId() != null && !row.machineId().equals(machineId)) {
            return new RegisterOutcome(true, row.machineId(), row.groupId(), row.status(), null);
        }

        jdbc.update("""
                update control.agent
                set machine_id = coalesce(machine_id, ?::text),
                    agent_version = ?::text,
                    boot_id = ?::text,
                    capabilities = ?::jsonb,
                    registered_at = now(),
                    last_heartbeat = now()
                where agent_id = ?
                """, machineId, agentVersion, bootId, capabilitiesJson, agentId);

        String boundMachineId = row.machineId() == null ? machineId : row.machineId();
        return new RegisterOutcome(false, boundMachineId, row.groupId(), row.status(),
                configs.latest(row.groupId()));
    }

    /**
     * Refreshes the heartbeat facts and returns the newest config for the agent's group.
     *
     * <p>{@code applied_version} only ever moves forward and only from the request's
     * {@code current_version}: the agent is trusted about what it applied, nothing else. The
     * desired version returned to the caller is the committed state, so a config published
     * concurrently is picked up on the next heartbeat rather than half-visible in this one.
     */
    @Transactional
    public HeartbeatOutcome heartbeat(String agentId, long currentVersion, String configState) {
        int appliedVersion = (int) Math.min(currentVersion, Integer.MAX_VALUE);
        List<String> groups = jdbc.query("""
                update control.agent
                set last_heartbeat = now(),
                    config_state = ?::text,
                    applied_version = GREATEST(applied_version, ?)
                where agent_id = ?
                returning group_id
                """,
                (rs, rowNum) -> rs.getString("group_id"),
                configState, appliedVersion, agentId);
        if (groups.isEmpty()) {
            return null;
        }
        return new HeartbeatOutcome(configs.latest(groups.get(0)));
    }

    /** Raw enrollment columns needed to answer {@code Register}. */
    private record RegistrationRow(String machineId, String groupId, String status) {}

    /**
     * @param machineConflict the row is bound to a different machine id; nothing was written
     * @param machineId the machine id now bound to the agent
     * @param desired newest config for the agent's group ({@code null} when none)
     */
    public record RegisterOutcome(boolean machineConflict, String machineId, String groupId,
            String status, ConfigRepository.DesiredConfig desired) {}

    /** @param desired newest config for the agent's group ({@code null} when none) */
    public record HeartbeatOutcome(ConfigRepository.DesiredConfig desired) {}
}
