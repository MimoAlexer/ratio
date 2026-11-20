CREATE TABLE billing_accounts (
    owner_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    customer_id text UNIQUE,
    provider_live boolean NOT NULL DEFAULT false,
    subscription_id text UNIQUE,
    provider_status text NOT NULL DEFAULT 'free',
    paid_until timestamptz,
    grace_until timestamptz,
    scheduled_plan text CHECK (scheduled_plan IN ('free','plus','max')),
    plan_changes_at timestamptz,
    pending_plan text CHECK (pending_plan IN ('plus','max')),
    pending_key uuid,
    pending_started_at timestamptz,
    pending_expires_at timestamptz,
    pending_session text UNIQUE,
    pending_url text,
    entitlement_version smallint NOT NULL DEFAULT 1,
    reconciled_at timestamptz
);

CREATE TABLE billing_events (
    event_id text PRIMARY KEY,
    owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    processed_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX billing_events_owner ON billing_events(owner_id);

-- Preferences can reference either class of resource. Selection and application
-- both join on the authenticated owner, including after resources are removed.
CREATE TABLE billing_preferences (
    owner_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    resource_type text NOT NULL CHECK (resource_type IN ('project','repo')),
    resource_id uuid NOT NULL,
    PRIMARY KEY(owner_id, resource_type, resource_id)
);

-- This minimal outbox survives deletion of the workspace. Successful closure
-- is checked again after all formerly open Checkout sessions have expired.
CREATE TABLE billing_shutdowns (
    owner_id uuid PRIMARY KEY,
    customer_id text NOT NULL UNIQUE,
    provider_live boolean NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    attempts integer NOT NULL DEFAULT 0
);
CREATE INDEX billing_shutdown_due ON billing_shutdowns(next_attempt_at);

CREATE FUNCTION clear_billing_preference() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM billing_preferences WHERE owner_id=OLD.owner_id
      AND resource_type=TG_ARGV[0] AND resource_id=OLD.id;
    RETURN OLD;
END
$$;
CREATE TRIGGER billing_project_removed AFTER DELETE ON projects
    FOR EACH ROW EXECUTE FUNCTION clear_billing_preference('project');
CREATE TRIGGER billing_repo_removed AFTER DELETE ON github_repos
    FOR EACH ROW EXECUTE FUNCTION clear_billing_preference('repo');
