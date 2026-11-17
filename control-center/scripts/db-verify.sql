-- Structural gate for the control schema. Run it after `migctl` on a freshly migrated database:
--
--   psql -h cc-pg -U cc_app -d control -v ON_ERROR_STOP=1 -f -
--
-- The 7 checks below are the ones a half-applied changelog silently gets wrong: Liquibase only
-- reports what it ran, not whether the resulting shape still matches the design. Any mismatch
-- raises an exception naming the column, so the gate fails instead of printing a green NOTICE.
DO $$
DECLARE
    actual_type     text;
    actual_pk       text;
    actual_nullable text;
    column_count    integer;
BEGIN
    -- 1. control.report.key_hash is bytea (32 raw bytes per contract §7). A text column would
    --    accept hex of any length and the dedupe key would stop matching the agent's bytes.
    IF to_regclass('control.report') IS NULL THEN
        RAISE EXCEPTION 'db-verify: control.report is missing';
    END IF;
    SELECT format_type(a.atttypid, a.atttypmod)
      INTO actual_type
      FROM pg_attribute a
     WHERE a.attrelid = 'control.report'::regclass
       AND a.attname = 'key_hash'
       AND NOT a.attisdropped;
    IF actual_type IS DISTINCT FROM 'bytea' THEN
        RAISE EXCEPTION 'db-verify: control.report.key_hash is %, expected bytea',
            coalesce(actual_type, '<missing>');
    END IF;

    -- 2. control.report_sample exists: sampled packets are their own table so aggregates stay
    --    one row per key and samples cannot inflate the row count.
    IF to_regclass('control.report_sample') IS NULL THEN
        RAISE EXCEPTION 'db-verify: control.report_sample is missing';
    END IF;

    -- 3. control.config_ack exists and is keyed by (agent_id, version): acks are idempotent per
    --    agent and version, and a second version must not overwrite the first.
    IF to_regclass('control.config_ack') IS NULL THEN
        RAISE EXCEPTION 'db-verify: control.config_ack is missing';
    END IF;
    SELECT string_agg(a.attname, ',' ORDER BY k.ord)
      INTO actual_pk
      FROM pg_constraint c
      JOIN pg_class t ON t.oid = c.conrelid
      CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord)
      JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum
     WHERE c.contype = 'p'
       AND t.oid = 'control.config_ack'::regclass;
    IF actual_pk IS DISTINCT FROM 'agent_id,version' THEN
        RAISE EXCEPTION 'db-verify: control.config_ack primary key is %, expected (agent_id,version)',
            coalesce(actual_pk, '<none>');
    END IF;

    -- 4. control.report_window PK is composite (agent_id, window_id): the cursor is per agent,
    --    and the report rows reference this exact pair.
    IF to_regclass('control.report_window') IS NULL THEN
        RAISE EXCEPTION 'db-verify: control.report_window is missing';
    END IF;
    SELECT string_agg(a.attname, ',' ORDER BY k.ord)
      INTO actual_pk
      FROM pg_constraint c
      JOIN pg_class t ON t.oid = c.conrelid
      CROSS JOIN LATERAL unnest(c.conkey) WITH ORDINALITY AS k(attnum, ord)
      JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum
     WHERE c.contype = 'p'
       AND t.oid = 'control.report_window'::regclass;
    IF actual_pk IS DISTINCT FROM 'agent_id,window_id' THEN
        RAISE EXCEPTION 'db-verify: control.report_window primary key is %, expected (agent_id,window_id)',
            coalesce(actual_pk, '<none>');
    END IF;

    -- 5. control.config_version.document is bytea: it stores the exact bytes whose SHA-256 the
    --    agent compares, so it must not be jsonb (which normalises and reorders).
    SELECT format_type(a.atttypid, a.atttypmod)
      INTO actual_type
      FROM pg_attribute a
     WHERE a.attrelid = 'control.config_version'::regclass
       AND a.attname = 'document'
       AND NOT a.attisdropped;
    IF actual_type IS DISTINCT FROM 'bytea' THEN
        RAISE EXCEPTION 'db-verify: control.config_version.document is %, expected bytea',
            coalesce(actual_type, '<missing>');
    END IF;

    -- 6. control.agent gained group_id and machine_id became nullable (bound on first Register).
    SELECT count(*)
      INTO column_count
      FROM information_schema.columns
     WHERE table_schema = 'control'
       AND table_name = 'agent'
       AND column_name = 'group_id';
    IF column_count <> 1 THEN
        RAISE EXCEPTION 'db-verify: control.agent.group_id is missing';
    END IF;
    SELECT is_nullable
      INTO actual_nullable
      FROM information_schema.columns
     WHERE table_schema = 'control'
       AND table_name = 'agent'
       AND column_name = 'machine_id';
    IF actual_nullable IS DISTINCT FROM 'YES' THEN
        RAISE EXCEPTION 'db-verify: control.agent.machine_id is not nullable (is_nullable=%)',
            coalesce(actual_nullable, '<missing>');
    END IF;

    -- 7. control.config_version_seq exists: versions come from the sequence, never from max+1,
    --    so a renumbered or missing sequence would hand out duplicate versions under concurrency.
    IF to_regclass('control.config_version_seq') IS NULL THEN
        RAISE EXCEPTION 'db-verify: control.config_version_seq is missing';
    END IF;

    RAISE NOTICE 'db-verify: OK (7 checks)';
END
$$;
