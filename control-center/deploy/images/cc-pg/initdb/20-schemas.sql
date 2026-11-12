-- Schemas, grants and extensions. Kept out of Liquibase on purpose: migrations must not
-- require superuser or CREATEROLE, so everything privilege-shaped is provisioned here,
-- once, by the initdb superuser.

\connect mgmt
CREATE SCHEMA mgmt AUTHORIZATION cc_migrator;
GRANT USAGE ON SCHEMA mgmt TO cc_app, cc_audit, cc_ro;
ALTER DEFAULT PRIVILEGES FOR ROLE cc_migrator IN SCHEMA mgmt
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO cc_app;
ALTER DEFAULT PRIVILEGES FOR ROLE cc_migrator IN SCHEMA mgmt
    GRANT SELECT ON TABLES TO cc_audit, cc_ro;
ALTER DEFAULT PRIVILEGES FOR ROLE cc_migrator IN SCHEMA mgmt
    GRANT USAGE, SELECT ON SEQUENCES TO cc_app;
CREATE EXTENSION IF NOT EXISTS pgcrypto;

-- Append-only guard for mgmt.audit. The function is created here, next to the roles and
-- extensions, because Liquibase's formatted-SQL splitter cannot carry a dollar-quoted
-- plpgsql body (it truncates it and the driver reports "Unterminated dollar quote").
-- The CREATE TRIGGER that uses it is a plain single statement and stays in the changelog.
CREATE OR REPLACE FUNCTION mgmt.audit_forbid_mutation() RETURNS trigger
LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'audit is append-only: % is not permitted', TG_OP;
END;
$$;
GRANT EXECUTE ON FUNCTION mgmt.audit_forbid_mutation() TO cc_app;

\connect control
CREATE SCHEMA control AUTHORIZATION cc_migrator;
GRANT USAGE ON SCHEMA control TO cc_app, cc_audit, cc_ro;
ALTER DEFAULT PRIVILEGES FOR ROLE cc_migrator IN SCHEMA control
    GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO cc_app;
ALTER DEFAULT PRIVILEGES FOR ROLE cc_migrator IN SCHEMA control
    GRANT SELECT ON TABLES TO cc_audit, cc_ro;
ALTER DEFAULT PRIVILEGES FOR ROLE cc_migrator IN SCHEMA control
    GRANT USAGE, SELECT ON SEQUENCES TO cc_app;
CREATE EXTENSION IF NOT EXISTS pgcrypto;
