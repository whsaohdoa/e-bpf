package dev.train0.cc.control.repo;

import java.time.Instant;
import java.sql.Timestamp;
import java.util.ArrayList;
import java.util.List;
import java.util.Optional;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.stereotype.Repository;
import org.springframework.transaction.annotation.Transactional;

/**
 * JDBC access to the report intake tables ({@code report_window}, {@code report},
 * {@code report_sample}) plus the per-agent cursor exposed by {@code GetCursor}.
 *
 * <p>Intake is idempotent by construction: the agent may replay a window after a lost response, so
 * every insert is {@code on conflict do nothing} and the affected row count is what distinguishes a
 * first delivery from a duplicate. The window header is written before its aggregates because the
 * composite foreign key on {@code control.report} makes the order mandatory; a window with zero
 * aggregates is still a real window (the cursor has to advance over it).
 *
 * <p>The batch shape (count and ordering) is validated by {@code AgentControlService} before this
 * repository is called, so a bad batch never opens a transaction; everything below the service is
 * plain JDBC.
 */
@Repository
public class ReportRepository {

    /** A window this far behind the agent's stored cursor is expired and is never stored. */
    static final long WINDOW_RETENTION = 1000;

    private final JdbcTemplate jdbc;

    public ReportRepository(JdbcTemplate jdbc) {
        this.jdbc = jdbc;
    }

    /**
     * Highest window id this agent ever delivered.
     *
     * @return empty when the agent never delivered a window, which {@code GetCursor} reports as an
     *         absent optional field rather than as zero
     */
    public Optional<Long> maxWindowId(String agentId) {
        Long max = jdbc.queryForObject(
                "select max(window_id) from control.report_window where agent_id = ?",
                Long.class, agentId);
        return Optional.ofNullable(max);
    }

    /**
     * Stores one validated batch and reports what happened to every window.
     *
     * <p>Runs in one transaction so that a replay cannot observe a half-written window: the header,
     * its aggregates and its samples commit together, and the cursor update is guaranteed to be at
     * least as high as every window stored by this batch.
     *
     * @return counts that satisfy {@code accepted + duplicates + rejected == windows.size()}
     */
    @Transactional
    public Intake recordWindows(String agentId, List<WindowPayload> windows) {
        long cursor = jdbc.queryForObject("""
                select coalesce(max(window_id), 0)
                from control.report_window
                where agent_id = ?
                """, Long.class, agentId);

        int accepted = 0;
        int duplicates = 0;
        List<Long> rejectedWindowIds = new ArrayList<>();
        long highestWindowId = 0;
        for (WindowPayload window : windows) {
            if (window.windowId() + WINDOW_RETENTION < cursor) {
                rejectedWindowIds.add(window.windowId());
                continue;
            }

            int headerRows = jdbc.update("""
                    insert into control.report_window
                        (agent_id, window_id, window_ms, complete, window_start, truncated)
                    values (?, ?, ?, ?, ?::timestamptz, ?)
                    on conflict (agent_id, window_id) do nothing
                    """,
                    agentId, window.windowId(), window.windowMs(), window.complete(),
                    window.windowStart() == null ? null : Timestamp.from(window.windowStart()),
                    window.truncated());

            int aggregateRows = 0;
            for (AggregatePayload aggregate : window.aggregates()) {
                aggregateRows += jdbc.update("""
                        insert into control.report
                            (agent_id, window_id, key_hash, packets, first_seen_offset_ns,
                             last_seen_offset_ns, sample_dropped, degraded)
                        values (?, ?, ?, ?, ?, ?, ?, ?)
                        on conflict (agent_id, window_id, key_hash) do nothing
                        """,
                        agentId, window.windowId(), aggregate.keyHash(), aggregate.packets(),
                        aggregate.firstSeenOffsetNs(), aggregate.lastSeenOffsetNs(),
                        aggregate.sampleDropped(), aggregate.degraded());
            }

            int sampleRows = 0;
            for (SamplePayload sample : window.samples()) {
                sampleRows += jdbc.update("""
                        insert into control.report_sample
                            (agent_id, window_id, key_hash, five_tuple, ts_offset_ns)
                        values (?, ?, ?, ?, ?)
                        on conflict (agent_id, window_id, key_hash, ts_offset_ns) do nothing
                        """,
                        agentId, window.windowId(), sample.keyHash(), sample.fiveTuple(),
                        sample.tsOffsetNs());
            }

            if (headerRows + aggregateRows + sampleRows == 0) {
                duplicates++;
            } else {
                accepted++;
                if (aggregateRows > 0) {
                    jdbc.update("""
                            update control.report_window
                            set row_count = row_count + ?
                            where agent_id = ? and window_id = ?
                            """, aggregateRows, agentId, window.windowId());
                }
            }
            highestWindowId = Math.max(highestWindowId, window.windowId());
        }

        if (accepted + duplicates > 0) {
            jdbc.update("""
                    update control.agent
                    set reported_cursor = GREATEST(reported_cursor, ?)
                    where agent_id = ?
                    """, highestWindowId, agentId);
        }

        long lastWindowId = jdbc.queryForObject("""
                select coalesce(max(window_id), 0)
                from control.report_window
                where agent_id = ?
                """, Long.class, agentId);
        return new Intake(accepted, duplicates, rejectedWindowIds.size(), rejectedWindowIds,
                lastWindowId);
    }

    /**
     * One window as sent by the agent, already converted out of the protobuf message so this class
     * never depends on the generated types.
     *
     * @param windowStart display-only wall clock of the window start; may be {@code null}
     */
    public record WindowPayload(long windowId, int windowMs, boolean complete, Instant windowStart,
            boolean truncated, List<AggregatePayload> aggregates, List<SamplePayload> samples) {}

    /** One aggregate row; {@code keyHash} is exactly the 32 opaque bytes from the contract. */
    public record AggregatePayload(byte[] keyHash, long packets, long firstSeenOffsetNs,
            long lastSeenOffsetNs, boolean sampleDropped, boolean degraded) {}

    /** One sample row; absent or empty {@code keyHash} is stored as sent. */
    public record SamplePayload(byte[] keyHash, String fiveTuple, long tsOffsetNs) {}

    /**
     * Outcome of one batch.
     *
     * @param lastWindowId highest window id now stored for the agent ({@code 0} when none)
     */
    public record Intake(int accepted, int duplicates, int rejected, List<Long> rejectedWindowIds,
            long lastWindowId) {}
}
