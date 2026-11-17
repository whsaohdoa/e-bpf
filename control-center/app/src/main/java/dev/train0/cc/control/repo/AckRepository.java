package dev.train0.cc.control.repo;

import java.sql.Timestamp;
import java.time.Instant;
import java.util.HexFormat;
import java.util.List;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.annotation.Transactional;

/**
 * JDBC access to {@code control.config_ack}, the per-version acknowledgement history.
 *
 * <p>The row is written once and then refreshed in place: a retried ack updates the result and
 * diagnostics but must not move {@code received_at}, because G1 is measured from the first control
 * plane clock reading of a version (design §6.4). The agent's view of "latest applied" lives on
 * {@code control.agent} and is only advanced by {@code applied} acks, again with
 * {@code GREATEST}/{@code COALESCE} so a replay cannot roll it back.
 */
@Repository
public class AckRepository {

    private final JdbcTemplate jdbc;
    private final ConfigRepository configs;

    public AckRepository(JdbcTemplate jdbc, ConfigRepository configs) {
        this.jdbc = jdbc;
        this.configs = configs;
    }

    /**
     * Records one acknowledgement and mirrors an {@code applied} result onto the agent row.
     *
     * @param contentSha256 the 32 wire bytes the agent compared; when absent or malformed the sha
     *        of the stored version is used instead, because the history row requires one
     * @param result one of {@code applied}, {@code rejected}, {@code failed} (validated upstream)
     * @param appliedAt agent wall clock for the human timeline; may be {@code null}
     * @return the outcome including the first-seen control-plane {@code received_at}, or
     *         {@code null} when the agent row disappeared
     */
    @Transactional
    public AckOutcome record(String agentId, long version, byte[] contentSha256, String result,
            Instant appliedAt, int applyMs, String[] degradedFeatures, String errorCode,
            String errorDetail) {
        List<String> groups = jdbc.query("select group_id from control.agent where agent_id = ?",
                (rs, rowNum) -> rs.getString("group_id"), agentId);
        if (groups.isEmpty()) {
            return null;
        }

        ConfigRepository.DesiredConfig desired = configs.latest(groups.get(0));
        int desiredVersion = desired == null ? 0 : desired.version();
        if (version > desiredVersion) {
            return new AckOutcome(AckStatus.VERSION_UNKNOWN, desiredVersion, null);
        }

        String contentHash = contentSha256 != null && contentSha256.length == 32
                ? HexFormat.of().formatHex(contentSha256)
                : desired == null ? null : desired.contentSha256();
        if (contentHash == null) {
            return new AckOutcome(AckStatus.INVALID_CONTENT_HASH, desiredVersion, null);
        }

        Timestamp appliedAtSql = appliedAt == null ? null : Timestamp.from(appliedAt);
        jdbc.update("""
                insert into control.config_ack
                    (agent_id, version, content_sha256, result, applied_at, apply_ms,
                     degraded_features, error_code, error_detail)
                values (?, ?, ?, ?, ?::timestamptz, ?, ?::text[], ?::text, ?::text)
                on conflict (agent_id, version) do update
                    set result = excluded.result,
                        content_sha256 = excluded.content_sha256,
                        applied_at = coalesce(control.config_ack.applied_at, excluded.applied_at),
                        apply_ms = excluded.apply_ms,
                        degraded_features = excluded.degraded_features,
                        error_code = excluded.error_code,
                        error_detail = excluded.error_detail
                """,
                agentId, (int) version, contentHash, result, appliedAtSql, applyMs,
                textArrayLiteral(degradedFeatures), errorCode, errorDetail);

        if ("applied".equals(result)) {
            jdbc.update("""
                    update control.agent
                    set applied_version = GREATEST(applied_version, ?),
                        applied_at = coalesce(applied_at, ?::timestamptz),
                        config_state = 'applied'
                    where agent_id = ?
                    """, (int) version, appliedAtSql, agentId);
        }

        Timestamp receivedAt = jdbc.queryForObject("""
                select received_at from control.config_ack
                where agent_id = ? and version = ?
                """, Timestamp.class, agentId, (int) version);
        return new AckOutcome(AckStatus.RECORDED, desiredVersion,
                receivedAt == null ? null : receivedAt.toInstant());
    }

    /** PostgreSQL text array literal for a JDBC varargs update (NULL is never needed here). */
    static String textArrayLiteral(String[] values) {
        StringBuilder out = new StringBuilder("{");
        for (int i = 0; i < values.length; i++) {
            if (i > 0) {
                out.append(',');
            }
            out.append('"')
                    .append(values[i].replace("\\", "\\\\").replace("\"", "\\\""))
                    .append('"');
        }
        return out.append('}').toString();
    }

    /** What happened to one ack; the service turns this into a gRPC status. */
    public enum AckStatus {
        /** Row persisted; {@link AckOutcome#receivedAt()} is authoritative. */
        RECORDED,
        /** The acked version is ahead of anything published for the agent's group. */
        VERSION_UNKNOWN,
        /** No usable content hash was supplied and the version has no stored sha either. */
        INVALID_CONTENT_HASH
    }

    /**
     * @param desiredVersion newest version for the agent's group ({@code 0} when none)
     * @param receivedAt control-plane clock of the first recorded ack, {@code null} unless recorded
     */
    public record AckOutcome(AckStatus status, int desiredVersion, Instant receivedAt) {}
}
