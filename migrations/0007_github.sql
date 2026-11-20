CREATE TABLE github_oauth_states (
    owner_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    state_hash bytea NOT NULL,
    session_hash bytea NOT NULL,
    expires_at timestamptz NOT NULL,
    started boolean NOT NULL DEFAULT false
);

CREATE TABLE github_connections (
    owner_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    user_remote_id bigint NOT NULL CHECK (user_remote_id > 0),
    access_cipher bytea NOT NULL,
    refresh_cipher bytea,
    expires_at timestamptz,
    refresh_expires_at timestamptz,
    grant_id uuid NOT NULL DEFAULT gen_random_uuid()
);
CREATE INDEX github_connections_identity ON github_connections(user_remote_id);

CREATE TABLE github_repos (
    owner_id uuid NOT NULL REFERENCES github_connections(owner_id) ON DELETE CASCADE,
    repo_id bigint NOT NULL CHECK (repo_id > 0),
    id uuid NOT NULL UNIQUE DEFAULT gen_random_uuid(),
    installation_id bigint NOT NULL CHECK (installation_id > 0),
    project_id uuid NOT NULL,
    generation uuid NOT NULL DEFAULT gen_random_uuid(),
    display_name text NOT NULL DEFAULT '',
    billing_paused boolean NOT NULL DEFAULT false,
    repo_grant_until timestamptz,
    alert_grant_until timestamptz,
    last_success timestamptz,
    last_attempt timestamptz,
    next_sync_at timestamptz NOT NULL DEFAULT now(),
    lease_until timestamptz,
    lease_id uuid,
    failures integer NOT NULL DEFAULT 0,
    last_manual timestamptz,
    PRIMARY KEY(owner_id,repo_id),
    FOREIGN KEY(owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE
);
CREATE INDEX github_repos_due ON github_repos(next_sync_at) WHERE NOT billing_paused;
CREATE INDEX github_repos_installation ON github_repos(installation_id);

CREATE TABLE github_alerts (
    owner_id uuid NOT NULL,
    repo_id bigint NOT NULL,
    number bigint NOT NULL CHECK (number > 0),
    package text NOT NULL,
    manifest text NOT NULL,
    patched text,
    updated_at timestamptz NOT NULL,
    PRIMARY KEY(owner_id,repo_id,number),
    FOREIGN KEY(owner_id,repo_id) REFERENCES github_repos(owner_id,repo_id) ON DELETE CASCADE
);

-- Keep the original fix while alerts close, reopen, or move to another project.
-- The local issue is independently owned and survives disconnecting the source.
CREATE TABLE github_fixes (
    owner_id uuid NOT NULL,
    repo_id bigint NOT NULL,
    number bigint NOT NULL CHECK (number > 0),
    issue_id uuid NOT NULL UNIQUE,
    PRIMARY KEY(owner_id,repo_id,number),
    FOREIGN KEY(owner_id,repo_id) REFERENCES github_repos(owner_id,repo_id) ON DELETE CASCADE,
    FOREIGN KEY(owner_id,issue_id) REFERENCES issues(owner_id,id) ON DELETE CASCADE
);
CREATE TABLE github_pulls (
    owner_id uuid NOT NULL,
    repo_id bigint NOT NULL,
    number bigint NOT NULL CHECK (number > 0),
    issue_id uuid NOT NULL,
    linked_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY(owner_id,issue_id,repo_id,number),
    FOREIGN KEY(owner_id,repo_id) REFERENCES github_repos(owner_id,repo_id) ON DELETE CASCADE,
    FOREIGN KEY(owner_id,issue_id) REFERENCES issues(owner_id,id) ON DELETE CASCADE
);

CREATE TABLE github_deliveries (
    delivery uuid PRIMARY KEY,
    received_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX github_deliveries_received ON github_deliveries(received_at);
