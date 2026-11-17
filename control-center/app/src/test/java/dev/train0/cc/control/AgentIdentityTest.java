package dev.train0.cc.control;

import static org.assertj.core.api.Assertions.assertThat;

import io.grpc.Attributes;
import io.grpc.Grpc;
import io.grpc.Metadata;
import io.grpc.MethodDescriptor;
import io.grpc.ServerCall;
import io.grpc.ServerCallHandler;
import io.grpc.Status;
import io.probe.control.agent.v1.AgentControlGrpc;
import java.io.ByteArrayInputStream;
import java.io.InputStream;
import java.lang.reflect.Proxy;
import java.nio.charset.StandardCharsets;
import java.security.cert.Certificate;
import java.security.cert.CertificateFactory;
import java.security.cert.X509Certificate;
import java.util.Optional;
import java.util.concurrent.atomic.AtomicReference;
import javax.naming.InvalidNameException;
import javax.net.ssl.SSLPeerUnverifiedException;
import javax.net.ssl.SSLSession;
import org.junit.jupiter.api.Test;

/**
 * Unit tests for {@link AgentIdentityInterceptor}, the only place that decides who an agent is.
 *
 * <p>No Spring and no database: a {@link AgentIdentityLookup} backed by a map stands in for
 * {@code control.agent}, and a fake {@link ServerCall} carries the TLS session attribute. The
 * certificate is a fixed self-signed fixture whose CN contains an escaped comma - the case the
 * design calls out as easy to get wrong.
 */
class AgentIdentityTest {

    /**
     * Self-signed fixture certificate, subject {@code O=probe,CN=agent\,one}. Only the certificate
     * is embedded: the private key is not needed to exercise the server-side interceptor.
     */
    private static final String CERTIFICATE_PEM = """
            -----BEGIN CERTIFICATE-----
            MIIDKTCCAhGgAwIBAgIUYZOe0NYgNlSOleFF9etzZimiQrwwDQYJKoZIhvcNAQEL
            BQAwJDESMBAGA1UEAwwJYWdlbnQsb25lMQ4wDAYDVQQKDAVwcm9iZTAeFw0yNjEx
            MTcyMDMyMTdaFw0zNjExMTQwMjMyMTdaMCQxEjAQBgNVBAMMCWFnZW50LG9uZTEO
            MAwGA1UECgwFcHJvYmUwggEiMA0GCSqGSIb3DQEBAQUAA4IBDwAwggEKAoIBAQDQ
            pP5e/qaTut75ss7/0BYBUkSYEDmFXY2+nnQzWvskHMRape81dxvddK1+il2rDeo6
            kKbfhPB4sUxmanLqxRRHiC4fIudkPym/X+t/p5jtsd6oFNfxQe7Xo0f4A4AXqs8k
            bblavRvAGcs+T3zS+IRoMRRyn7F87SK7KZKvRDHEX3DbNf7hm+/q8sTv2Kgyk4wp
            vESaAnf9b2aZDf8PCuSG8AcDETFYxtFE3sFhYtD0lk/nvnPl/YMmRkWKXBQUgCPr
            ZBV9D37VIzU4bGVYxc5yXEUFVchVu4GQsxDhfXTiOWJk4FS4T2e46hMVlTLNYHoK
            UO2IFGCyfJsGcaciPiHxAgMBAAGjUzBRMB0GA1UdDgQWBBT8ZHeTpAi05LamXM3e
            cCh+VDgB0jAfBgNVHSMEGDAWgBT8ZHeTpAi05LamXM3ecCh+VDgB0jAPBgNVHRMB
            Af8EBTADAQH/MA0GCSqGSIb3DQEBCwUAA4IBAQCo8+gNpUClphfrP2FvscHKyHRe
            S4SsnC/t/4PyPhSmjibqK5PnVZ36XXSHJ1WiGA79WBYtKs4xmtuZ18zFT/MmtHgB
            mGGjWj7gLQ8c8dOdpQwJMpqyqQ7Mk96ad8gBArOrf1EV857Cy1cjp2Csh8cLNAJY
            fiu5wk/F7vuX5fcFv5GIHX93KEwEwBi5fMGDfsw8aB0Le8yqrxbSPSbERu4sbKht
            gdCyMyZIsZntBYznliaFmmXT5uxF3hdOavdgEPRX0DGNKJcFHhzFpGLMa++C98xG
            JyRZjCai7W9IJQ4eyb4gyn0ZWRMb4hB8SCZnMI7q3UqIUIoUmg7U63nK266J
            -----END CERTIFICATE-----
            """;

    private static final X509Certificate PEER_CERTIFICATE = loadCertificate();
    private static final String PEER_CN = "agent,one";
    private static final String PEER_FINGERPRINT =
            AgentIdentityInterceptor.sha256Hex(encoded(PEER_CERTIFICATE));

