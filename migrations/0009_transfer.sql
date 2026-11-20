-- Keep image bytes and their metadata in the same transaction and backup as their notes.
CREATE TABLE transfer_blobs (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(), owner_id uuid NOT NULL, project_id uuid NOT NULL,
    path text NOT NULL, content_type text NOT NULL CHECK(content_type IN ('image/png','image/jpeg','image/gif','image/webp')),
    bytes bytea NOT NULL, sha256 text NOT NULL CHECK(length(sha256)=64),
    UNIQUE(project_id,path), UNIQUE(project_id,id),
    FOREIGN KEY(owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE
);
CREATE INDEX transfer_blobs_owner ON transfer_blobs(owner_id,project_id);

CREATE TABLE transfer_records (
    owner_id uuid NOT NULL, project_id uuid NOT NULL, kind text NOT NULL CHECK(kind IN ('note','issue','blob')),
    source_id uuid NOT NULL, local_id uuid NOT NULL, base_hash text NOT NULL, path text NOT NULL,
    PRIMARY KEY(project_id,kind,source_id),
    FOREIGN KEY(owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE
);
CREATE TABLE transfer_linear (
    owner_id uuid NOT NULL, project_id uuid NOT NULL, source_id text NOT NULL, issue_id uuid NOT NULL,
    base jsonb NOT NULL, parent_source text, parent_id uuid,
    PRIMARY KEY(project_id,source_id), UNIQUE(project_id,issue_id),
    FOREIGN KEY(owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE,
    FOREIGN KEY(project_id,issue_id) REFERENCES issues(project_id,id) ON DELETE CASCADE,
    FOREIGN KEY(project_id,parent_id) REFERENCES issues(project_id,id) ON DELETE SET NULL (parent_id)
);
CREATE TABLE transfer_previews (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(), owner_id uuid NOT NULL UNIQUE, project_id uuid NOT NULL,
    format text NOT NULL CHECK(format IN ('linear','vault')), folder text NOT NULL, payload bytea,
    report jsonb NOT NULL, stale boolean NOT NULL DEFAULT false,
    expires_at timestamptz NOT NULL DEFAULT now()+interval '30 minutes',
    FOREIGN KEY(owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE
);
CREATE INDEX transfer_previews_expiry ON transfer_previews(expires_at);

-- A user's conflict selections apply to exactly the version reviewed. Normal edits and
-- background writes invalidate an outstanding preview, including edits to linked rows.
CREATE FUNCTION invalidate_transfer_preview() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='DELETE' THEN
        UPDATE transfer_previews SET stale=true WHERE owner_id=OLD.owner_id AND project_id=OLD.project_id AND payload IS NOT NULL AND NOT stale;
    ELSE
        UPDATE transfer_previews SET stale=true WHERE owner_id=NEW.owner_id AND project_id=NEW.project_id AND payload IS NOT NULL AND NOT stale;
    END IF;
    RETURN NULL;
END $$;
DO $$ DECLARE rel text;
BEGIN
    FOR rel IN SELECT tablename FROM pg_tables WHERE schemaname=current_schema() AND tablename IN
        ('notes','issues','issue_events','issue_blockers','note_issue_sources','note_links','milestones','milestone_dependencies','milestone_issues',
        'releases','release_membership_events','roadmap_weeks_off','release_members','release_entries','release_amendments','release_carryovers','transfer_blobs','transfer_records','transfer_linear')
    LOOP
        EXECUTE format('CREATE TRIGGER invalidate_import AFTER INSERT OR UPDATE OR DELETE ON %I FOR EACH ROW EXECUTE FUNCTION invalidate_transfer_preview()',rel);
    END LOOP;
END $$;
