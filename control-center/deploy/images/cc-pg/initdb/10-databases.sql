-- Two databases, two planes. The management plane never opens control, and the control
-- plane never opens mgmt: that is what makes the resource-level authorization boundary in
-- Train 0 structural rather than a code convention.
CREATE DATABASE mgmt    OWNER cc_migrator;
CREATE DATABASE control OWNER cc_migrator;
