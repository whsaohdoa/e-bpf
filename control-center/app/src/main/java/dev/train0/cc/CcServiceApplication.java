package dev.train0.cc;

import org.springframework.boot.SpringApplication;
import org.springframework.boot.autoconfigure.SpringBootApplication;

/**
 * One jar, two roles: {@code CC_ROLE=mgmt} serves the management/BFF API,
 * {@code CC_ROLE=control} serves the control plane (PEP, versions, agent channel).
 *
 * <p>They are separate deployments with separate databases so that the resource-level
 * authorization boundary in Train 0 stays real: the control plane never reads the
 * management database and never trusts a {@code group_id} from a request body.
 */
@SpringBootApplication
public class CcServiceApplication {

    public static void main(String[] args) {
        SpringApplication.run(CcServiceApplication.class, args);
    }
}
