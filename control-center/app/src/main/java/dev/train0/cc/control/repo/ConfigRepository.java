package dev.train0.cc.control.repo;

import java.time.Instant;
import java.util.List;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;

/**
 * Read access to the immutable configuration versions in {@code control.config_version}.
 *
 * <p>Writing a version belongs to {@code ConfigPublisher}, which owns the publish transaction; the
 * agent RPCs only ever need the newest row for the agent's group. The query is a plain
 * {@code order by version desc limit 1} and is served by {@code config_version_group_idx}, so it
 * stays a single index scan regardless of how many versions the group accumulated.
 *
 * <p>The record keeps the stored text form of the sha256: the wire contract uses 32 raw bytes, so
 * parsing happens once in the RPC layer where the protobuf message is assembled.
 */
@Repository
public class ConfigRepository {

    private final JdbcTemplate jdbc;

    public ConfigRepository(JdbcTemplate jdbc) {
        this.jdbc = jdbc;
    }

    /**
     * @param groupId group to look up; {@code null} when the agent has no group yet
     * @return the highest published version for the group, or {@code null} when the group has none
     */
    public DesiredConfig latest(String groupId) {
        if (groupId == null) {
            return null;
        }
        List<DesiredConfig> rows = jdbc.query("""
                select version, content_sha256, schema_version, document, published_at
                from control.config_version
                where group_id = ?
                order by version desc
                limit 1
                """,
                (rs, rowNum) -> new DesiredConfig(
                        rs.getInt("version"),
                        rs.getString("content_sha256"),
                        rs.getInt("schema_version"),
                        rs.getBytes("document"),
                        rs.getTimestamp("published_at").toInstant()),
                groupId);
        return rows.isEmpty() ? null : rows.get(0);
    }

    /**
     * Newest immutable configuration version of one group.
     *
     * @param version sequence-assigned version number ({@code 0} is never published)
     * @param contentSha256 lowercase hex sha256 of exactly {@link #document()}
     * @param schemaVersion schema version the document was validated against
     * @param document serialized {@code ConfigDocument}; served to agents byte for byte
     * @param publishedAt publish timestamp with millisecond precision
     */
    public record DesiredConfig(int version, String contentSha256, int schemaVersion, byte[] document,
            Instant publishedAt) {}
}
