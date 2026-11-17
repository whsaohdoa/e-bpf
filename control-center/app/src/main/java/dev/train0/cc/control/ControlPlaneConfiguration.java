package dev.train0.cc.control;

import io.grpc.ServerInterceptor;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.core.annotation.Order;
import org.springframework.grpc.server.GlobalServerInterceptor;
import org.springframework.grpc.server.exception.GrpcExceptionHandler;

/**
 * Wires the agent gRPC control plane. Only the control role serves agents:
 * with {@code cc.role=mgmt} there is no {@code BindableService} bean and the
 * gRPC server auto-configuration backs off, so the management instance never
 * listens on the agent port.
 *
 * <p>The interceptor order is load bearing. Request id runs first so that a
 * rejected handshake (no client certificate) still gets a request id echoed in
 * the trailers; identity runs second because every error handler decision needs
 * the authenticated principal.
 */
@Configuration(proxyBeanMethods = false)
@ConditionalOnProperty(name = "cc.role", havingValue = "control")
class ControlPlaneConfiguration {

    @Bean
    @GlobalServerInterceptor
    @Order(0)
    ServerInterceptor requestIdInterceptor() {
        return new RequestIdInterceptor();
    }

    @Bean
    @GlobalServerInterceptor
    @Order(10)
    ServerInterceptor agentIdentityInterceptor(AgentIdentityLookup agentIdentityLookup) {
        return new AgentIdentityInterceptor(agentIdentityLookup);
    }

    @Bean
    GrpcExceptionHandler agentErrorHandler() {
        return new AgentErrorHandler();
    }
}
