package dev.train0.cc.control;

import io.grpc.Status;

/**
 * Failure raised by control-plane logic, independent of the transport that reported it. The
 * gRPC layer maps it to {@code StatusException} plus the {@code x-error-code} trailer (design
 * §4.4); the unauthenticated HTTP publish endpoint maps the same instance to a 4xx body. Keeping
 * the class transport-agnostic is what lets {@code ConfigPublisher} stay testable without a
 * server.
 */
final class AgentRpcException extends RuntimeException {

    private final Status.Code code;
    private final String errorCode;
    private final String detail;
    private final Integer retryAfterMs;

    AgentRpcException(Status.Code code, String errorCode, String detail) {
        this(code, errorCode, detail, null);
    }

    AgentRpcException(Status.Code code, String errorCode, String detail, Integer retryAfterMs) {
        super(errorCode + ": " + detail);
        this.code = code;
        this.errorCode = errorCode;
        this.detail = detail;
        this.retryAfterMs = retryAfterMs;
    }

    Status.Code code() {
        return code;
    }

    String errorCode() {
        return errorCode;
    }

    String detail() {
        return detail;
    }

    Integer retryAfterMs() {
        return retryAfterMs;
    }
}
