#!/bin/sh
# Sourced by the postgres entrypoint (not executed), so it keeps the entrypoint's shell
# functions and environment. Runs before 10-databases.sql; ordering matters because the
# databases below are owned by cc_migrator.
set -eu

: "${CC_MIGRATOR_PASSWORD:=cc_migrator}"
: "${CC_APP_PASSWORD:=cc_app}"
: "${CC_AUDIT_PASSWORD:=cc_audit}"
: "${CC_RO_PASSWORD:=cc_ro}"

psql -v ON_ERROR_STOP=1 --username "${POSTGRES_USER}" --dbname postgres <<-SQL
	CREATE ROLE cc_migrator LOGIN PASSWORD '${CC_MIGRATOR_PASSWORD}';
	CREATE ROLE cc_app      LOGIN PASSWORD '${CC_APP_PASSWORD}';
	CREATE ROLE cc_audit    LOGIN PASSWORD '${CC_AUDIT_PASSWORD}';
	CREATE ROLE cc_ro       LOGIN PASSWORD '${CC_RO_PASSWORD}';
SQL

echo "cc-pg: created roles cc_migrator, cc_app, cc_audit, cc_ro"
