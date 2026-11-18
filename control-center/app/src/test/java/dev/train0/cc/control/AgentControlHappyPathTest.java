package dev.train0.cc.control;

import static dev.train0.cc.control.TestFixtures.AGENT_ID;
import static dev.train0.cc.control.TestFixtures.GROUP_G1;
import static org.assertj.core.api.Assertions.assertThat;

import dev.train0.cc.control.TestFixtures;
import io.grpc.ManagedChannel;
import io.probe.control.agent.v1.AckConfigRequest;
import io.probe.control.agent.v1.AckConfigResponse;
import io.probe.control.agent.v1.AckResult;
import io.probe.control.agent.v1.AgentControlGrpc;
import io.probe.control.agent.v1.AgentStatus;
import io.probe.control.agent.v1.Capabilities;
import io.probe.control.agent.v1.ConfigDocument;
import io.probe.control.agent.v1.DefaultAction;
import io.probe.control.agent.v1.GetConfigRequest;
import io.probe.control.agent.v1.GetConfigResponse;
import io.probe.control.agent.v1.HeartbeatRequest;
import io.probe.control.agent.v1.HeartbeatResponse;
import io.probe.control.agent.v1.RegisterRequest;
import io.probe.control.agent.v1.RegisterResponse;
import java.util.concurrent.TimeUnit;
import org.junit.jupiter.api.AfterEach;
import org.junit.jupiter.api.BeforeEach;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.grpc.test.autoconfigure.LocalGrpcServerPort;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.jdbc.core.JdbcTemplate;

/**
 * End-to-end happy path for the agent-facing control plane (plan T3.3).
 *
 * <p>Runs the real control-role application with mutual TLS and a real PostgreSQL, then drives the
 * RPCs through a netty channel that authenticates as {@code agent-m1} - the agent row is resolved by
 * certificate fingerprint just like in production.
 */
@SpringBootTest(
        webEnvironment = SpringBootTest.WebEnvironment.RANDOM_PORT,
        properties = {"spring.grpc.server.port=0"})
@EnabledIfEnvironmentVariable(named = "CC_IT_PG", matches = "1")
class AgentControlHappyPathTest {

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
    void registerBindsMachineIdOnFirstCall() {
        RegisterResponse response = stub.register(registerRequest("machine-1", "0.1.0", "boot-1"));

        assertThat(response.getAgentId()).isEqualTo(AGENT_ID);
        assertThat(response.getMachineId()).isEqualTo("machine-1");
        assertThat(response.getGroupId()).isEqualTo(GROUP_G1);
        assertThat(response.getStatus()).isEqualTo(AgentStatus.AGENT_STATUS_ACTIVE);
        assertThat(response.getHeartbeatIntervalMs()).isEqualTo(1000);
        assertThat(response.getIdleHeartbeatIntervalMs()).isEqualTo(5000);
        assertThat(response.getServerTime().getSeconds()).isPositive();
        assertThat(response.getEffectiveCapabilities().getConfigSchemaList()).containsExactly(1);
        assertThat(response.getEffectiveCapabilities().getDefaultActionList())
                .containsExactly(DefaultAction.DEFAULT_ACTION_PASS);
        assertThat(response.getEffectiveCapabilities().getRulesExecution()).isFalse();
        assertThat(machineIdInDatabase()).isEqualTo("machine-1");
        assertThat(column("registered_at")).isNotNull();
        assertThat(column("last_heartbeat")).isNotNull();
    }

    @Test
    void registerAgainWithSameMachineIdIsIdempotent() {
        stub.register(registerRequest("machine-1", "0.1.0", "boot-1"));

        RegisterResponse second = stub.register(registerRequest("machine-1", "0.2.0", "boot-2"));

        assertThat(second.getMachineId()).isEqualTo("machine-1");
        assertThat(second.getAgentId()).isEqualTo(AGENT_ID);
        assertThat(machineIdInDatabase()).isEqualTo("machine-1");
        assertThat(jdbc.queryForObject(
                "select agent_version from control.agent where agent_id = ?", String.class, AGENT_ID))
                .isEqualTo("0.2.0");
        assertThat(jdbc.queryForObject(
                "select boot_id from control.agent where agent_id = ?", String.class, AGENT_ID))
                .isEqualTo("boot-2");
        assertThat(rowCount()).isEqualTo(1);
    }

