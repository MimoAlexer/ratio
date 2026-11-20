CREATE TABLE workflow_views (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (btrim(name) <> ''),
    project_id uuid,
    status text NOT NULL DEFAULT '' CHECK (status IN ('','backlog','ready','in_progress','done','canceled')),
    priority text NOT NULL DEFAULT '' CHECK (priority IN ('','low','normal','high','urgent')),
    kind text NOT NULL DEFAULT '' CHECK (kind IN ('','feature','bug','maintenance','security')),
    critical text NOT NULL DEFAULT '' CHECK (critical IN ('','open')),
    scope text NOT NULL DEFAULT 'active' CHECK (scope IN ('active','archived','all')),
    layout text NOT NULL DEFAULT 'list' CHECK (layout IN ('list','board')),
    search text NOT NULL DEFAULT '',
    FOREIGN KEY (owner_id, project_id) REFERENCES projects(owner_id, id) ON DELETE CASCADE
);
CREATE UNIQUE INDEX workflow_view_name ON workflow_views(owner_id, lower(name));

CREATE TABLE issue_templates (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (btrim(name) <> ''),
    title text NOT NULL CHECK (btrim(title) <> ''),
    body text NOT NULL DEFAULT '',
    next_action text NOT NULL DEFAULT '',
    status text NOT NULL CHECK (status IN ('backlog','ready','in_progress','done','canceled')),
    priority text NOT NULL CHECK (priority IN ('low','normal','high','urgent')),
    kind text NOT NULL CHECK (kind IN ('feature','bug','maintenance','security'))
);
CREATE UNIQUE INDEX issue_template_name ON issue_templates(owner_id, lower(name));

CREATE TABLE portfolio_digest_schedules (
    owner_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    enabled boolean NOT NULL,
    enabled_at timestamptz NOT NULL DEFAULT now(),
    next_week date NOT NULL CHECK (extract(isodow FROM next_week) = 1)
);
CREATE INDEX portfolio_digest_schedule_due ON portfolio_digest_schedules (next_week, owner_id)
    WHERE enabled;

CREATE TABLE portfolio_digests (
    owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    week_start date NOT NULL CHECK (extract(isodow FROM week_start) = 1),
    generated_at timestamptz NOT NULL DEFAULT now(),
    new_issues bigint NOT NULL CHECK (new_issues >= 0),
    issues_done bigint NOT NULL CHECK (issues_done >= 0),
    milestone_targets bigint NOT NULL CHECK (milestone_targets >= 0),
    releases_published bigint NOT NULL CHECK (releases_published >= 0),
    PRIMARY KEY (owner_id, week_start)
);

CREATE FUNCTION pause_portfolio_digest_on_downgrade() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE portfolio_digest_schedules SET enabled = false WHERE owner_id = NEW.id AND enabled;
    RETURN NEW;
END
$$;
CREATE TRIGGER portfolio_digest_plan_change AFTER UPDATE OF plan ON users
    FOR EACH ROW WHEN (OLD.plan = 'max' AND NEW.plan IS DISTINCT FROM 'max')
    EXECUTE FUNCTION pause_portfolio_digest_on_downgrade();
