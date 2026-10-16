#!/bin/sh
# SPDX-License-Identifier: AGPL-3.0-only
# Runs ONCE on first database initialization, as the Postgres superuser (POSTGRES_USER),
# BEFORE the server ever connects. Two jobs:
#
#   1. Install pgcrypto here (as superuser) so migration 0001's
#      `CREATE EXTENSION IF NOT EXISTS pgcrypto` is a no-op even when the serving role is
#      not a superuser.
#   2. Create the NON-OWNER `yapstack_app` LOGIN role the RLS design relies on (§5), with
#      the password from YAPSTACK_APP_PASSWORD. Migration 0001 also creates this role
#      idempotently (NOLOGIN) and GRANTs it per-table privileges; creating it here FIRST
#      (LOGIN + password) lets a hardened deployment run the SERVING connection as this
#      non-owner role, which can never bypass RLS. The default single-role compose
#      connects as the owner (FORCE RLS still isolates it) — see docs/self-hosting.md.
set -eu

: "${YAPSTACK_APP_PASSWORD:?YAPSTACK_APP_PASSWORD must be set}"

# psql runs as the superuser via the entrypoint's trust socket. The password is passed
# as a bound psql variable (never interpolated into the SQL text).
# The password is a bound psql variable substituted only OUTSIDE any dollar-quoting;
# format(%L) quotes it as a SQL literal and \gexec runs the generated statement only
# when the role does not already exist (no CREATE ROLE IF NOT EXISTS in Postgres).
psql -v ON_ERROR_STOP=1 \
     --username "$POSTGRES_USER" \
     --dbname "$POSTGRES_DB" \
     -v app_password="$YAPSTACK_APP_PASSWORD" <<'SQL'
CREATE EXTENSION IF NOT EXISTS "pgcrypto";

SELECT format('CREATE ROLE yapstack_app LOGIN PASSWORD %L', :'app_password')
WHERE NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'yapstack_app')
\gexec
SQL

echo "yapstack: pgcrypto installed and yapstack_app role ready"