    @Test
    void heartbeatWhileLaggingAsksForFastPolling() {
        var published = publisher.publish(GROUP_G1, TestFixtures.document("proto/doc_a.textproto"),
                "it");

        HeartbeatResponse response = stub.heartbeat(HeartbeatRequest.newBuilder()
                .setCurrentVersion(0)
                .build());

        assertThat(response.getDesiredVersion()).isEqualTo(published.version());
        assertThat(response.getDesiredSha256()).isEqualTo(
                TestFixtures.shaBytes(published.contentSha256()));
        assertThat(response.getHeartbeatIntervalMs()).isEqualTo(1000);
        assertThat(response.getIdleHeartbeatIntervalMs()).isEqualTo(5000);
        assertThat(response.getServerTime().getSeconds()).isPositive();
        assertThat(jdbc.queryForObject("""
                select applied_version from control.agent where agent_id = ?
                """, Integer.class, AGENT_ID)).isZero();
    }

    @Test
    void heartbeatWhenCaughtUpUsesIdleInterval() {
        var published = publisher.publish(GROUP_G1, TestFixtures.document("proto/doc_a.textproto"),
                "it");

        HeartbeatResponse response = stub.heartbeat(HeartbeatRequest.newBuilder()
                .setCurrentVersion(published.version())
                .build());

        assertThat(response.getDesiredVersion()).isEqualTo(published.version());
        assertThat(response.getHeartbeatIntervalMs()).isEqualTo(5000);
        assertThat(response.getIdleHeartbeatIntervalMs()).isEqualTo(5000);
    }

    @Test
    void getConfigReturnsStoredBytesAndSha() {
        ConfigDocument document = TestFixtures.document("proto/doc_a.textproto");
        var published = publisher.publish(GROUP_G1, document, "it");

        GetConfigResponse response = stub.getConfig(GetConfigRequest.newBuilder()
                .setCurrentVersion(published.version() - 1)
                .build());

        assertThat(response.getUpToDate()).isFalse();
        assertThat(response.getConfig().getVersion()).isEqualTo(published.version());
        assertThat(response.getConfig().getSchemaVersion()).isEqualTo(document.getSchemaVersion());
        assertThat(response.getConfig().getContentSha256())
                .isEqualTo(TestFixtures.shaBytes(published.contentSha256()));
        assertThat(response.getConfig().getDocument().toByteArray())
                .isEqualTo(document.toByteArray());
        assertThat(response.getConfig().getPublishedAt().getSeconds()).isPositive();
    }

    @Test
    void ackConfigAppliedWritesBackAppliedVersion() {
        ConfigDocument document = TestFixtures.document("proto/doc_a.textproto");
        var published = publisher.publish(GROUP_G1, document, "it");

        AckConfigResponse response = stub.ackConfig(AckConfigRequest.newBuilder()
                .setVersion(published.version())
                .setContentSha256(TestFixtures.shaBytes(published.contentSha256()))
                .setResult(AckResult.ACK_RESULT_APPLIED)
                .setAppliedAt(TestFixtures.millis(System.currentTimeMillis()))
                .setApplyMs(12)
                .addDegradedFeatures("rules")
                .build());

        assertThat(response.getRecorded()).isTrue();
        assertThat(response.getDesiredVersion()).isEqualTo(published.version());
        assertThat(response.getReceivedAt().getSeconds()).isPositive();
        assertThat(response.getServerTime().getSeconds()).isPositive();
        assertThat(jdbc.queryForObject("""
                select applied_version from control.agent where agent_id = ?
                """, Integer.class, AGENT_ID)).isEqualTo(published.version());
        assertThat(jdbc.queryForObject("""
                select config_state from control.agent where agent_id = ?
                """, String.class, AGENT_ID)).isEqualTo("applied");
        assertThat(jdbc.queryForObject("""
                select result from control.config_ack where agent_id = ? and version = ?
                """, String.class, AGENT_ID, published.version())).isEqualTo("applied");
        assertThat(jdbc.queryForObject("""
                select apply_ms from control.config_ack where agent_id = ? and version = ?
                """, Integer.class, AGENT_ID, published.version())).isEqualTo(12);
    }

    private RegisterRequest registerRequest(String machineId, String agentVersion, String bootId) {
        return RegisterRequest.newBuilder()
                .setMachineId(machineId)
                .setAgentVersion(agentVersion)
                .setBootId(bootId)
                .setCapabilities(Capabilities.newBuilder()
                        .addConfigSchema(1)
                        .addDefaultAction(DefaultAction.DEFAULT_ACTION_PASS))
                .build();
    }

    private String machineIdInDatabase() {
        return jdbc.queryForObject("select machine_id from control.agent where agent_id = ?",
                String.class, AGENT_ID);
    }

    private Object column(String name) {
        return jdbc.queryForObject("select " + name + " from control.agent where agent_id = ?",
                Object.class, AGENT_ID);
    }

    private int rowCount() {
        Integer count = jdbc.queryForObject("select count(*) from control.agent where agent_id = ?",
                Integer.class, AGENT_ID);
        return count == null ? 0 : count;
    }
}
