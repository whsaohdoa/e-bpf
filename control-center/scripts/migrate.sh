#!/usr/bin/env bash
# Migration demo, driven from a throwaway Maven container on the compose network.
#
#   bash scripts/migrate.sh mgmt 1     # down: roll back the last changeset
#   bash scripts/migrate.sh mgmt up    # up: apply every pending changeset
#   bash scripts/migrate.sh control 2
#
# The Maven image is the one the service image was built with, so the Liquibase version and
# the JDBC driver match runtime exactly.
#
# Two details are easy to get wrong:
#   * Liquibase must be told the schema (defaultSchemaName). Without it the tool looks for
#     DATABASECHANGELOG in "public", sees no applied changesets, rolls back nothing and
#     still exits 0 ("0 changesets rolled back").
#   * changelog.sql declares logicalFilePath, so the service (classpath:...) and this script
#     (filesystem path) record the same changeset identity.
set -euo pipefail
source "$(dirname "$0")/lib.sh"

db="${1:?usage: migrate.sh <mgmt|control> [<count>|up]}"
mode="${2:-1}"
case "$db" in
    mgmt | control) ;;
    *) fail "db must be 'mgmt' or 'control'" ;;
esac

docker network inspect "$COMPOSE_NET" >/dev/null 2>&1 \
    || fail "network $COMPOSE_NET not found; run 'bash scripts/up.sh' first"

common=(
    -Dliquibase.changeLogFile="src/main/resources/db/$db/changelog.sql"
    -Dliquibase.defaultSchemaName="$db"
    -Dliquibase.url="jdbc:postgresql://cc-pg:5432/$db"
    -Dliquibase.username="${CC_MIGRATOR_USER:-cc_migrator}"
    -Dliquibase.password="${CC_MIGRATOR_PASSWORD:-cc_migrator}"
)

if [ "$mode" = "up" ]; then
    log "applying pending changesets of $db"
    goal=liquibase:update
    args=()
else
    log "rolling back $mode changeset(s) of $db"
    goal=liquibase:rollback
    args=(-Dliquibase.rollbackCount="$mode")
fi

docker run --rm \
    --network "$COMPOSE_NET" \
    -v "$ROOT/app:/app" \
    -w /app \
    -e MAVEN_CONFIG=/tmp/.m2 \
    "${CC_MAVEN_IMAGE:-docker.1ms.run/library/maven:3.9-eclipse-temurin-21}" \
    mvn -B -ntp "$goal" "${args[@]+"${args[@]}"}" "${common[@]}"

if [ "$mode" = "up" ]; then
    ok "$db is up to date"
else
    ok "rolled back $db by $mode changeset(s); 'bash scripts/migrate.sh $db up' re-applies"
fi
