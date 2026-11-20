-- An absent row is revision zero. Existing workspaces need no content scan.
CREATE TABLE workspace_device_changes (
    owner_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    revision bigint NOT NULL CHECK (revision > 0),
    writer_xid xid8 NOT NULL DEFAULT pg_current_xact_id()
);

-- Transition tables let an import notify each owner once per statement. The
-- xid check also coalesces changes across all statements in one transaction.
CREATE FUNCTION record_workspace_device_change() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    affected text;
BEGIN
    IF TG_OP = 'INSERT' THEN
        affected := 'SELECT owner_id FROM added';
    ELSIF TG_OP = 'DELETE' THEN
        affected := 'SELECT owner_id FROM removed';
    ELSE
        -- Join on the declared primary key. A changed key appears as a removal
        -- and an addition. A transfer between owners invalidates both views.
        affected := format(
            'SELECT owners.owner_id
               FROM removed AS previous FULL JOIN added AS current USING (%s)
               CROSS JOIN LATERAL (VALUES (previous.owner_id), (current.owner_id)) AS owners(owner_id)
              WHERE previous IS DISTINCT FROM current',
            TG_ARGV[0]
        );
    END IF;

    -- A parent account can already be gone during cascaded deletes. Updating
    -- only surviving users also avoids resurrecting their change records.
    EXECUTE format(
        'INSERT INTO workspace_device_changes AS changes (owner_id, revision)
         SELECT id, 1 FROM users WHERE id IN (%s) ORDER BY id
         ON CONFLICT (owner_id) DO UPDATE
           SET revision = changes.revision + 1, writer_xid = excluded.writer_xid
           WHERE changes.writer_xid <> excluded.writer_xid OR changes.xmin <> excluded.writer_xid::xid',
        affected
    );
    -- Checking both the full XID and the physical tuple handles wraparound
    -- and logical restores into a cluster with an overlapping XID history.
    RETURN NULL;
END $$;

DO $$
DECLARE
    tracked record;
BEGIN
    -- Native workspace content only. Provider grants, alerts, and sync jobs
    -- have independent authorization and never advance this revision.
    FOR tracked IN SELECT * FROM (VALUES
        ('projects', 'id'),
        ('issues', 'id'),
        ('issue_events', 'id'),
        ('issue_blockers', 'issue_id, blocker_id'),
        ('notes', 'id'),
        ('note_links', 'source_id, ordinal'),
        ('note_issue_sources', 'issue_id'),
        ('milestones', 'id'),
        ('milestone_dependencies', 'owner_id, project_id, prerequisite_id, dependent_id'),
        ('milestone_issues', 'owner_id, project_id, milestone_id, issue_id'),
        ('milestone_releases', 'owner_id, project_id, milestone_id'),
        ('roadmap_weeks_off', 'owner_id, project_id, week_of'),
        ('roadmap_release_spans', 'owner_id, project_id, release_id'),
        ('releases', 'id'),
        ('release_members', 'release_id, issue_id'),
        ('release_membership_events', 'id'),
        ('release_entries', 'release_id, issue_id'),
        ('release_carryovers', 'release_id, issue_id'),
        ('release_amendments', 'id'),
        ('transfer_blobs', 'id'),
        ('workflow_views', 'id'),
        ('issue_templates', 'id')
    ) AS definition(relation, key_columns)
    LOOP
        EXECUTE format(
            'CREATE TRIGGER workspace_device_insert AFTER INSERT ON %I REFERENCING NEW TABLE AS added
             FOR EACH STATEMENT EXECUTE FUNCTION record_workspace_device_change()', tracked.relation);
        EXECUTE format(
            'CREATE TRIGGER workspace_device_edit AFTER UPDATE ON %I REFERENCING OLD TABLE AS removed NEW TABLE AS added
             FOR EACH STATEMENT EXECUTE FUNCTION record_workspace_device_change(%L)', tracked.relation, tracked.key_columns);
        EXECUTE format(
            'CREATE TRIGGER workspace_device_delete AFTER DELETE ON %I REFERENCING OLD TABLE AS removed
             FOR EACH STATEMENT EXECUTE FUNCTION record_workspace_device_change()', tracked.relation);
    END LOOP;
END $$;
