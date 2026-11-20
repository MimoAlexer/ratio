CREATE TRIGGER clear_digest_email_for_unverified_identity AFTER UPDATE OF identity_current ON users
    FOR EACH ROW WHEN (OLD.identity_current AND NOT NEW.identity_current)
    EXECUTE FUNCTION clear_changed_digest_recipient();
