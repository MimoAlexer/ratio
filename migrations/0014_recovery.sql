-- Each recovery code and its current verification link belong to one owner.
-- Keeping the challenge on this row also invalidates it on code rotation.
CREATE TABLE account_recovery (
    user_id uuid PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    code_hash bytea NOT NULL UNIQUE CHECK (octet_length(code_hash) = 32),
    challenge_hash bytea UNIQUE CHECK (octet_length(challenge_hash) = 32),
    replacement_email text,
    expires_at timestamptz,
    CHECK ((challenge_hash IS NULL) = (replacement_email IS NULL)),
    CHECK ((challenge_hash IS NULL) = (expires_at IS NULL))
);
