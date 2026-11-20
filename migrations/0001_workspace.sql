CREATE TABLE users (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    email text NOT NULL UNIQUE CHECK (length(email) BETWEEN 3 AND 320),
    plan text NOT NULL DEFAULT 'free' CHECK (plan IN ('free','plus','max')),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE projects (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name text NOT NULL CHECK (length(trim(name)) BETWEEN 1 AND 120),
    key text NOT NULL CHECK (key ~ '^[A-Z][A-Z0-9]{0,11}$'),
    archived boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(owner_id,id), UNIQUE(owner_id,key)
);
CREATE TABLE issues (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id uuid NOT NULL,
    number bigint NOT NULL CHECK (number > 0),
    title text NOT NULL CHECK (length(trim(title)) BETWEEN 1 AND 500),
    body text NOT NULL DEFAULT '',
    status text NOT NULL DEFAULT 'ready' CHECK (status IN ('backlog','ready','in_progress','done','canceled')),
    priority text NOT NULL DEFAULT 'normal' CHECK (priority IN ('low','normal','high','urgent')),
    kind text NOT NULL DEFAULT 'feature' CHECK (kind IN ('feature','bug','maintenance','security')),
    security_critical boolean NOT NULL DEFAULT false,
    next_action text NOT NULL DEFAULT '',
    checkpoint_at timestamptz,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    started_at timestamptz, completed_at timestamptz,
    archived boolean NOT NULL DEFAULT false,
    FOREIGN KEY (owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE,
    UNIQUE(project_id,number), UNIQUE(owner_id,id), UNIQUE(project_id,id), UNIQUE(owner_id,project_id,id)
);
CREATE INDEX issues_workspace_status ON issues(owner_id,project_id,status,updated_at DESC);
CREATE INDEX issues_resume ON issues(owner_id,checkpoint_at DESC NULLS LAST) WHERE NOT archived;
CREATE INDEX issues_search ON issues USING gin(to_tsvector('simple', title || ' ' || body));
CREATE TABLE issue_events (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    issue_id uuid NOT NULL,
    old_status text, new_status text NOT NULL,
    occurred_at timestamptz NOT NULL DEFAULT now(),
    command_id uuid,
    FOREIGN KEY(owner_id,project_id,issue_id) REFERENCES issues(owner_id,project_id,id) ON DELETE CASCADE,
    UNIQUE(owner_id,command_id)
);
CREATE INDEX issue_events_project_history ON issue_events(owner_id,project_id,occurred_at,id);
CREATE INDEX issue_events_item_history ON issue_events(issue_id,occurred_at,id);
CREATE TABLE notes (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    project_id uuid NOT NULL,
    title text NOT NULL CHECK(length(trim(title)) BETWEEN 1 AND 500),
    path text NOT NULL CHECK(length(path) BETWEEN 1 AND 1024),
    body text NOT NULL DEFAULT '',
    revision bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY(owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE,
    UNIQUE(project_id,path), UNIQUE(owner_id,id), UNIQUE(project_id,id), UNIQUE(owner_id,project_id,id)
);
CREATE INDEX notes_search ON notes USING gin(to_tsvector('simple', title || ' ' || left(body, 32000)));
CREATE INDEX notes_recent ON notes(owner_id, project_id, updated_at DESC);
