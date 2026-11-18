package dev.train0.cc.control;

import io.grpc.ForwardingServerCall;
import io.grpc.Metadata;
import io.grpc.ServerCall;
import io.grpc.ServerCallHandler;
import io.grpc.ServerInterceptor;
import io.grpc.Status;
import java.util.UUID;
import org.slf4j.MDC;

/**
 * Outermost interceptor ({@code @Order(0)}) that gives every agent RPC a request id.
 *
 * <p>It reads {@code x-request-id} from the request metadata (generating a UUIDv4 when the agent
 * did not send one), exposes it through the MDC key used by the logging pattern, and appends it to
 * the response trailers. The {@code close(status, trailers)} hook is the only place that still runs
 * when authentication or the RPC itself fails, which is why the id lives in the trailers rather than
 * in the response message: a rejected handshake must still be correlatable on the agent side.
 *
 * <p>Trailer names are shared with the other interceptors and with the error handler, so they are
 * declared here once.
 */
public final class RequestIdInterceptor implements ServerInterceptor {

    /** Request/response trailer carrying the per-RPC correlation id. */
    public static final Metadata.Key<String> REQUEST_ID_KEY =
            Metadata.Key.of("x-request-id", Metadata.ASCII_STRING_MARSHALLER);

    /** Trailer naming the stable machine-readable failure code, present only on errors. */
    public static final Metadata.Key<String> ERROR_CODE_KEY =
            Metadata.Key.of("x-error-code", Metadata.ASCII_STRING_MARSHALLER);

    /** Optional backoff hint in milliseconds, present only for retryable failures. */
    public static final Metadata.Key<String> RETRY_AFTER_MS_KEY =
            Metadata.Key.of("x-retry-after-ms", Metadata.ASCII_STRING_MARSHALLER);

    /** MDC key; must match {@code %X{request_id:-}} in the logging pattern. */
    public static final String MDC_KEY = "request_id";

    @Override
    public <ReqT, RespT> ServerCall.Listener<ReqT> interceptCall(
            ServerCall<ReqT, RespT> call, Metadata headers, ServerCallHandler<ReqT, RespT> next) {
        String requestId = headers.get(REQUEST_ID_KEY);
        if (requestId == null || requestId.isBlank()) {
            requestId = UUID.randomUUID().toString();
        }
        MDC.put(MDC_KEY, requestId);
        try {
            return next.startCall(new TrailerEchoCall<>(call, requestId), headers);
        } finally {
            MDC.remove(MDC_KEY);
        }
    }

    /**
     * Adds the correlation id (and, when present, the error code) to the trailers of the close that
     * ends the call. Both values are copied into the caller's metadata object before delegating.
     */
    private static final class TrailerEchoCall<ReqT, RespT>
            extends ForwardingServerCall.SimpleForwardingServerCall<ReqT, RespT> {

        private final String requestId;

        TrailerEchoCall(ServerCall<ReqT, RespT> delegate, String requestId) {
            super(delegate);
            this.requestId = requestId;
        }

        @Override
        public void close(Status status, Metadata trailers) {
            Metadata out = trailers == null ? new Metadata() : trailers;
            out.put(REQUEST_ID_KEY, requestId);
            // x-error-code is authored by whoever produced the failure (identity interceptor or
            // exception handler); this layer only guarantees it survives to the wire.
            super.close(status, out);
        }
    }
}
