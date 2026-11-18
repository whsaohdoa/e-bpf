package dev.train0.cc.control;

import static dev.train0.cc.control.TestFixtures.AGENT_ID;
import static dev.train0.cc.control.TestFixtures.GROUP_G1;
import static org.assertj.core.api.Assertions.assertThat;

import io.grpc.ManagedChannel;
import io.probe.control.agent.v1.AgentControlGrpc;
import io.probe.control.agent.v1.GetCursorRequest;
import io.probe.control.agent.v1.GetCursorResponse;
import io.probe.control.agent.v1.ReportWindowsRequest;
import io.probe.control.agent.v1.ReportWindowsResponse;
import io.probe.control.agent.v1.WindowReport;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.grpc.test.autoconfigure.LocalGrpcServerPort;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.jdbc.core.JdbcTemplate;

/**
 * Cursor semantics of report intake (plan T3.5, design §6.5 and §6.6).
 *
 * <p>{@code GetCursor} is the authority for "what did the control plane already store": it reports
 * the highest stored window id, or leaves the optional field absent when the agent never delivered
 * anything. Windows far behind that cursor are expired and are rejected per window without a write.
 */
@SpringBootTest(
        webEnvironment = SpringBootTest.WebEnvironment.RANDOM_PORT,
        properties = {"spring.grpc.server.port=0"})
@EnabledIfEnvironmentVariable(named = "CC_IT_PG", matches = "1")
class ReportCursorTest {

    @LocalGrpcServerPort
    private int grpcPort;

    @Autowired
    private JdbcTemplate jdbc;

    private ManagedChannel channel;
    private AgentControlGrpc.AgentControlBlockingStub stub;

    @BeforeEach
    void setUp() throws Exception {
        TestFixtures.assumeTlsAvailable();
        TestFixtures.seedAgent(jdbc, AgentPrincipal.STATUS_ACTIVE, GROUP_G1);
        channel = TestFixtures.channel(grpcPort);
        stub = TestFixtures.stub(channel);
    }

    @AfterEach
    void tearDown() {
        TestFixtures.shutdown(channel);
        TestFixtures.cleanup(jdbc);
    }

    @Test
    void getCursorOmitsLastWindowIdBeforeTheFirstReport() {
        GetCursorResponse response = stub.getCursor(GetCursorRequest.getDefaultInstance());

        assertThat(response.hasLastWindowId()).isFalse();
        assertThat(response.getServerTime().getSeconds()).isPositive();
    }

    @Test
    void getCursorReturnsTheHighestStoredWindowId() {
        stub.reportWindows(ReportWindowsRequest.newBuilder()
                .addWindows(TestFixtures.window(7))
                .addWindows(TestFixtures.window(9))
                .build());

        GetCursorResponse response = stub.getCursor(GetCursorRequest.getDefaultInstance());

        assertThat(response.hasLastWindowId()).isTrue();
        assertThat(response.getLastWindowId()).isEqualTo(9);
        assertThat(jdbc.queryForObject("""
                select reported_cursor from control.agent where agent_id = ?
                """, Long.class, AGENT_ID)).isEqualTo(9L);
    }

    @Test
    void windowWithoutAggregatesStillAdvancesTheCursor() {
        ReportWindowsResponse response = stub.reportWindows(ReportWindowsRequest.newBuilder()
                .addWindows(TestFixtures.window(5))
                .build());

        assertThat(response.getAccepted()).isEqualTo(1);
        assertThat(response.getDuplicates()).isZero();
        assertThat(response.getLastWindowId()).isEqualTo(5);
        assertThat(stub.getCursor(GetCursorRequest.getDefaultInstance()).getLastWindowId())
                .isEqualTo(5);
        assertThat(jdbc.queryForObject("""
                select row_count from control.report_window where agent_id = ? and window_id = ?
                """, Integer.class, AGENT_ID, 5L)).isZero();
    }

    @Test
    void windowOlderThanTheRetentionWindowIsRejectedWithoutBeingStored() {
        stub.reportWindows(ReportWindowsRequest.newBuilder()
                .addWindows(TestFixtures.window(5000))
                .build());

        ReportWindowsRequest stale = ReportWindowsRequest.newBuilder()
                .addWindows(TestFixtures.window(3999))
                .build();
        ReportWindowsResponse response = stub.reportWindows(stale);

        assertThat(response.getAccepted()).isZero();
        assertThat(response.getDuplicates()).isZero();
        assertThat(response.getRejected()).isEqualTo(1);
        assertThat(response.getRejectedWindowsCount()).isEqualTo(1);
        assertThat(response.getRejectedWindows(0).getWindowId()).isEqualTo(3999);
        assertThat(response.getRejectedWindows(0).getErrorCode()).isEqualTo("report.window_too_old");
        assertThat(response.getLastWindowId()).isEqualTo(5000);
        assertThat(response.getAccepted() + response.getDuplicates() + response.getRejected())
                .isEqualTo(stale.getWindowsCount());
        assertThat(jdbc.queryForObject("""
                select count(*) from control.report_window where agent_id = ? and window_id = ?
                """, Integer.class, AGENT_ID, 3999L)).isZero();
    }
}
