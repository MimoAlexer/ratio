-- The migration owner runs these grants after migrations. workspace_app owns no tables.
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO workspace_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO workspace_app;
REVOKE INSERT, UPDATE, DELETE, TRUNCATE ON _sqlx_migrations FROM workspace_app;
REVOKE UPDATE, DELETE, TRUNCATE ON account_deletions FROM workspace_app;
REVOKE DELETE, TRUNCATE, UPDATE ON auth_recovery_events FROM workspace_app;
GRANT UPDATE (event_id, recovered_at) ON auth_recovery_events TO workspace_app;
-- The recovery operator is responsible for placing and lifting identity holds.
REVOKE INSERT, UPDATE ON users FROM workspace_app;
GRANT INSERT (id, email, plan, created_at, timezone), UPDATE (email, plan, timezone) ON users TO workspace_app;
-- Storage accounting runs as the caller when an issue, note or image changes.
GRANT UPDATE (content_bytes, content_bytes_at_tx_start) ON users TO workspace_app;
