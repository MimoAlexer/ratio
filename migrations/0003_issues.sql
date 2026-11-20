ALTER TABLE issues
    ADD COLUMN labels text[] NOT NULL DEFAULT '{}',
    ADD COLUMN manual_severity text NOT NULL DEFAULT 'none'
        CHECK (manual_severity IN ('none','low','medium','high','critical')),
    ADD COLUMN finding_closed boolean NOT NULL DEFAULT false;

UPDATE issues SET manual_severity = 'critical' WHERE security_critical;

ALTER TABLE issue_events
    ADD COLUMN action text NOT NULL DEFAULT 'status',
    ADD COLUMN before_state jsonb,
    ADD COLUMN after_state jsonb,
    ADD COLUMN revision_after bigint,
    ADD COLUMN reverses uuid,
    ADD COLUMN ingested_at timestamptz NOT NULL DEFAULT now();

CREATE TABLE issue_blockers (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    issue_id uuid NOT NULL,
    blocker_id uuid NOT NULL,
    PRIMARY KEY (issue_id, blocker_id),
    CHECK (issue_id <> blocker_id),
    FOREIGN KEY (owner_id, project_id) REFERENCES projects(owner_id, id) ON DELETE CASCADE,
    FOREIGN KEY (owner_id, issue_id) REFERENCES issues(owner_id, id) ON DELETE CASCADE,
    FOREIGN KEY (project_id, issue_id) REFERENCES issues(project_id, id) ON DELETE CASCADE,
    FOREIGN KEY (owner_id, blocker_id) REFERENCES issues(owner_id, id) ON DELETE CASCADE,
    FOREIGN KEY (project_id, blocker_id) REFERENCES issues(project_id, id) ON DELETE CASCADE
);
CREATE INDEX issue_blockers_reverse ON issue_blockers(owner_id, project_id, blocker_id, issue_id);

CREATE INDEX issue_checkpoint_order ON issues (owner_id, checkpoint_at DESC, updated_at DESC, id)
    WHERE NOT archived AND checkpoint_at IS NOT NULL AND status IN ('backlog','ready','in_progress');
CREATE INDEX issue_progress_order ON issues (owner_id, updated_at DESC, id)
    WHERE NOT archived AND status = 'in_progress';
CREATE INDEX issue_manual_critical ON issues (owner_id, project_id, updated_at DESC)
    WHERE security_critical AND NOT archived;
-- A tsvector is limited to 1 MiB, while an issue description can reach 2 MiB.
-- Index ordinary issues in full and use an exact-text fallback for longer descriptions.
CREATE FUNCTION issue_search_vector(issue_title text, issue_body text, next_action text)
RETURNS tsvector LANGUAGE sql IMMUTABLE PARALLEL SAFE STRICT AS $$
    SELECT to_tsvector('simple', issue_title || ' ' || next_action || ' ' || left(issue_body, 65536))
$$;
CREATE INDEX issue_search_text ON issues USING gin (issue_search_vector(title, body, next_action));
DROP INDEX issues_search;

CREATE FUNCTION issue_search_labels(issue_labels text[])
RETURNS text[] LANGUAGE sql IMMUTABLE PARALLEL SAFE STRICT AS $$
    SELECT ARRAY(SELECT lower(label) FROM unnest(issue_labels) AS label)
$$;
CREATE INDEX issue_search_tags ON issues USING gin (issue_search_labels(labels));
CREATE INDEX issue_long_descriptions ON issues(owner_id, id) WHERE octet_length(body) > 65536;
CREATE INDEX issue_revision_history ON issue_events (owner_id, issue_id, revision_after DESC)
    WHERE revision_after IS NOT NULL;
