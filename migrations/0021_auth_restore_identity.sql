-- A restore can predate a verified change of sign-in address. The restore
-- runner compares these opaque markers to the independently held live ledger
-- and suspends login for affected workspaces until their identity is reconciled.
ALTER TABLE users ADD COLUMN identity_current boolean NOT NULL DEFAULT true;

CREATE TABLE auth_recovery_events (
    user_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    event_id uuid NOT NULL DEFAULT gen_random_uuid(),
    recovered_at timestamptz NOT NULL DEFAULT now()
);
