package dev.train0.cc.api;

import java.time.Instant;
import java.util.LinkedHashMap;
import java.util.Map;

import org.springframework.beans.factory.annotation.Value;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RestController;

/** Unauthenticated liveness/identity probe used by smoke tests and by the G1 latency harness. */
@RestController
public class HealthController {

    private final String role;
    private final String version;

    public HealthController(@Value("${cc.role}") String role, @Value("${cc.version:dev}") String version) {
        this.role = role;
        this.version = version;
    }

    @GetMapping("/api/health")
    public Map<String, Object> health() {
        Map<String, Object> body = new LinkedHashMap<>();
        body.put("service", "cc-" + role);
        body.put("role", role);
        body.put("version", version);
        body.put("utc", Instant.now().toString());
        return body;
    }
}
