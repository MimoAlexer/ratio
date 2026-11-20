CREATE TABLE milestones (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    title text NOT NULL CHECK (length(trim(title)) BETWEEN 1 AND 200),
    details text NOT NULL DEFAULT '' CHECK (length(details) <= 8000),
    status text NOT NULL DEFAULT 'planned' CHECK (status IN ('planned','in_progress','reached')),
    start_on date,
    end_on date,
    target_on date,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    CHECK ((start_on IS NULL) = (end_on IS NULL)),
    CHECK (start_on <= end_on),
    UNIQUE (owner_id,project_id,id),
    FOREIGN KEY (owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE
);

CREATE INDEX milestones_calendar ON milestones(owner_id,project_id,target_on);
CREATE TABLE milestone_dependencies (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    prerequisite_id uuid NOT NULL,
    dependent_id uuid NOT NULL,
    PRIMARY KEY (owner_id,project_id,prerequisite_id,dependent_id),
    CONSTRAINT milestone_dependencies_not_self CHECK (prerequisite_id <> dependent_id),
    FOREIGN KEY (owner_id,project_id,prerequisite_id) REFERENCES milestones(owner_id,project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (owner_id,project_id,dependent_id) REFERENCES milestones(owner_id,project_id,id) ON DELETE CASCADE
);
CREATE INDEX milestone_dependencies_incoming ON milestone_dependencies(owner_id,project_id,dependent_id);

-- Native imports and roadmap forms share the same acyclic graph. The lock
-- serializes graph writers in the project's read-committed transactions.
CREATE FUNCTION check_milestone_dependencies() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    PERFORM id FROM projects
      WHERE owner_id = NEW.owner_id AND id = NEW.project_id FOR NO KEY UPDATE;
    IF EXISTS (
        WITH RECURSIVE downstream(id) AS (
            SELECT NEW.dependent_id
            UNION
            SELECT link.dependent_id FROM milestone_dependencies link
              JOIN downstream prev ON prev.id = link.prerequisite_id
              WHERE link.owner_id = NEW.owner_id AND link.project_id = NEW.project_id
        )
        SELECT FROM downstream WHERE id = NEW.prerequisite_id
    ) THEN
        RAISE EXCEPTION 'A milestone cannot depend on a downstream milestone'
          USING ERRCODE = '23514', CONSTRAINT = 'milestone_dependencies_acyclic';
    END IF;
    RETURN NULL;
END $$;
CREATE TRIGGER milestone_dependencies_acyclic AFTER INSERT OR UPDATE ON milestone_dependencies
    FOR EACH ROW EXECUTE FUNCTION check_milestone_dependencies();

CREATE TABLE milestone_issues (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    milestone_id uuid NOT NULL,
    issue_id uuid NOT NULL,
    PRIMARY KEY (owner_id,project_id,milestone_id,issue_id),
    FOREIGN KEY (owner_id,project_id,milestone_id) REFERENCES milestones(owner_id,project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,issue_id) REFERENCES issues(project_id,id) ON DELETE CASCADE
);
CREATE INDEX milestone_issues_by_issue ON milestone_issues(project_id,issue_id);

CREATE TABLE roadmap_weeks_off (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    week_of date NOT NULL CHECK (extract(isodow FROM week_of) = 1),
    note text NOT NULL DEFAULT '' CHECK (length(note) <= 200),
    PRIMARY KEY (owner_id,project_id,week_of),
    FOREIGN KEY (owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE
);
