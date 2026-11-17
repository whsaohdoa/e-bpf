package dev.train0.cc.control;

import io.grpc.Metadata;
import io.grpc.Status;
import io.grpc.StatusException;
import io.grpc.StatusRuntimeException;
import org.slf4j.Logger;
import org.slf4j.LoggerFactory;
import org.springframework.grpc.server.exception.GrpcExceptionHandler;

/**
 * Maps control-plane failures to gRPC status plus the {@code x-error-code} trailer (design §4.4).
 *
 * <p>{@link AgentRpcException} is the transport-agnostic failure raised by publishers and RPC
 * bodies; its code and optional backoff hint are copied verbatim so agents can branch on a stable
 * string instead of the numeric status. Anything that already carries a gRPC status is passed
 * through unchanged - notably {@code UNIMPLEMENTED} for {@code Watch} and the
 * {@code StatusRuntimeException} raised by the identity interceptor. Any other throwable becomes
 * {@code INTERNAL}/{@code server.error} and is only written to the log: the wire response must not
 * leak SQL, stack frames or host names.
 */
public final class AgentErrorHandler implements GrpcExceptionHandler {

    /** Error code used for failures the control plane did not classify. */
    public static final String SERVER_ERROR_CODE = "server.error";

    private static final Logger log = LoggerFactory.getLogger(AgentErrorHandler.class);

    @Override
    public StatusException handleException(Throwable exception) {
        if (exception instanceof AgentRpcException rpcFailure) {
            Metadata trailers = new Metadata();
            trailers.put(RequestIdInterceptor.ERROR_CODE_KEY, rpcFailure.errorCode());
            if (rpcFailure.retryAfterMs() != null) {
                trailers.put(RequestIdInterceptor.RETRY_AFTER_MS_KEY,
                        Integer.toString(rpcFailure.retryAfterMs()));
            }
            return Status.fromCodeValue(rpcFailure.code().value())
                    .withDescription(rpcFailure.detail())
                    .asException(trailers);
        }
        if (exception instanceof StatusException statusFailure) {
            return statusFailure;
        }
        if (exception instanceof StatusRuntimeException statusFailure) {
            return statusFailure.getStatus().asException(statusFailure.getTrailers());
        }

        log.error("unhandled exception on the agent gRPC path", exception);
        Metadata trailers = new Metadata();
        trailers.put(RequestIdInterceptor.ERROR_CODE_KEY, SERVER_ERROR_CODE);
        return Status.INTERNAL.withDescription("internal server error").asException(trailers);
    }
}
