package dev.train0.cc.control;

import static dev.train0.cc.control.TestFixtures.GROUP_G1;
import static org.assertj.core.api.Assertions.assertThat;

import io.grpc.ManagedChannel;
import io.probe.control.agent.v1.AgentControlGrpc;
import io.probe.control.agent.v1.ConfigDocument;
import io.probe.control.agent.v1.GetConfigRequest;
import io.probe.control.agent.v1.GetConfigResponse;
import io.probe.control.agent.v1.HeartbeatRequest;
import io.probe.control.agent.v1.HeartbeatResponse;
import java.util.Arrays;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.grpc.test.autoconfigure.LocalGrpcServerPort;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.jdbc.core.JdbcTemplate;

/**
 * G1 polling latency budget (plan T4.2): publish - heartbeat - fetch must stay well below the
 * three-second target the plan sets for the polling path.
 *
 * <p>Each iteration publishes one immutable version, asks the control plane whether the (lagging)
 * agent should poll faster, and then fetches the payload. The measured span covers exactly the
 * control-plane work an agent waits for on a new configuration: the publish commit, the heartbeat
 * round trip and the config fetch. Ten samples are used; {@code p95} is the
 * {@code ceil(0.95 * n)}-th smallest value, i.e. the maximum for n = 10.
 */
@SpringBootTest(
        webEnvironment = SpringBootTest.WebEnvironment.RANDOM_PORT,
        properties = {"spring.grpc.server.port=0"})
@EnabledIfEnvironmentVariable(named = "CC_IT_PG", matches = "1")
class PollingLatencyTest {

    private static final int ITERATIONS = 10;

    /** Plan T4.2 budget for the whole publish - heartbeat - fetch cycle. */
    private static final long P95_BUDGET_MS = 3000;

    @LocalGrpcServerPort
    private int grpcPort;

    @Autowired
    private JdbcTemplate jdbc;

    @Autowired
    private ConfigPublisher publisher;

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
    void publishHeartbeatFetchLatencyStaysWithinBudget() {
        ConfigDocument document = TestFixtures.document("proto/doc_a.textproto");
        long[] samples = new long[ITERATIONS];

        for (int iteration = 0; iteration < ITERATIONS; iteration++) {
            long startedNanos = System.nanoTime();

            var published = publisher.publish(GROUP_G1, document, "g1-it");
            HeartbeatResponse heartbeat = stub.heartbeat(HeartbeatRequest.newBuilder()
                    .setCurrentVersion(published.version() - 1L)
                    .build());
            assertThat(heartbeat.getDesiredVersion()).isEqualTo(published.version());
            assertThat(heartbeat.getHeartbeatIntervalMs()).isEqualTo(1000);

            GetConfigResponse config = stub.getConfig(GetConfigRequest.newBuilder()
                    .setCurrentVersion(published.version() - 1L)
                    .build());
            samples[iteration] = (System.nanoTime() - startedNanos) / 1_000_000L;

            assertThat(config.getUpToDate()).isFalse();
            assertThat(config.getConfig().getVersion()).isEqualTo(published.version());
            assertThat(config.getConfig().getDocument().toByteArray())
                    .isEqualTo(document.toByteArray());
        }

        long p95 = percentile95(samples);
        System.out.println("g1_p95_ms=" + p95);
        assertThat(p95)
                .as("G1 publish-heartbeat-fetch p95 latency in milliseconds (samples %s)",
                        Arrays.toString(samples))
                .isLessThanOrEqualTo(P95_BUDGET_MS);
    }

    /** {@code ceil(0.95 * n)}-th smallest sample, using 1-based ranks. */
    static long percentile95(long[] samples) {
        long[] sorted = samples.clone();
        Arrays.sort(sorted);
        int rank = (int) Math.ceil(0.95 * sorted.length);
        return sorted[Math.max(0, rank - 1)];
    }
}
