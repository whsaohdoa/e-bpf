--liquibase formatted SQL logicalFilePath:db/mgmt/changelog.sql
-- The logical path keeps the DB record identical whether Liquibase is started by the
-- service (classpath:db/mgmt/changelog.sql) or by scripts/migrate.sh (a filesystem
-- path). Without it the two paths count as different changelogs and rollback finds
-- nothing to undo.

--changeset train0:mgmt-001-users-and-groups
CREATE TABLE mgmt.app_user (
    id                text PRIMARY KEY,
    display_name      text NOT NULL,
    password_hash     text,
    is_auditor        boolean NOT NULL DEFAULT false,
    is_platform_admin boolean NOT NULL DEFAULT false,
    created_at        timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE mgmt.user_group (
    id         text PRIMARY KEY,
    name       text NOT NULL UNIQUE,
    created_at timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE mgmt.user_group_member (
    group_id text NOT NULL REFERENCES mgmt.user_group (id) ON DELETE CASCADE,
    user_id  text NOT NULL REFERENCES mgmt.app_user (id) ON DELETE CASCADE,
    role     text NOT NULL CHECK (role IN ('group_admin', 'member')),
    PRIMARY KEY (group_id, user_id)
);
--rollback DROP TABLE mgmt.user_group_member;
--rollback DROP TABLE mgmt.user_group;
--rollback DROP TABLE mgmt.app_user;

--changeset train0:mgmt-002-machines
-- C1: a machine belongs to exactly one group, so group_id is NOT NULL and this FK is the
-- only place membership is expressed (later, no group_id is trusted from a request body).
CREATE TABLE mgmt.machine (
    id               text PRIMARY KEY,
    name             text NOT NULL UNIQUE,
    group_id         text NOT NULL REFERENCES mgmt.user_group (id),
    status           text NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'active', 'revoked')),
    cert_fingerprint text,
    last_seen        timestamptz,
    created_at       timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX machine_group_idx ON mgmt.machine (group_id);
--rollback DROP TABLE mgmt.machine;

--changeset train0:mgmt-003-audit-append-only
CREATE TABLE mgmt.audit (
    id            bigserial PRIMARY KEY,
    ts            timestamptz NOT NULL DEFAULT now(),
    actor_id      text,
    actor_type    text NOT NULL CHECK (actor_type IN ('user', 'agent', 'system')),
    act_as        text,
    group_id      text,
    action        text NOT NULL,
    resource_type text NOT NULL,
    resource_id   text,
    result        text NOT NULL CHECK (result IN ('allow', 'deny', 'error')),
    reason        text,
    request_id    text NOT NULL,
    ip            inet,
    user_agent    text
);
CREATE INDEX audit_ts_idx ON mgmt.audit (ts DESC);
CREATE INDEX audit_request_idx ON mgmt.audit (request_id);

-- Append-only is enforced twice: a trigger (survives grant mistakes) and revoked grants
-- (so cc_app cannot even attempt it). Both are part of the C5 acceptance evidence.
-- The plpgsql body lives in deploy/images/cc-pg/initdb/20-schemas.sql: Liquibase's
-- formatted-SQL splitter cannot carry a dollar-quoted function body, and splitting it
-- produces "Unterminated dollar quote" from the PostgreSQL driver.
CREATE TRIGGER audit_no_update_delete
    BEFORE UPDATE OR DELETE ON mgmt.audit
    FOR EACH ROW EXECUTE FUNCTION mgmt.audit_forbid_mutation();

REVOKE UPDATE, DELETE, TRUNCATE ON mgmt.audit FROM cc_app;
--rollback DROP TABLE mgmt.audit;

--changeset train0:mgmt-004-seed
-- Password hashes are intentionally unusable placeholders: S1-2 replaces them with
-- Argon2id hashes produced by the registration path, never written by hand.
INSERT INTO mgmt.app_user (id, display_name, password_hash, is_auditor, is_platform_admin) VALUES
    ('admin', 'Platform Admin', '!pending', false, true),
    ('aud',   'Auditor',        '!pending', true,  false),
    ('alice', 'Alice',          '!pending', false, false),
    ('bob',   'Bob',            '!pending', false, false),
    ('carol', 'Carol',          '!pending', false, false);

INSERT INTO mgmt.user_group (id, name) VALUES
    ('g1', 'team-alpha'),
    ('g2', 'team-beta');

INSERT INTO mgmt.user_group_member (group_id, user_id, role) VALUES
    ('g1', 'alice', 'group_admin'),
    ('g1', 'bob',   'member'),
    ('g2', 'carol', 'member');

INSERT INTO mgmt.machine (id, name, group_id, status) VALUES
    ('m1', 'alpha-node-1', 'g1', 'pending'),
    ('m2', 'beta-node-1',  'g2', 'pending');
--rollback DELETE FROM mgmt.machine;
--rollback DELETE FROM mgmt.user_group_member;
--rollback DELETE FROM mgmt.user_group;
--rollback DELETE FROM mgmt.app_user;