    @Test
    void missingSslSessionIsUnauthenticated() {
        FakeCall call = new FakeCall(Attributes.EMPTY);
        CapturingHandler handler = new CapturingHandler();

        new AgentIdentityInterceptor(AgentIdentityLookup.none())
                .interceptCall(call, new Metadata(), handler);

        assertThat(handler.invoked).isFalse();
        assertThat(call.closedStatus).isNotNull();
        assertThat(call.closedStatus.getCode()).isEqualTo(Status.Code.UNAUTHENTICATED);
        assertThat(call.closedTrailers.get(RequestIdInterceptor.ERROR_CODE_KEY))
                .isEqualTo(AgentIdentityInterceptor.ERROR_UNAUTHENTICATED);
    }

    @Test
    void validCertificatePublishesPrincipalAndDetachesContext() {
        FakeCall call = new FakeCall(tlsAttributes(PEER_CERTIFICATE));
        CapturingHandler handler = new CapturingHandler();
        AgentIdentityLookup lookup = id -> Optional.of(principal(id, PEER_FINGERPRINT, "active"));

        new AgentIdentityInterceptor(lookup).interceptCall(call, new Metadata(), handler);

        assertThat(call.closedStatus).isNull();
        assertThat(handler.invoked).isTrue();
        assertThat(handler.principal).isNotNull();
        assertThat(handler.principal.agentId()).isEqualTo(PEER_CN);
        assertThat(handler.principal.fingerprint()).isEqualTo(PEER_FINGERPRINT);
        assertThat(handler.principal.status()).isEqualTo(AgentPrincipal.STATUS_ACTIVE);
        // The context is attached only for the duration of startCall.
        assertThat(AgentPrincipal.CONTEXT_KEY.get()).isNull();
    }

    @Test
    void escapedCommaInCommonNameIsUnescaped() {
        FakeCall call = new FakeCall(tlsAttributes(PEER_CERTIFICATE));
        CapturingHandler handler = new CapturingHandler();
        AtomicReference<String> lookedUp = new AtomicReference<>();
        AgentIdentityLookup lookup = id -> {
            lookedUp.set(id);
            return Optional.of(principal(id, PEER_FINGERPRINT, "active"));
        };

        new AgentIdentityInterceptor(lookup).interceptCall(call, new Metadata(), handler);

        assertThat(lookedUp).hasValue(PEER_CN);
        assertThat(handler.principal.agentId()).isEqualTo(PEER_CN);
    }

    @Test
    void commonNameAttributeTypeIsCaseInsensitive() throws InvalidNameException {
        assertThat(AgentIdentityInterceptor.firstCommonName("cn=agent-m1,o=probe"))
                .isEqualTo("agent-m1");
        assertThat(AgentIdentityInterceptor.firstCommonName("O=probe,CN=agent-m1"))
                .isEqualTo("agent-m1");
        assertThat(AgentIdentityInterceptor.firstCommonName("O=probe")).isNull();
    }

    @Test
    void uppercaseStoredFingerprintIsAccepted() {
        FakeCall call = new FakeCall(tlsAttributes(PEER_CERTIFICATE));
        CapturingHandler handler = new CapturingHandler();
        String storedUpperCase = PEER_FINGERPRINT.toUpperCase();

        new AgentIdentityInterceptor(id -> Optional.of(principal(id, storedUpperCase, "active")))
                .interceptCall(call, new Metadata(), handler);

        assertThat(handler.invoked).isTrue();
        assertThat(handler.principal.fingerprint()).isEqualTo(PEER_FINGERPRINT);
    }

    @Test
    void unknownAgentAndWrongFingerprintShareTheSameCode() {
        FakeCall unknownCall = new FakeCall(tlsAttributes(PEER_CERTIFICATE));
        new AgentIdentityInterceptor(AgentIdentityLookup.none())
                .interceptCall(unknownCall, new Metadata(), new CapturingHandler());

        FakeCall mismatchCall = new FakeCall(tlsAttributes(PEER_CERTIFICATE));
        new AgentIdentityInterceptor(id -> Optional.of(principal(id, "0".repeat(64), "active")))
                .interceptCall(mismatchCall, new Metadata(), new CapturingHandler());

        assertThat(unknownCall.closedStatus.getCode()).isEqualTo(Status.Code.PERMISSION_DENIED);
        assertThat(mismatchCall.closedStatus.getCode()).isEqualTo(Status.Code.PERMISSION_DENIED);
        assertThat(unknownCall.closedTrailers.get(RequestIdInterceptor.ERROR_CODE_KEY))
                .isEqualTo(AgentIdentityInterceptor.ERROR_IDENTITY_MISMATCH);
        assertThat(mismatchCall.closedTrailers.get(RequestIdInterceptor.ERROR_CODE_KEY))
                .isEqualTo(AgentIdentityInterceptor.ERROR_IDENTITY_MISMATCH);
    }

