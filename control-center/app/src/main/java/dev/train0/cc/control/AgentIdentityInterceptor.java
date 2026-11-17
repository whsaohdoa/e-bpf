package dev.train0.cc.control;

import io.grpc.Context;
import io.grpc.Contexts;
import io.grpc.Grpc;
import io.grpc.Metadata;
import io.grpc.ServerCall;
import io.grpc.ServerCallHandler;
import io.grpc.ServerInterceptor;
import io.grpc.Status;
import io.grpc.StatusRuntimeException;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.security.cert.Certificate;
import java.security.cert.CertificateEncodingException;
import java.security.cert.X509Certificate;
import java.util.HexFormat;
import java.util.Objects;
import javax.naming.InvalidNameException;
import javax.naming.ldap.LdapName;
import javax.naming.ldap.Rdn;
import javax.net.ssl.SSLPeerUnverifiedException;
import javax.net.ssl.SSLSession;
import javax.security.auth.x500.X500Principal;

/**
 * Authentication interceptor ({@code @Order(10)}): binds the peer certificate to a
 * {@code control.agent} row and publishes an {@link AgentPrincipal} through the gRPC context.
 *
 * <p>The agent id comes exclusively from the certificate CN; nothing in the request payload is
 * trusted for identity (contract §1). The certificate fingerprint is always recomputed from the
 * DER bytes of the peer chain, and the comparison result is deliberately coarse: an unknown CN and
 * a known CN with a wrong fingerprint produce the same {@code agent.identity_mismatch} code so a
 * client cannot probe which agent ids exist (design §4.2).
 *
 * <p>This interceptor authenticates only; authorization decisions live in the RPCs. A revoked
 * agent is refused here so that no RPC - including {@code Register} - can run for it.
 */
public final class AgentIdentityInterceptor implements ServerInterceptor {

    static final String ERROR_UNAUTHENTICATED = "agent.unauthenticated";
    static final String ERROR_IDENTITY_MISMATCH = "agent.identity_mismatch";
    static final String ERROR_REVOKED = "agent.revoked";

    private final AgentIdentityLookup lookup;

    public AgentIdentityInterceptor(AgentIdentityLookup lookup) {
        this.lookup = Objects.requireNonNull(lookup, "lookup");
    }

    @Override
    public <ReqT, RespT> ServerCall.Listener<ReqT> interceptCall(
            ServerCall<ReqT, RespT> call, Metadata headers, ServerCallHandler<ReqT, RespT> next) {
        AgentPrincipal principal;
        try {
            principal = authenticate(call);
        } catch (StatusRuntimeException rejected) {
            Metadata trailers = rejected.getTrailers() == null ? new Metadata() : rejected.getTrailers();
            call.close(rejected.getStatus(), trailers);
            return new ServerCall.Listener<ReqT>() {};
        }

        // The context has to stay attached for the whole call, not just startCall: the service body
        // runs later from onHalfClose and still reads AgentPrincipal.CONTEXT_KEY. Contexts wraps the
        // listener so each callback runs in this context and the previous one is restored afterwards.
        Context context = Context.current().withValue(AgentPrincipal.CONTEXT_KEY, principal);
        return Contexts.interceptCall(context, call, headers, next);
    }

    private AgentPrincipal authenticate(ServerCall<?, ?> call) {
        SSLSession session = call.getAttributes().get(Grpc.TRANSPORT_ATTR_SSL_SESSION);
        if (session == null) {
            // The listener accepted a plaintext connection: client-auth is misconfigured.
            throw reject(Status.UNAUTHENTICATED, ERROR_UNAUTHENTICATED,
                    "client certificate required");
        }

        X509Certificate peer;
        try {
            Certificate[] chain = session.getPeerCertificates();
            if (chain == null || chain.length == 0 || !(chain[0] instanceof X509Certificate x509)) {
                throw reject(Status.UNAUTHENTICATED, ERROR_UNAUTHENTICATED,
                        "no X.509 client certificate");
            }
            peer = x509;
        } catch (SSLPeerUnverifiedException e) {
            throw reject(Status.UNAUTHENTICATED, ERROR_UNAUTHENTICATED,
                    "no X.509 client certificate");
        }

        String cn;
        try {
            String distinguishedName =
                    peer.getSubjectX500Principal().getName(X500Principal.RFC2253);
            cn = firstCommonName(distinguishedName);
        } catch (InvalidNameException e) {
            throw reject(Status.UNAUTHENTICATED, ERROR_UNAUTHENTICATED,
                    "unparseable client certificate subject");
        }
        if (cn == null || cn.isBlank()) {
            throw reject(Status.UNAUTHENTICATED, ERROR_UNAUTHENTICATED,
                    "client certificate has no CN");
        }

        String fingerprint;
        try {
            fingerprint = sha256Hex(peer.getEncoded());
        } catch (CertificateEncodingException e) {
            throw reject(Status.UNAUTHENTICATED, ERROR_UNAUTHENTICATED,
                    "unencodable client certificate");
        }

        AgentPrincipal stored = lookup.find(cn).orElse(null);
        if (stored == null || stored.fingerprint() == null
                || !fingerprint.equalsIgnoreCase(stored.fingerprint())) {
            throw reject(Status.PERMISSION_DENIED, ERROR_IDENTITY_MISMATCH,
                    "client certificate does not match a registered agent");
        }
        if (stored.revoked()) {
            throw reject(Status.FAILED_PRECONDITION, ERROR_REVOKED, "agent is revoked");
        }
        // Publish the fingerprint that was actually presented, normalised to lowercase hex, so
        // downstream code never has to wonder which casing the database row happened to use.
        return new AgentPrincipal(stored.agentId(), fingerprint, stored.groupId(), stored.status());
    }

    /**
     * Returns the value of the first {@code CN} RDN of an RFC 2253 distinguished name, or
     * {@code null} when the name has none. Attribute types are matched case-insensitively because
     * the string form is not guaranteed to be canonical; escaped separators inside the value are
     * already unescaped by the parser.
     */
    static String firstCommonName(String distinguishedName) throws InvalidNameException {
        LdapName name = new LdapName(distinguishedName);
        for (Rdn rdn : name.getRdns()) {
            if ("CN".equalsIgnoreCase(rdn.getType())) {
                return String.valueOf(rdn.getValue());
            }
        }
        return null;
    }

    /** Lowercase SHA-256 hex of the certificate DER encoding. */
    static String sha256Hex(byte[] der) {
        try {
            return HexFormat.of().formatHex(MessageDigest.getInstance("SHA-256").digest(der));
        } catch (NoSuchAlgorithmException e) {
            throw new IllegalStateException("SHA-256 is mandatory in every JRE", e);
        }
    }

    private static StatusRuntimeException reject(Status status, String errorCode, String detail) {
        Metadata trailers = new Metadata();
        trailers.put(RequestIdInterceptor.ERROR_CODE_KEY, errorCode);
        return status.withDescription(detail).asRuntimeException(trailers);
    }
}
