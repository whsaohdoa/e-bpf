package dev.train0.cc.api;

import java.util.List;
import java.util.Map;

import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.jdbc.core.JdbcTemplate;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.RestController;

/**
 * Management-plane read model behind the SPA. Deliberately read-only in the skeleton: the
 * authorization rules (group scope, platform_admin override) land with the BFF session work.
 */
@RestController
@ConditionalOnProperty(name = "cc.role", havingValue = "mgmt")
public class MachineController {

    private final JdbcTemplate jdbc;

    public MachineController(JdbcTemplate jdbc) {
        this.jdbc = jdbc;
    }

    @GetMapping("/api/machines")
    public List<Map<String, Object>> machines() {
        return jdbc.queryForList("""
                SELECT m.id,
                       m.name,
                       m.group_id,
                       g.name AS group_name,
                       m.status,
                       m.last_seen
                  FROM machine m
                  JOIN user_group g ON g.id = m.group_id
                 ORDER BY m.id
                """);
    }
}
