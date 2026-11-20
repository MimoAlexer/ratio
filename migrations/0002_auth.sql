ALTER TABLE users ADD COLUMN timezone text NOT NULL DEFAULT 'UTC';

CREATE TABLE login_challenges (
    token_hash bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    email text NOT NULL,
    return_to text NOT NULL DEFAULT '/',
    expires_at timestamptz NOT NULL DEFAULT now() + interval '15 minutes'
);
CREATE INDEX login_challenges_email ON login_challenges(email);
CREATE INDEX login_challenges_expiry ON login_challenges(expires_at);

CREATE TABLE user_sessions (
    token_hash bytea PRIMARY KEY CHECK (octet_length(token_hash) = 32),
    user_id uuid NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at timestamptz NOT NULL DEFAULT now(),
    expires_at timestamptz NOT NULL DEFAULT now() + interval '30 days'
);
CREATE INDEX user_sessions_owner ON user_sessions(user_id);
CREATE INDEX user_sessions_expiry ON user_sessions(expires_at);

CREATE TABLE login_rate_limits (
    bucket bytea PRIMARY KEY,
    started_at timestamptz NOT NULL DEFAULT now(),
    hits integer NOT NULL CHECK (hits > 0)
);
CREATE INDEX login_rate_expiry ON login_rate_limits(started_at);

-- Entries contain no email, content or authentication material. Operators can
-- retain an independent copy for applying deletions after restoring a backup.
CREATE TABLE account_deletions (
    user_id uuid PRIMARY KEY,
    deleted_at timestamptz NOT NULL DEFAULT now()
);
