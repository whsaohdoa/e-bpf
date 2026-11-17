package dev.train0.cc.control;

import static dev.train0.cc.control.TestFixtures.AGENT_ID;
import static dev.train0.cc.control.TestFixtures.GROUP_G1;
import static dev.train0.cc.control.TestFixtures.GROUP_G2;
import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.catchThrowable;

import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;
import io.probe.control.agent.v1.AckConfigRequest;
import io.probe.control.agent.v1.AckResult;
import io.probe.control.agent.v1.AgentControlGrpc;
import io.probe.control.agent.v1.GetConfigRequest;
import io.probe.control.agent.v1.GetConfigResponse;
import io.probe.control.agent.v1.GetCursorRequest;
import io.probe.control.agent.v1.RegisterRequest;
import io.probe.control.agent.v1.ReportWindowsRequest;
import io.probe.control.agent.v1.Sample;
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
 * Failure contract of the agent-facing control plane (plan T3.3, design §5 and §6).
 *
 * <p>Every assertion pins both halves of the protocol: the gRPC status code and the
 * {@code x-error-code} trailer. The transport-level oversized-message case is expected to carry no
 * error-code trailer at all, because the netty codec rejects the request before any application
 * interceptor runs.
 */
@SpringBootTest(
        webEnvironment = SpringBootTest.WebEnvironment.RANDOM_PORT,
        properties = {"spring.grpc.server.port=0"})
@EnabledIfEnvironmentVariable(named = "CC_IT_PG", matches = "1")
class AgentControlErrorTest {

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
    void blankMachineIdIsRejected() {
        RegisterRequest request = RegisterRequest.newBuilder().setMachineId("  ").build();

        assertError(catchThrowable(() -> stub.register(request)),
                Status.Code.INVALID_ARGUMENT, "request.invalid");
    }

    @Test
    void registeringADifferentMachineIdConflictsWithTheBoundOne() {
        stub.register(RegisterRequest.newBuilder().setMachineId("machine-1").build());

        assertError(catchThrowable(() -> stub.register(
                        RegisterRequest.newBuilder().setMachineId("machine-2").build())),
                Status.Code.FAILED_PRECONDITION, "agent.machine_conflict");
        assertThat(jdbc.queryForObject(
                "select machine_id from control.agent where agent_id = ?", String.class, AGENT_ID))
                .isEqualTo("machine-1");
    }

    @Test
    void ackForUnpublishedVersionIsRejected() {
        var published = publisher.publish(GROUP_G1, TestFixtures.document("proto/doc_a.textproto"),
                "it");

        assertError(catchThrowable(() -> stub.ackConfig(AckConfigRequest.newBuilder()
                        .setVersion(published.version() + 3)
                        .setResult(AckResult.ACK_RESULT_APPLIED)
                        .build())),
                Status.Code.FAILED_PRECONDITION, "ack.version_unknown");
        assertThat(ackRows()).isZero();
    }

    @Test
    void ackWithUnspecifiedResultIsRejected() {
        var published = publisher.publish(GROUP_G1, TestFixtures.document("proto/doc_a.textproto"),
                "it");

        assertError(catchThrowable(() -> stub.ackConfig(AckConfigRequest.newBuilder()
                        .setVersion(published.version())
                        .build())),
                Status.Code.INVALID_ARGUMENT, "request.invalid");
        assertThat(ackRows()).isZero();
    }

    @Test
    void pendingAgentNeverSeesConfiguration() {
        publisher.publish(GROUP_G1, TestFixtures.document("proto/doc_a.textproto"), "it");
        jdbc.update("update control.agent set status = 'pending' where agent_id = ?", AGENT_ID);

        GetConfigResponse response = stub.getConfig(GetConfigRequest.newBuilder()
                .setCurrentVersion(0)
                .build());

        assertThat(response.getUpToDate()).isTrue();
        assertThat(response.hasConfig()).isFalse();
    }

    @Test
    void agentDoesNotSeeConfigurationOfAnotherGroup() {
        publisher.publish(GROUP_G2, TestFixtures.document("proto/doc_b.textproto"), "it");

        GetConfigResponse response = stub.getConfig(GetConfigRequest.newBuilder()
                .setCurrentVersion(0)
                .build());

        assertThat(response.getUpToDate()).isTrue();
        assertThat(response.hasConfig()).isFalse();
    }

    @Test
    void noPublishedConfigurationIsReportedAsUpToDate() {
        GetConfigResponse response = stub.getConfig(GetConfigRequest.newBuilder()
                .setCurrentVersion(0)
                .build());

        assertThat(response.getUpToDate()).isTrue();
        assertThat(response.hasConfig()).isFalse();
    }

    @Test
    void errorResponsesCarryRequestIdAndErrorCodeTrailers() {
        Throwable thrown = catchThrowable(() -> stub.ackConfig(AckConfigRequest.newBuilder()
                .setVersion(0)
                .build()));

        assertThat(thrown).isInstanceOf(StatusRuntimeException.class);
        Metadata trailers = ((StatusRuntimeException) thrown).getTrailers();
        assertThat(trailers).isNotNull();
        assertThat(trailers.get(TestFixtures.REQUEST_ID)).isNotBlank();
        assertThat(trailers.get(TestFixtures.ERROR_CODE)).isEqualTo("request.invalid");
    }

    @Test
    void oversizedReportRequestIsRejectedByTheTransport() {
        // The server caps inbound messages at 4 MiB (application.yml); 5 samples of 900 KiB push
        // the encoded request over that limit, so the netty codec answers RESOURCE_EXHAUSTED
        // without ever invoking the service or the application interceptors.
        String fiveTuple = "x".repeat(900_000);
        WindowReport.Builder window = WindowReport.newBuilder()
                .setWindowId(1)
                .setWindowMs(1000)
                .setComplete(true);
        for (int i = 0; i < 5; i++) {
            window.addSamples(Sample.newBuilder()
                    .setFiveTuple(fiveTuple)
                    .setTsOffsetNs(i)
                    .build());
        }

        Throwable thrown = catchThrowable(() -> stub.reportWindows(ReportWindowsRequest.newBuilder()
                .addWindows(window)
                .build()));

        assertThat(thrown).isInstanceOf(StatusRuntimeException.class);
        StatusRuntimeException failure = (StatusRuntimeException) thrown;
        assertThat(failure.getStatus().getCode()).isEqualTo(Status.Code.RESOURCE_EXHAUSTED);
        assertThat(TestFixtures.errorCode(failure))
                .as("transport rejections carry no application error code")
                .isNull();
    }

    @Test
    void revokedAgentIsRejectedByTheIdentityInterceptor() {
        jdbc.update("update control.agent set status = 'revoked' where agent_id = ?", AGENT_ID);

        assertError(catchThrowable(() -> stub.getCursor(GetCursorRequest.getDefaultInstance())),
                Status.Code.FAILED_PRECONDITION, "agent.revoked");
    }

    private int ackRows() {
        Integer count = jdbc.queryForObject(
                "select count(*) from control.config_ack where agent_id = ?",
                Integer.class, AGENT_ID);
        return count == null ? 0 : count;
    }

    private static void assertError(Throwable thrown, Status.Code expectedCode,
            String expectedErrorCode) {
        assertThat(thrown).isInstanceOf(StatusRuntimeException.class);
        StatusRuntimeException failure = (StatusRuntimeException) thrown;
        assertThat(failure.getStatus().getCode()).isEqualTo(expectedCode);
        assertThat(TestFixtures.errorCode(failure)).isEqualTo(expectedErrorCode);
    }
}
