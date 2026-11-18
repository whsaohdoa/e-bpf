package dev.train0.cc.control;

import static org.assertj.core.api.Assertions.assertThat;
import static org.assertj.core.api.Assertions.catchThrowable;
import static org.junit.jupiter.api.Assumptions.assumeTrue;

import io.grpc.ManagedChannel;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;
import io.grpc.netty.GrpcSslContexts;
import io.grpc.netty.NettyChannelBuilder;
import io.netty.handler.ssl.SslContextBuilder;
import io.probe.control.agent.v1.AgentControlGrpc;
import io.probe.control.agent.v1.GetCursorRequest;
import io.probe.control.agent.v1.GetCursorResponse;
import java.io.InputStream;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.List;
import java.util.concurrent.TimeUnit;
import javax.net.ssl.SSLException;
import org.junit.jupiter.api.AfterAll;
import org.junit.jupiter.api.BeforeAll;
import org.junit.jupiter.api.Test;
import org.junit.jupiter.api.TestInstance;
import org.junit.jupiter.api.condition.EnabledIfEnvironmentVariable;
import org.springframework.beans.factory.annotation.Autowired;
import org.springframework.boot.grpc.test.autoconfigure.LocalGrpcServerPort;
import org.springframework.boot.test.context.SpringBootTest;
import org.springframework.jdbc.core.JdbcTemplate;

/**
 * End-to-end proof that the agent control plane speaks mutual TLS (design §4.1, plan T3.4).
 *
 * <p>The real server is started on a random port with the PEM bundle from {@code application.yml}
 * ({@code client-auth: require}) and is dialled with three netty channels: no client certificate,
 * a certificate from an unrelated CA, and the trusted client certificate. The first two must die in
 * the TLS handshake; only the trusted channel may reach {@code GetCursor}.
 *
 * <p>Requires the helper-provided {@code CC_TLS_DIR} fixture directory and a reachable PostgreSQL
 * (the app context boots the full control role). Gated on {@code CC_IT_PG=1} at class level so CI
 * (no PostgreSQL) skips it instead of trying to load the context with an unresolved
 * {@code CC_DB_URL}; the fixture check below only runs once that gate is satisfied.
 */
@SpringBootTest(
        webEnvironment = SpringBootTest.WebEnvironment.RANDOM_PORT,
        properties = {"spring.grpc.server.port=0"})
@EnabledIfEnvironmentVariable(named = "CC_IT_PG", matches = "1")
@TestInstance(TestInstance.Lifecycle.PER_CLASS)
class MutualTlsTest {

    /** Agent id carried as the CN of the trusted client certificate. */
    private static final String AGENT_ID = "agent-m1";

    @LocalGrpcServerPort
    private int grpcPort;

    @Autowired
    private JdbcTemplate jdbc;

    private Path tlsDir;
    private String clientFingerprint;

    @BeforeAll
    void seedTrustedAgent() throws Exception {
        String configured = System.getenv("CC_TLS_DIR");
        assumeTrue(configured != null && !configured.isBlank(), "CC_TLS_DIR is not set");
        tlsDir = Path.of(configured);
        for (String name : List.of("ca.crt", "client.crt", "client.key")) {
            assumeTrue(Files.isReadable(tlsDir.resolve(name)),
                    () -> "missing TLS fixture " + name + " under " + configured);
        }
        // The database row is what the identity interceptor compares the presented certificate
        // against, so it has to exist before the trusted channel is opened.
        clientFingerprint = fingerprintOf(tlsDir.resolve("client.crt"));
        jdbc.update("""
                insert into control.agent (agent_id, cert_fingerprint, status, group_id)
                values (?, ?, 'active', 'it-mtls')
                on conflict (agent_id) do update
                set cert_fingerprint = excluded.cert_fingerprint,
                    status = excluded.status,
                    group_id = excluded.group_id
                """, AGENT_ID, clientFingerprint);
    }

    @AfterAll
    void removeTrustedAgent() {
        if (tlsDir == null) {
            return;
        }
        try {
            jdbc.update("delete from control.agent where agent_id = ?", AGENT_ID);
        } catch (RuntimeException e) {
            // A leftover test row must not mask the TLS assertions; other tests own their cleanup.
        }
    }

    @Test
    void channelWithoutClientCertificateIsRejectedDuringHandshake() throws Exception {
        ManagedChannel channel = newChannel(GrpcSslContexts.forClient()
                .trustManager(tlsDir.resolve("ca.crt").toFile()));
        try {
            assertHandshakeRejected(channel);
        } finally {
            channel.shutdownNow();
        }
    }

    @Test
    void certificateFromUntrustedAuthorityIsRejectedDuringHandshake() throws Exception {
        ManagedChannel channel = newChannel(GrpcSslContexts.forClient()
                .trustManager(tlsDir.resolve("ca.crt").toFile())
                .keyManager(tlsDir.resolve("rogue-client.crt").toFile(),
                        tlsDir.resolve("rogue-client.key").toFile()));
        try {
            assertHandshakeRejected(channel);
        } finally {
            channel.shutdownNow();
        }
    }

    @Test
    void trustedClientCertificateCanCallGetCursor() throws Exception {
        ManagedChannel channel = newChannel(GrpcSslContexts.forClient()
                .trustManager(tlsDir.resolve("ca.crt").toFile())
                .keyManager(tlsDir.resolve("client.crt").toFile(),
                        tlsDir.resolve("client.key").toFile()));
        try {
            GetCursorResponse response = AgentControlGrpc.newBlockingStub(channel)
                    .withDeadlineAfter(30, TimeUnit.SECONDS)
                    .getCursor(GetCursorRequest.getDefaultInstance());

            assertThat(response.getServerTime().getSeconds()).isPositive();
        } finally {
            channel.shutdownNow();
        }
    }

    private ManagedChannel newChannel(SslContextBuilder ssl) throws SSLException {
        return NettyChannelBuilder.forAddress("127.0.0.1", grpcPort)
                .sslContext(ssl.build())
                .build();
    }

    /**
     * The handshake must fail before the RPC reaches the service, so the client sees the transport
     * failure as {@code UNAVAILABLE} rather than an application status.
     */
    private static void assertHandshakeRejected(ManagedChannel channel) {
        Throwable thrown = catchThrowable(() -> AgentControlGrpc.newBlockingStub(channel)
                .withDeadlineAfter(20, TimeUnit.SECONDS)
                .getCursor(GetCursorRequest.getDefaultInstance()));

        assertThat(thrown)
                .as("an untrusted channel must not reach the service")
                .isInstanceOf(StatusRuntimeException.class);
        StatusRuntimeException failure = (StatusRuntimeException) thrown;
        assertThat(failure.getStatus().getCode()).isEqualTo(Status.Code.UNAVAILABLE);
        assertThat(isTlsFailure(failure))
                .as("failure must come from the TLS handshake, not from the application: %s", thrown)
                .isTrue();
    }

    private static boolean isTlsFailure(Throwable failure) {
        for (Throwable cause = failure; cause != null; cause = cause.getCause()) {
            if (cause instanceof SSLException) {
                return true;
            }
            String message = cause.getMessage();
            if (message != null && (message.contains("handshake") || message.contains("SSL"))) {
                return true;
            }
        }
        return false;
    }

    private static String fingerprintOf(Path certificateFile) throws Exception {
        try (InputStream in = Files.newInputStream(certificateFile)) {
            X509Certificate certificate = (X509Certificate) CertificateFactory.getInstance("X.509")
                    .generateCertificate(in);
            return AgentIdentityInterceptor.sha256Hex(certificate.getEncoded());
        }
    }
}