    @Test
    void revokedAgentIsFailedPrecondition() {
        FakeCall call = new FakeCall(tlsAttributes(PEER_CERTIFICATE));
        CapturingHandler handler = new CapturingHandler();

        new AgentIdentityInterceptor(
                        id -> Optional.of(principal(id, PEER_FINGERPRINT, AgentPrincipal.STATUS_REVOKED)))
                .interceptCall(call, new Metadata(), handler);

        assertThat(handler.invoked).isFalse();
        assertThat(call.closedStatus.getCode()).isEqualTo(Status.Code.FAILED_PRECONDITION);
        assertThat(call.closedTrailers.get(RequestIdInterceptor.ERROR_CODE_KEY))
                .isEqualTo(AgentIdentityInterceptor.ERROR_REVOKED);
    }

    private static AgentPrincipal principal(String agentId, String fingerprint, String status) {
        return new AgentPrincipal(agentId, fingerprint, "g1", status);
    }

    private static X509Certificate loadCertificate() {
        try (InputStream in = new ByteArrayInputStream(
                CERTIFICATE_PEM.getBytes(StandardCharsets.US_ASCII))) {
            return (X509Certificate) CertificateFactory.getInstance("X.509")
                    .generateCertificate(in);
        } catch (Exception e) {
            throw new IllegalStateException("cannot load the embedded test certificate", e);
        }
    }

    private static byte[] encoded(X509Certificate certificate) {
        try {
            return certificate.getEncoded();
        } catch (Exception e) {
            throw new IllegalStateException(e);
        }
    }

    private static Attributes tlsAttributes(X509Certificate certificate) {
        SSLSession session = (SSLSession) Proxy.newProxyInstance(
                SSLSession.class.getClassLoader(),
                new Class<?>[] {SSLSession.class},
                (proxy, method, args) -> switch (method.getName()) {
                    case "getPeerCertificates" -> new Certificate[] {certificate};
                    case "getPeerCertificateChain" ->
                            throw new SSLPeerUnverifiedException("only X.509 is supported");
                    case "getPeerHost" -> "localhost";
                    case "getPeerPort" -> 9090;
                    case "getProtocol" -> "TLSv1.3";
                    case "getCipherSuite" -> "TLS_AES_128_GCM_SHA256";
                    case "getId" -> new byte[] {1};
                    case "toString" -> "fake SSLSession";
                    case "hashCode" -> System.identityHashCode(proxy);
                    case "equals" -> proxy == args[0];
                    default -> defaultValue(method.getReturnType());
                });
        return Attributes.newBuilder().set(Grpc.TRANSPORT_ATTR_SSL_SESSION, session).build();
    }

    private static Object defaultValue(Class<?> type) {
        if (!type.isPrimitive()) {
            return null;
        }
        if (type == boolean.class) {
            return false;
        }
        if (type == long.class) {
            return 0L;
        }
        if (type == int.class) {
            return 0;
        }
        if (type == short.class) {
            return (short) 0;
        }
        if (type == byte.class) {
            return (byte) 0;
        }
        if (type == char.class) {
            return (char) 0;
        }
        if (type == float.class) {
            return 0f;
        }
        if (type == double.class) {
            return 0d;
        }
        return null;
    }

    /** Records whether the RPC handler ran and what principal it saw in the gRPC context. */
    private static final class CapturingHandler implements ServerCallHandler<Object, Object> {

        boolean invoked;
        AgentPrincipal principal;

        @Override
        public ServerCall.Listener<Object> startCall(
                ServerCall<Object, Object> call, Metadata headers) {
            invoked = true;
            principal = AgentPrincipal.CONTEXT_KEY.get();
            return new ServerCall.Listener<>() {};
        }
    }

    /** Minimal {@link ServerCall} that only remembers how it was closed. */
    private static final class FakeCall extends ServerCall<Object, Object> {

        private final Attributes attributes;
        Status closedStatus;
        Metadata closedTrailers;

        FakeCall(Attributes attributes) {
            this.attributes = attributes;
        }

        @Override
        public void request(int numMessages) {
            // The interceptor never pulls messages.
        }

        @Override
        public void sendHeaders(Metadata headers) {
            // No response is produced in these tests.
        }

        @Override
        public void sendMessage(Object message) {
            // No response is produced in these tests.
        }

        @Override
        public void close(Status status, Metadata trailers) {
            this.closedStatus = status;
            this.closedTrailers = trailers;
        }

        @Override
        public boolean isCancelled() {
            return false;
        }

        @SuppressWarnings("unchecked")
        @Override
        public MethodDescriptor<Object, Object> getMethodDescriptor() {
            return (MethodDescriptor<Object, Object>)
                    (MethodDescriptor<?, ?>) AgentControlGrpc.getGetCursorMethod();
        }

        @Override
        public Attributes getAttributes() {
            return attributes;
        }

        @Override
        public boolean isReady() {
            return true;
        }

        @Override
        public void setMessageCompression(boolean enabled) {
            // Compression is irrelevant for fail-fast authentication tests.
        }

        @Override
        public void setCompression(String compressor) {
            // Compression is irrelevant for fail-fast authentication tests.
        }

        @Override
        public String getAuthority() {
            return "localhost";
        }
    }
}
