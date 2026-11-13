--liquibase formatted SQL logicalFilePath:db/control/changelog.sql

--changeset train0:control-001-agents
CREATE TABLE control.agent (
    agent_id         text PRIMARY KEY,
    machine_id       text NOT NULL,
    cert_fingerprint text NOT NULL UNIQUE,
    status           text NOT NULL DEFAULT 'pending'
                     CHECK (status IN ('pending', 'active', 'revoked')),
    last_heartbeat   timestamptz,
    reported_cursor  bigint NOT NULL DEFAULT 0,
    created_at       timestamptz NOT NULL DEFAULT now()
);
--rollback DROP TABLE control.agent;

--changeset train0:control-002-config-versions
-- A release is a number plus a content hash: the agent compares the hash, and the control
-- plane never re-derives it from the document at read time.
CREATE TABLE control.config_version (
    version        integer PRIMARY KEY,
    group_id       text NOT NULL,
    content_sha256 text NOT NULL CHECK (content_sha256 ~ '^[0-9a-f]{64}$'),
    schema_version integer NOT NULL,
    document       jsonb NOT NULL,
    published_by   text NOT NULL,
    published_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX config_version_group_idx ON control.config_version (group_id, version DESC);
--rollback DROP TABLE control.config_version;

--changeset train0:control-003-notify-outbox
-- Transactional outbox: publish writes the version plus one notify row per target agent in
-- a single transaction; the dispatcher uses a 500ms timeout and at most 3 attempts.
CREATE TABLE control.notify_outbox (
    id           bigserial PRIMARY KEY,
    agent_id     text NOT NULL REFERENCES control.agent (agent_id),
    version      integer NOT NULL REFERENCES control.config_version (version),
    state        text NOT NULL DEFAULT 'pending'
                 CHECK (state IN ('pending', 'sent', 'delivered', 'failed')),
    attempts     integer NOT NULL DEFAULT 0,
    next_attempt timestamptz NOT NULL DEFAULT now(),
    last_error   text,
    created_at   timestamptz NOT NULL DEFAULT now(),
    updated_at   timestamptz NOT NULL DEFAULT now(),
    UNIQUE (agent_id, version)
);
CREATE INDEX notify_outbox_due_idx ON control.notify_outbox (state, next_attempt);
--rollback DROP TABLE control.notify_outbox;

--changeset train0:control-004-audit-outbox
-- The control plane appends audit locally, then a forwarder posts batches to the management
-- plane at /internal/v1/audit/events. Local rows double as the retry buffer.
CREATE TABLE control.audit_outbox (
    id           bigserial PRIMARY KEY,
    request_id   text NOT NULL,
    payload      jsonb NOT NULL,
    forwarded_at timestamptz,
    created_at   timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX audit_outbox_pending_idx ON control.audit_outbox (id) WHERE forwarded_at IS NULL;
--rollback DROP TABLE control.audit_outbox;

--changeset train0:control-005-report-cursor
-- Idempotent report intake is keyed by (agent_id, window_id, key_hash); the cursor here is
-- what the agent asks for on reconnect. Both are declared now, populated by S1-3.
CREATE TABLE control.report (
    agent_id  text NOT NULL REFERENCES control.agent (agent_id),
    window_id bigint NOT NULL,
    key_hash  text NOT NULL,
    payload   jsonb NOT NULL,
    received_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, window_id, key_hash)
);
--rollback DROP TABLE control.report;
