ALTER TABLE portfolio_digests
    ADD COLUMN id uuid NOT NULL DEFAULT gen_random_uuid() UNIQUE,
    ADD COLUMN timezone text NOT NULL DEFAULT 'UTC';
ALTER TABLE portfolio_digest_schedules
    ADD COLUMN email_enabled boolean NOT NULL DEFAULT false,
    ADD CHECK (enabled OR NOT email_enabled);

CREATE TABLE portfolio_digest_email_outbox (
    owner_id uuid NOT NULL,
    week_start date NOT NULL,
    status text NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'sent', 'canceled', 'failed')),
    attempts integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    first_attempt_at timestamptz,
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    lease_id uuid,
    sent_at timestamptz,
    PRIMARY KEY (owner_id, week_start),
    FOREIGN KEY (owner_id, week_start)
        REFERENCES portfolio_digests (owner_id, week_start) ON DELETE CASCADE
);
CREATE INDEX portfolio_digest_email_due ON portfolio_digest_email_outbox
    (next_attempt_at, owner_id, week_start) WHERE status='pending';

CREATE FUNCTION clear_paused_digest_email() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF NOT NEW.enabled THEN NEW.email_enabled := false; END IF;
    RETURN NEW;
END
$$;
CREATE TRIGGER clear_paused_digest_email BEFORE INSERT OR UPDATE OF enabled, email_enabled
    ON portfolio_digest_schedules FOR EACH ROW EXECUTE FUNCTION clear_paused_digest_email();

CREATE FUNCTION cancel_pending_digest_emails() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE portfolio_digest_email_outbox SET status='canceled', lease_id=NULL
        WHERE owner_id=NEW.owner_id AND status='pending';
    RETURN NEW;
END
$$;
CREATE TRIGGER cancel_pending_digest_emails AFTER UPDATE ON portfolio_digest_schedules
    FOR EACH ROW WHEN (OLD.email_enabled AND NOT NEW.email_enabled)
    EXECUTE FUNCTION cancel_pending_digest_emails();

CREATE FUNCTION clear_changed_digest_recipient() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    UPDATE portfolio_digest_schedules SET email_enabled=false
        WHERE owner_id=NEW.id AND email_enabled;
    RETURN NEW;
END
$$;
CREATE TRIGGER clear_changed_digest_recipient AFTER UPDATE OF email ON users
    FOR EACH ROW WHEN (OLD.email IS DISTINCT FROM NEW.email)
    EXECUTE FUNCTION clear_changed_digest_recipient();
