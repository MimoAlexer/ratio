CREATE TABLE releases (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    name text NOT NULL CHECK (char_length(trim(name)) BETWEEN 1 AND 150),
    version text NOT NULL DEFAULT '' CHECK (char_length(version) <= 100),
    source_url text NOT NULL DEFAULT '' CHECK (char_length(source_url) <= 1024),
    target_on date,
    published_on date,
    published_at timestamptz,
    publish_command uuid,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE,
    UNIQUE (owner_id,id),
    UNIQUE (project_id,id),
    UNIQUE (owner_id,project_id,id),
    UNIQUE (owner_id,publish_command),
    CHECK ((published_on IS NULL) = (published_at IS NULL)),
    CHECK ((publish_command IS NULL) = (published_at IS NULL))
);

CREATE TABLE release_members (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    release_id uuid NOT NULL,
    issue_id uuid NOT NULL,
    PRIMARY KEY (release_id,issue_id),
    UNIQUE (owner_id,issue_id),
    FOREIGN KEY (owner_id,project_id,release_id) REFERENCES releases(owner_id,project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,issue_id) REFERENCES issues(project_id,id) ON DELETE CASCADE
);

CREATE TABLE release_membership_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    release_id uuid NOT NULL,
    issue_id uuid NOT NULL,
    in_scope boolean NOT NULL,
    status text NOT NULL CHECK (status IN ('backlog','ready','in_progress','done','canceled')),
    occurred_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    source text NOT NULL DEFAULT 'assignment' CHECK (char_length(source) BETWEEN 1 AND 100),
    FOREIGN KEY (owner_id,project_id,release_id) REFERENCES releases(owner_id,project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,issue_id) REFERENCES issues(project_id,id) ON DELETE CASCADE
);
CREATE INDEX release_membership_review ON release_membership_events (release_id,issue_id,occurred_at,id);
CREATE INDEX release_timeline ON releases (owner_id,project_id,published_on DESC);

CREATE TABLE release_entries (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    release_id uuid NOT NULL,
    issue_id uuid NOT NULL,
    number bigint NOT NULL,
    title text NOT NULL,
    issue_revision bigint NOT NULL CHECK (issue_revision > 0),
    PRIMARY KEY (release_id,issue_id),
    FOREIGN KEY (owner_id,project_id,release_id) REFERENCES releases(owner_id,project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,issue_id) REFERENCES issues(project_id,id) ON DELETE CASCADE
);
CREATE INDEX release_issue_shipments ON release_entries (owner_id,issue_id,release_id);

CREATE TABLE release_carryovers (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    release_id uuid NOT NULL,
    issue_id uuid NOT NULL,
    number bigint NOT NULL,
    title text NOT NULL,
    status text NOT NULL CHECK (status IN ('backlog','ready','in_progress','done','canceled')),
    issue_revision bigint NOT NULL CHECK (issue_revision > 0),
    next_release_id uuid,
    PRIMARY KEY (release_id,issue_id),
    CHECK (next_release_id IS DISTINCT FROM release_id),
    FOREIGN KEY (owner_id,project_id,release_id) REFERENCES releases(owner_id,project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,issue_id) REFERENCES issues(project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,next_release_id) REFERENCES releases(project_id,id) DEFERRABLE INITIALLY DEFERRED
);

CREATE TABLE release_amendments (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    release_id uuid NOT NULL,
    issue_id uuid NOT NULL,
    actor_id uuid NOT NULL CHECK (actor_id = owner_id),
    reason text NOT NULL CHECK (char_length(trim(reason)) BETWEEN 1 AND 2000),
    replacement text NOT NULL CHECK (char_length(trim(replacement)) BETWEEN 1 AND 2000),
    created_at timestamptz NOT NULL DEFAULT clock_timestamp(),
    FOREIGN KEY (owner_id,project_id,release_id) REFERENCES releases(owner_id,project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (release_id,issue_id) REFERENCES release_entries(release_id,issue_id) ON DELETE CASCADE
);
CREATE INDEX release_amendment_history ON release_amendments (release_id,created_at,id);

-- The publisher writes all snapshot rows first and seals the header last in one transaction.
CREATE FUNCTION guard_release_freeze() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF OLD.published_at IS NOT NULL THEN
        RAISE EXCEPTION 'A published release is sealed' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER freeze_release BEFORE UPDATE ON releases
    FOR EACH ROW EXECUTE FUNCTION guard_release_freeze();

CREATE FUNCTION guard_release_record() RETURNS trigger LANGUAGE plpgsql AS $$
DECLARE
    sealed boolean;
BEGIN
    IF TG_OP = 'UPDATE' THEN
        RAISE EXCEPTION 'Release history records are append-only' USING ERRCODE='23514';
    END IF;
    SELECT published_at IS NOT NULL INTO sealed FROM releases WHERE id=NEW.release_id FOR SHARE;
    IF TG_TABLE_NAME = 'release_amendments' AND NOT sealed THEN
        RAISE EXCEPTION 'Only published releases accept amendments' USING ERRCODE='23514';
    ELSIF TG_TABLE_NAME <> 'release_amendments' AND sealed THEN
        RAISE EXCEPTION 'This release is already sealed' USING ERRCODE='23514';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER release_entries_guard BEFORE INSERT OR UPDATE ON release_entries
    FOR EACH ROW EXECUTE FUNCTION guard_release_record();
CREATE TRIGGER release_carryovers_guard BEFORE INSERT OR UPDATE ON release_carryovers
    FOR EACH ROW EXECUTE FUNCTION guard_release_record();
CREATE TRIGGER release_amendments_guard BEFORE INSERT OR UPDATE ON release_amendments
    FOR EACH ROW EXECUTE FUNCTION guard_release_record();
CREATE TRIGGER release_members_guard BEFORE INSERT OR UPDATE ON release_members
    FOR EACH ROW EXECUTE FUNCTION guard_release_record();
CREATE TRIGGER release_events_guard BEFORE INSERT OR UPDATE ON release_membership_events
    FOR EACH ROW EXECUTE FUNCTION guard_release_record();

-- Project/account teardown is allowed after the parent disappears. A single-row
-- delete against retained release history must go through an amendment instead.
CREATE FUNCTION guard_release_history_delete() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF EXISTS(SELECT 1 FROM projects WHERE id=OLD.project_id)
       AND (TG_TABLE_NAME <> 'releases' OR (to_jsonb(OLD)->>'published_at') IS NOT NULL) THEN
        RAISE EXCEPTION 'Release history is retained with the project' USING ERRCODE='23514';
    END IF;
    RETURN OLD;
END $$;
CREATE TRIGGER release_header_delete BEFORE DELETE ON releases
    FOR EACH ROW EXECUTE FUNCTION guard_release_history_delete();
CREATE TRIGGER release_entries_delete BEFORE DELETE ON release_entries
    FOR EACH ROW EXECUTE FUNCTION guard_release_history_delete();
CREATE TRIGGER release_carryovers_delete BEFORE DELETE ON release_carryovers
    FOR EACH ROW EXECUTE FUNCTION guard_release_history_delete();
CREATE TRIGGER release_amendments_delete BEFORE DELETE ON release_amendments
    FOR EACH ROW EXECUTE FUNCTION guard_release_history_delete();
CREATE TRIGGER release_events_delete BEFORE DELETE ON release_membership_events
    FOR EACH ROW EXECUTE FUNCTION guard_release_history_delete();
