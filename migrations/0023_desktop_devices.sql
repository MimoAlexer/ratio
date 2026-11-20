-- The desktop holds a random secret. Only its hash and the hash of its
-- separately derived display code are stored. Deleting the approving browser
-- session invalidates its unfinished grants, including after account recovery.
CREATE TABLE desktop_devices (
    device_hash bytea PRIMARY KEY CHECK (octet_length(device_hash) = 32),
    code_hash bytea NOT NULL UNIQUE CHECK (octet_length(code_hash) = 32),
    approved_by bytea REFERENCES user_sessions(token_hash) ON DELETE CASCADE,
    expires_at timestamptz NOT NULL DEFAULT clock_timestamp() + interval '10 minutes'
);
CREATE INDEX desktop_devices_expiry ON desktop_devices(expires_at);
CREATE INDEX desktop_devices_approver ON desktop_devices(approved_by) WHERE approved_by IS NOT NULL;
