-- Each saved board and checkpoint retains a complete drawing. Meter the
-- current document and each retained copy in canonical UTF-8 JSON, regardless
-- of TOAST compression or whether the checkpoint was created during a restore.
DO $$
DECLARE
    meter record;
BEGIN
    FOR meter IN
        SELECT * FROM (VALUES
            ('canvas_boards', 'octet_length(title)::bigint + octet_length(document::text)'),
            ('canvas_checkpoints', 'octet_length(name)::bigint + octet_length(title) + octet_length(document::text)')
        ) AS definitions(relation, byte_expression)
    LOOP
        EXECUTE format('LOCK TABLE %I IN SHARE ROW EXCLUSIVE MODE', meter.relation);
        EXECUTE format(
            'UPDATE users u SET content_bytes = u.content_bytes + s.bytes
             FROM (SELECT owner_id, sum(%s)::bigint AS bytes FROM %I GROUP BY owner_id) s
             WHERE u.id = s.owner_id AND s.bytes > 0', meter.byte_expression, meter.relation
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
        EXECUTE format(
            'CREATE TRIGGER invalidate_import AFTER INSERT OR UPDATE OR DELETE ON %I
             FOR EACH ROW EXECUTE FUNCTION invalidate_transfer_preview()', meter.relation
        );
    END LOOP;
END $$;
