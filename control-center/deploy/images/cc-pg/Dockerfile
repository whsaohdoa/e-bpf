# postgres:17-bookworm already ships psql, pg_dump/pg_restore, pg_isready, pgbench and the
# contrib extensions we need (pgcrypto, pg_stat_statements, pg_trgm, uuid-ossp).
# It does NOT ship curl, openssl or jq -- that is why cc-toolbox exists instead of
# stuffing client tooling into the database image.
ARG PG_IMAGE=postgres:17-bookworm
FROM ${PG_IMAGE}

COPY deploy/images/cc-pg/initdb/ /docker-entrypoint-initdb.d/
# Executable .sh files are run as a subprocess; non-executable ones are *sourced*, which
# would leak `set -eu` into the entrypoint. Keep them executable.
RUN chmod +x /docker-entrypoint-initdb.d/*.sh
