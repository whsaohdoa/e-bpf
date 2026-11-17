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

-- ---------------------------------------------------------------------------------------------
-- Train 0 control-plane schema (design §5). Appended only: a changeset that already ran must
-- never be edited, otherwise every existing deployment fails its checksum check.
-- ---------------------------------------------------------------------------------------------

--changeset train0:control-006-agent-enrollment
-- Enrollment facts live on the control side: group_id is written when the certificate is issued
-- (design §12-D2) because the control plane must never read the management database. machine_id
-- becomes nullable so the first Register can bind it; a NOT NULL column with no seed row would
-- make enrollment impossible. The applied_* / config_state columns are the denormalised "latest
-- known state" that Heartbeat reads; the per-version history lives in config_ack (010).
ALTER TABLE control.agent ALTER COLUMN machine_id DROP NOT NULL;
ALTER TABLE control.agent
    ADD COLUMN group_id        text,
    ADD COLUMN agent_version   text,
    ADD COLUMN boot_id         text,
    ADD COLUMN config_state    text,
    ADD COLUMN capabilities    jsonb,
    ADD COLUMN applied_version integer,
    ADD COLUMN applied_at      timestamptz,
    ADD COLUMN registered_at   timestamptz;
--rollback ALTER TABLE control.agent DROP COLUMN group_id;
--rollback ALTER TABLE control.agent DROP COLUMN agent_version;
--rollback ALTER TABLE control.agent DROP COLUMN boot_id;
--rollback ALTER TABLE control.agent DROP COLUMN config_state;
--rollback ALTER TABLE control.agent DROP COLUMN capabilities;
--rollback ALTER TABLE control.agent DROP COLUMN applied_version;
--rollback ALTER TABLE control.agent DROP COLUMN applied_at;
--rollback ALTER TABLE control.agent DROP COLUMN registered_at;
--rollback ALTER TABLE control.agent ALTER COLUMN machine_id SET NOT NULL;

--changeset train0:control-007-config-document-bytes
-- Contract §6.4: the control plane encodes once, stores the bytes and hashes those bytes; reads
-- never re-derive them from a jsonb document (jsonb normalises and reorders, so the hash an
-- agent compares would not be reproducible). Train 0 is the only window where the old column can
-- be dropped directly: the table is empty (design §5.2).
ALTER TABLE control.config_version DROP COLUMN document;
ALTER TABLE control.config_version ADD COLUMN document bytea NOT NULL;
-- Versions come from a sequence, not max+1: publishing is concurrent and the contract explicitly
-- accepts gaps between groups (e.g. 7 -> 12).
CREATE SEQUENCE control.config_version_seq START WITH 1;
--rollback DROP SEQUENCE control.config_version_seq;
--rollback ALTER TABLE control.config_version DROP COLUMN document;
--rollback ALTER TABLE control.config_version ADD COLUMN document jsonb NOT NULL;

--changeset train0:control-008-report-window
-- Window-level rows exist even for a window with zero aggregates. GetCursor is defined as "the
-- highest window_id fully received"; max(report.window_id) would never advance on an empty
-- window, and complete/truncated/window_ms are window facts that have nowhere else to live.
CREATE TABLE control.report_window (
    agent_id     text   NOT NULL REFERENCES control.agent (agent_id),
    window_id    bigint NOT NULL,
    window_ms    integer NOT NULL,
    complete     boolean NOT NULL,
    window_start timestamptz,
    truncated    boolean NOT NULL DEFAULT false,
    row_count    integer NOT NULL DEFAULT 0,
    received_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, window_id)
);
--rollback DROP TABLE control.report_window;

--changeset train0:control-009-report-rows
-- The jsonb payload cannot hold a 32-byte key_hash as the contract defines it and cannot dedupe
-- or aggregate by key in SQL (S1-3 builds a panel on these columns); it also cannot represent a
-- window that had zero aggregates. Train 0 has no rows, so the table is rebuilt, not migrated.
DROP TABLE control.report;
CREATE TABLE control.report (
    agent_id             text   NOT NULL,
    window_id            bigint NOT NULL,
    key_hash             bytea  NOT NULL CHECK (octet_length(key_hash) = 32),
    packets              bigint NOT NULL,
    first_seen_offset_ns bigint NOT NULL,
    last_seen_offset_ns  bigint NOT NULL,
    sample_dropped       boolean NOT NULL DEFAULT false,
    degraded             boolean NOT NULL DEFAULT false,
    received_at          timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, window_id, key_hash),
    -- The window row must exist before its aggregates: the composite FK makes the intake order
    -- explicit instead of relying on the service to remember it.
    FOREIGN KEY (agent_id, window_id) REFERENCES control.report_window (agent_id, window_id)
);
CREATE TABLE control.report_sample (
    agent_id     text   NOT NULL,
    window_id    bigint NOT NULL,
    key_hash     bytea  NOT NULL,
    five_tuple   text   NOT NULL,
    ts_offset_ns bigint NOT NULL,
    PRIMARY KEY (agent_id, window_id, key_hash, ts_offset_ns)
);
--rollback DROP TABLE control.report_sample;
--rollback DROP TABLE control.report;
--rollback CREATE TABLE control.report (agent_id text NOT NULL REFERENCES control.agent (agent_id), window_id bigint NOT NULL, key_hash text NOT NULL, payload jsonb NOT NULL, received_at timestamptz NOT NULL DEFAULT now(), PRIMARY KEY (agent_id, window_id, key_hash));

--changeset train0:control-010-config-ack
-- One row per (agent, version): the contract only asks for the latest applied_version on
-- control.agent, but G1 and "who rejected which version and why" need history. received_at is
-- the control-plane clock and is written once (COALESCE on retries), otherwise a retried ack
-- would move the G1 end marker forward.
CREATE TABLE control.config_ack (
    agent_id          text   NOT NULL REFERENCES control.agent (agent_id),
    version           integer NOT NULL,
    content_sha256    text   NOT NULL CHECK (content_sha256 ~ '^[0-9a-f]{64}$'),
    result            text   NOT NULL CHECK (result IN ('applied', 'rejected', 'failed')),
    applied_at        timestamptz,
    apply_ms          integer,
    degraded_features text[] NOT NULL DEFAULT '{}',
    error_code        text,
    error_detail      text,
    received_at       timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (agent_id, version)
);
--rollback DROP TABLE control.config_ack;
