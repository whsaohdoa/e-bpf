package dev.train0.cc.control;

import static org.junit.jupiter.api.Assumptions.assumeTrue;

import com.google.protobuf.ByteString;
import com.google.protobuf.Timestamp;
import com.google.protobuf.TextFormat;
import io.grpc.ManagedChannel;
import io.grpc.Metadata;
import io.grpc.StatusRuntimeException;
import io.grpc.netty.GrpcSslContexts;
import io.grpc.netty.NettyChannelBuilder;
import io.netty.handler.ssl.SslContextBuilder;
import io.probe.control.agent.v1.Aggregate;
import io.probe.control.agent.v1.AgentControlGrpc;
import io.probe.control.agent.v1.ConfigDocument;
import io.probe.control.agent.v1.Sample;
import io.probe.control.agent.v1.WindowReport;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.security.MessageDigest;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.HexFormat;
import java.util.List;
import java.util.concurrent.TimeUnit;
import javax.net.ssl.SSLException;
import org.springframework.jdbc.core.JdbcTemplate;

/**
 * Shared fixtures for the mutual-TLS integration tests of {@code AgentControlService}.
 *
 * <p>The control plane starts with the real PEM bundle from {@code CC_TLS_DIR} and
 * {@code client-auth: require}, so every test talks to it through a netty channel that presents the
 * fixture client certificate (CN {@code agent-m1}). Authentication therefore resolves the agent row
 * by certificate fingerprint, which is exactly the production path - the tests never inject a
 * principal into the {@code Context}.
 */
final class TestFixtures {

    /** Agent id of the fixture client certificate (CN); the only agent these tests authenticate as. */
    static final String AGENT_ID = "agent-m1";

    static final String GROUP_G1 = "g1";
    static final String GROUP_G2 = "g2";

    /** Trailer carrying the machine-readable error code (design §5). */
    static final Metadata.Key<String> ERROR_CODE =
            Metadata.Key.of("x-error-code", Metadata.ASCII_STRING_MARSHALLER);

    /** Trailer carrying the per-request correlation id (design §5). */
    static final Metadata.Key<String> REQUEST_ID =
            Metadata.Key.of("x-request-id", Metadata.ASCII_STRING_MARSHALLER);

    private static final long RPC_DEADLINE_SECONDS = 30;

    private TestFixtures() {}

    /** Skips the test when the helper did not provide the development TLS fixtures. */
    static void assumeTlsAvailable() {
        Path dir = tlsDir();
        assumeTrue(dir != null, "CC_TLS_DIR is not set");
        for (String name : List.of("ca.crt", "client.crt", "client.key")) {
            assumeTrue(Files.isReadable(dir.resolve(name)),
                    () -> "missing TLS fixture " + name + " under " + dir);
        }
    }

    static Path tlsDir() {
        String configured = System.getenv("CC_TLS_DIR");
        return configured == null || configured.isBlank() ? null : Path.of(configured);
    }

    /** SHA-256 of the DER encoding of the fixture client certificate, lowercase hex. */
    static String clientFingerprint() {
        Path certificate = tlsDir().resolve("client.crt");
        try (InputStream in = Files.newInputStream(certificate)) {
            X509Certificate parsed = (X509Certificate) CertificateFactory.getInstance("X.509")
                    .generateCertificate(in);
            return HexFormat.of().formatHex(
                    MessageDigest.getInstance("SHA-256").digest(parsed.getEncoded()));
        } catch (Exception e) {
            throw new IllegalStateException("cannot fingerprint " + certificate, e);
        }
    }

    /** Removes every row these tests may have written for {@link #AGENT_ID} and the g1/g2 groups. */
    static void cleanup(JdbcTemplate jdbc) {
        jdbc.update("delete from control.report_sample where agent_id = ?", AGENT_ID);
        jdbc.update("delete from control.report where agent_id = ?", AGENT_ID);
        jdbc.update("delete from control.report_window where agent_id = ?", AGENT_ID);
        jdbc.update("delete from control.config_ack where agent_id = ?", AGENT_ID);
        jdbc.update("delete from control.agent where agent_id = ?", AGENT_ID);
        jdbc.update("delete from control.config_version where group_id in (?, ?)",
                GROUP_G1, GROUP_G2);
        jdbc.update("""
                delete from control.audit_outbox
                where payload->>'group_id' in (?, ?)
                """, GROUP_G1, GROUP_G2);
    }

