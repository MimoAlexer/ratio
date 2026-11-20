#!/usr/bin/env bash
# Run as the database administrator before the first deployment or to rotate the app password.
set -euo pipefail
: "${WORKSPACE_DB_PASSWORD:?Supply a dedicated random runtime password}"
export PGUSER=${PGUSER:-${POSTGRES_USER:?Set PGUSER to the administrative role}}
export PGDATABASE=${PGDATABASE:-${POSTGRES_DB:?Set PGDATABASE to the application database}}
psql --no-password --no-psqlrc --quiet --set ON_ERROR_STOP=1 --single-transaction <<'SQL'
\getenv runtime_password WORKSPACE_DB_PASSWORD
DO $$
BEGIN
  IF EXISTS (
    SELECT FROM pg_roles r WHERE r.rolname = 'workspace_app' AND
      (r.rolsuper OR r.rolcreatedb OR r.rolcreaterole OR r.rolreplication OR r.rolbypassrls OR
       EXISTS (SELECT FROM pg_auth_members WHERE member = r.oid) OR
       EXISTS (SELECT FROM pg_database WHERE datdba = r.oid) OR
       EXISTS (SELECT FROM pg_namespace WHERE nspowner = r.oid) OR
       EXISTS (SELECT FROM pg_class WHERE relowner = r.oid))
  ) THEN
    RAISE EXCEPTION 'workspace_app already has elevated privileges or ownership';
  END IF;
  IF NOT EXISTS (SELECT FROM pg_roles WHERE rolname = 'workspace_app') THEN
    CREATE ROLE workspace_app LOGIN NOINHERIT NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION NOBYPASSRLS;
  END IF;
END $$;
ALTER ROLE workspace_app WITH LOGIN PASSWORD :'runtime_password';
REVOKE CREATE ON SCHEMA public FROM PUBLIC, workspace_app;
GRANT USAGE ON SCHEMA public TO workspace_app;
SELECT format('GRANT CONNECT ON DATABASE %I TO workspace_app', current_database()) \gexec
SQL
