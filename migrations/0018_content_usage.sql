-- Bill the UTF-8 source and image bytes retained by the owner. Derived links,
-- historical snapshots, import bookkeeping and provider caches are excluded.
-- Archiving a project or moving an image to Trash preserves its content.
ALTER TABLE users
    ADD COLUMN content_bytes bigint NOT NULL DEFAULT 0 CHECK (content_bytes >= 0),
    ADD COLUMN content_bytes_at_tx_start bigint;

-- The counter is immediately visible to import previews and preflight checks.
-- Check the final net growth only once per owner/transaction. This permits
-- imports to replace large records in either order and lets downgraded owners
-- keep editing while their total usage stays level or falls.
CREATE FUNCTION enforce_workspace_storage() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    final_bytes bigint;
    final_limit bigint;
BEGIN
    -- Only the final content total persists into backups and the next write.
    UPDATE users SET content_bytes_at_tx_start = NULL WHERE id = NEW.id
    RETURNING content_bytes, CASE plan WHEN 'max' THEN 5000000000::bigint
            WHEN 'plus' THEN 1000000000::bigint ELSE 100000000::bigint END
        INTO final_bytes, final_limit;
    IF final_bytes > NEW.content_bytes_at_tx_start AND final_bytes > final_limit THEN
        RAISE EXCEPTION 'Workspace storage limit exceeded'
            USING ERRCODE = '23514', CONSTRAINT = 'workspace_storage_limit';
    END IF;
    RETURN NULL;
END $$;
CREATE CONSTRAINT TRIGGER enforce_workspace_storage
    AFTER UPDATE ON users DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW WHEN (OLD.content_bytes_at_tx_start IS NULL AND NEW.content_bytes_at_tx_start IS NOT NULL)
    EXECUTE FUNCTION enforce_workspace_storage();

-- Each statement aggregates its transition rows, then updates each affected
-- owner once. Transfers between owners debit and credit separately in UUID
-- order. Cascading project deletes fire these same triggers. During account
-- deletion the owner has already gone, so there is no surviving meter to debit.
CREATE FUNCTION adjust_workspace_storage() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    parts text[] := '{}';
    adjustment record;
BEGIN
    -- The expression is supplied only by the DDL below. Row data never enters
    -- the SQL text. octet_length reads the logical length of TOASTed images.
    IF TG_OP IN ('INSERT', 'UPDATE') THEN
        parts := array_append(parts, format('SELECT owner_id, (%s)::bigint AS bytes FROM added', TG_ARGV[0]));
    END IF;
    IF TG_OP IN ('DELETE', 'UPDATE') THEN
        parts := array_append(parts, format('SELECT owner_id, -(%s)::bigint AS bytes FROM removed', TG_ARGV[0]));
    END IF;
    FOR adjustment IN EXECUTE format(
        'SELECT owner_id, sum(bytes)::bigint AS delta FROM (%s) changes
         GROUP BY owner_id HAVING sum(bytes) <> 0 ORDER BY owner_id',
        array_to_string(parts, ' UNION ALL ')
    ) LOOP
        UPDATE users
           SET content_bytes_at_tx_start = COALESCE(content_bytes_at_tx_start, content_bytes),
               content_bytes = content_bytes + adjustment.delta
         WHERE id = adjustment.owner_id;
    END LOOP;
    RETURN NULL;
END $$;

-- Backfill and install triggers atomically, using one definition per content
-- class for historical and future rows. Lock out content writes until both
-- steps are complete. Existing over-cap workspaces can still reduce usage.
DO $$
DECLARE
    meter record;
BEGIN
    FOR meter IN
        SELECT * FROM (VALUES
            ('issues', 'octet_length(title)::bigint + octet_length(body) + octet_length(next_action) + octet_length(array_to_string(labels, ''''))'),
            ('notes', 'octet_length(title)::bigint + octet_length(path) + octet_length(body)'),
            ('transfer_blobs', 'octet_length(bytes)'),
            ('workflow_views', 'octet_length(name)::bigint + octet_length(search)'),
            ('issue_templates', 'octet_length(name)::bigint + octet_length(title) + octet_length(body) + octet_length(next_action)'),
            ('milestones', 'octet_length(title)::bigint + octet_length(details)'),
            ('roadmap_weeks_off', 'octet_length(note)'),
            ('releases', 'octet_length(name)::bigint + octet_length(version) + octet_length(source_url)'),
            ('release_amendments', 'octet_length(reason)::bigint + octet_length(replacement)')
        ) AS definitions(relation, byte_expression)
    LOOP
        EXECUTE format('LOCK TABLE %I IN SHARE ROW EXCLUSIVE MODE', meter.relation);
        EXECUTE format(
            'UPDATE users u SET content_bytes = u.content_bytes + s.bytes
             FROM (SELECT owner_id, sum(%s)::bigint AS bytes FROM %I GROUP BY owner_id) s
             WHERE u.id = s.owner_id', meter.byte_expression, meter.relation
        );
        EXECUTE format(
            'CREATE TRIGGER workspace_content_insert AFTER INSERT ON %I REFERENCING NEW TABLE AS added
             FOR EACH STATEMENT EXECUTE FUNCTION adjust_workspace_storage(%L)', meter.relation, meter.byte_expression
        );
        EXECUTE format(
            'CREATE TRIGGER workspace_content_edit AFTER UPDATE ON %I REFERENCING OLD TABLE AS removed NEW TABLE AS added
             FOR EACH STATEMENT EXECUTE FUNCTION adjust_workspace_storage(%L)', meter.relation, meter.byte_expression
        );
        EXECUTE format(
            'CREATE TRIGGER workspace_content_delete AFTER DELETE ON %I REFERENCING OLD TABLE AS removed
             FOR EACH STATEMENT EXECUTE FUNCTION adjust_workspace_storage(%L)', meter.relation, meter.byte_expression
        );
    END LOOP;
END $$;