    /**
     * Seeds the agent row the identity interceptor authenticates the fixture certificate against.
     *
     * <p>The fingerprint is computed from the certificate itself so the row and the channel can
     * never drift apart.
     */
    static void seedAgent(JdbcTemplate jdbc, String status, String groupId) {
        cleanup(jdbc);
        jdbc.update("""
                insert into control.agent
                    (agent_id, cert_fingerprint, status, group_id, reported_cursor)
                values (?, ?, ?, ?, 0)
                """, AGENT_ID, clientFingerprint(), status, groupId);
    }

    static ManagedChannel channel(int grpcPort) throws SSLException {
        Path tls = tlsDir();
        SslContextBuilder ssl = GrpcSslContexts.forClient()
                .trustManager(tls.resolve("ca.crt").toFile())
                .keyManager(tls.resolve("client.crt").toFile(), tls.resolve("client.key").toFile());
        return NettyChannelBuilder.forAddress("127.0.0.1", grpcPort)
                .sslContext(ssl.build())
                .build();
    }

    static AgentControlGrpc.AgentControlBlockingStub stub(ManagedChannel channel) {
        return AgentControlGrpc.newBlockingStub(channel)
                .withDeadlineAfter(RPC_DEADLINE_SECONDS, TimeUnit.SECONDS);
    }

    static void shutdown(ManagedChannel channel) {
        if (channel == null) {
            return;
        }
        channel.shutdownNow();
        try {
            channel.awaitTermination(5, TimeUnit.SECONDS);
        } catch (InterruptedException e) {
            Thread.currentThread().interrupt();
        }
    }

    /** One of the two published test documents, parsed straight from the classpath fixture. */
    static ConfigDocument document(String resource) {
        try (InputStream in = TestFixtures.class.getClassLoader().getResourceAsStream(resource)) {
            if (in == null) {
                throw new IllegalStateException("classpath resource not found: " + resource);
            }
            return TextFormat.parse(new String(in.readAllBytes(), StandardCharsets.UTF_8),
                    ConfigDocument.class);
        } catch (Exception e) {
            throw new IllegalStateException("cannot parse " + resource, e);
        }
    }

    static ByteString shaBytes(String sha256Hex) {
        return ByteString.copyFrom(HexFormat.of().parseHex(sha256Hex));
    }

    /** The 32 opaque key-hash bytes the schema requires ({@code octet_length = 32}). */
    static ByteString keyHash(int seed) {
        byte[] bytes = new byte[32];
        for (int i = 0; i < bytes.length; i++) {
            bytes[i] = (byte) (seed + i);
        }
        return ByteString.copyFrom(bytes);
    }

    static Aggregate aggregate(int keySeed, long packets, long firstSeenOffsetNs,
            long lastSeenOffsetNs) {
        return Aggregate.newBuilder()
                .setKeyHash(keyHash(keySeed))
                .setPackets(packets)
                .setFirstSeenOffsetNs(firstSeenOffsetNs)
                .setLastSeenOffsetNs(lastSeenOffsetNs)
                .build();
    }

    static Sample sample(int keySeed, String fiveTuple, long tsOffsetNs) {
        return Sample.newBuilder()
                .setKeyHash(keyHash(keySeed))
                .setFiveTuple(fiveTuple)
                .setTsOffsetNs(tsOffsetNs)
                .build();
    }

    static WindowReport window(long windowId) {
        return WindowReport.newBuilder()
                .setWindowId(windowId)
                .setWindowMs(1000)
                .setComplete(true)
                .build();
    }

    static Timestamp millis(long epochMillis) {
        return Timestamp.newBuilder()
                .setSeconds(Math.floorDiv(epochMillis, 1000L))
                .setNanos((int) Math.floorMod(epochMillis, 1000L) * 1_000_000)
                .build();
    }

    /** Machine-readable error code of a failed RPC, or {@code null} when the trailer is absent. */
    static String errorCode(StatusRuntimeException failure) {
        Metadata trailers = failure.getTrailers();
        return trailers == null ? null : trailers.get(ERROR_CODE);
    }
}
