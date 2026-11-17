package dev.train0.cc.control;

import static dev.train0.cc.control.TestFixtures.AGENT_ID;
import static dev.train0.cc.control.TestFixtures.GROUP_G1;
import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.catchThrowable;

import io.grpc.ManagedChannel;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;
import io.probe.control.agent.v1.AgentControlGrpc;
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
 * Idempotent report intake (plan T3.5, design §6.5).
 *
 * <p>The agent may replay a batch after a lost response, so every insert is conflict-tolerant and
 * the response accounts for each window exactly once: {@code accepted + duplicates + rejected} must
 * equal the number of windows in the request. Row counts must not grow on replay.
 */
@SpringBootTest(
        webEnvironment = SpringBootTest.WebEnvironment.RANDOM_PORT,
        properties = {"spring.grpc.server.port=0"})
@EnabledIfEnvironmentVariable(named = "CC_IT_PG", matches = "1")
class ReportIdempotencyTest {

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
    void replayOfIdenticalBatchIsAllDuplicates() {
        ReportWindowsRequest batch = ReportWindowsRequest.newBuilder()
                .addWindows(windowWithAggregateAndSample(1, 1))
                .addWindows(windowWithAggregate(2, 2))
                .build();

        ReportWindowsResponse first = stub.reportWindows(batch);
        assertThat(first.getAccepted()).isEqualTo(2);
        assertThat(first.getDuplicates()).isZero();
        assertThat(first.getRejected()).isZero();
        assertThat(first.getLastWindowId()).isEqualTo(2);

        ReportWindowsResponse replay = stub.reportWindows(batch);

        assertThat(replay.getAccepted()).isZero();
        assertThat(replay.getDuplicates()).isEqualTo(2);
        assertThat(replay.getRejected()).isZero();
        assertThat(replay.getLastWindowId()).isEqualTo(2);
        assertThat(accounts(replay, batch)).isTrue();
        assertThat(count("control.report_window")).isEqualTo(2);
        assertThat(count("control.report")).isEqualTo(2);
        assertThat(count("control.report_sample")).isEqualTo(1);
        assertThat(sumRowCount()).as("replay must not inflate row_count").isEqualTo(2);
    }

    @Test
    void partiallyNewBatchAcceptsOnlyTheNewWindow() {
        stub.reportWindows(ReportWindowsRequest.newBuilder()
                .addWindows(windowWithAggregate(1, 1))
                .addWindows(windowWithAggregate(2, 2))
                .build());

        ReportWindowsRequest next = ReportWindowsRequest.newBuilder()
                .addWindows(windowWithAggregate(2, 2))
                .addWindows(windowWithAggregate(3, 3))
                .build();
        ReportWindowsResponse response = stub.reportWindows(next);

        assertThat(response.getAccepted()).isEqualTo(1);
        assertThat(response.getDuplicates()).isEqualTo(1);
        assertThat(response.getRejected()).isZero();
        assertThat(response.getLastWindowId()).isEqualTo(3);
        assertThat(accounts(response, next)).isTrue();
        assertThat(count("control.report_window")).isEqualTo(3);
    }

    @Test
    void batchesAboveTheWindowLimitAreRejectedBeforeAnyWrite() {
        ReportWindowsRequest.Builder batch = ReportWindowsRequest.newBuilder();
        for (int windowId = 1; windowId <= 1001; windowId++) {
            batch.addWindows(TestFixtures.window(windowId));
        }

        Throwable thrown = catchThrowable(() -> stub.reportWindows(batch.build()));

        assertThat(thrown).isInstanceOf(StatusRuntimeException.class);
        StatusRuntimeException failure = (StatusRuntimeException) thrown;
        assertThat(failure.getStatus().getCode()).isEqualTo(Status.Code.RESOURCE_EXHAUSTED);
        assertThat(TestFixtures.errorCode(failure)).isEqualTo("report.batch_too_large");
        assertThat(count("control.report_window")).isZero();
    }

    @Test
    void outOfOrderWindowsRejectTheWholeBatch() {
        ReportWindowsRequest batch = ReportWindowsRequest.newBuilder()
                .addWindows(windowWithAggregate(5, 5))
                .addWindows(windowWithAggregate(4, 4))
                .build();

        Throwable thrown = catchThrowable(() -> stub.reportWindows(batch));

        assertThat(thrown).isInstanceOf(StatusRuntimeException.class);
        StatusRuntimeException failure = (StatusRuntimeException) thrown;
        assertThat(failure.getStatus().getCode()).isEqualTo(Status.Code.INVALID_ARGUMENT);
        assertThat(TestFixtures.errorCode(failure)).isEqualTo("report.malformed");
        assertThat(count("control.report_window")).isZero();
        assertThat(count("control.report")).isZero();
    }

    private static WindowReport windowWithAggregate(long windowId, int keySeed) {
        return TestFixtures.window(windowId).toBuilder()
                .addAggregates(TestFixtures.aggregate(keySeed, 10, 0, 900_000_000))
                .build();
    }

    private static WindowReport windowWithAggregateAndSample(long windowId, int keySeed) {
        return windowWithAggregate(windowId, keySeed).toBuilder()
                .addSamples(TestFixtures.sample(keySeed, "10.0.0.1:1234->10.0.0.2:80", 1_000))
                .build();
    }

    private static boolean accounts(ReportWindowsResponse response, ReportWindowsRequest request) {
        return response.getAccepted() + response.getDuplicates() + response.getRejected()
                == request.getWindowsCount();
    }

    private int count(String table) {
        Integer value = jdbc.queryForObject("select count(*) from " + table + " where agent_id = ?",
                Integer.class, AGENT_ID);
        return value == null ? 0 : value;
    }

    private int sumRowCount() {
        Integer value = jdbc.queryForObject(
                "select coalesce(sum(row_count), 0) from control.report_window where agent_id = ?",
                Integer.class, AGENT_ID);
        return value == null ? 0 : value;
    }
}
