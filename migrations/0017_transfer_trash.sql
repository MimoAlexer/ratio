ALTER TABLE transfer_blobs ADD COLUMN trashed_at timestamptz;
CREATE INDEX transfer_blobs_trash_expiry ON transfer_blobs(trashed_at,owner_id,project_id)
    WHERE trashed_at IS NOT NULL;
CREATE INDEX transfer_blobs_trash_list ON transfer_blobs(owner_id,trashed_at DESC,id)
    WHERE trashed_at IS NOT NULL;

-- Remove all import identities for a purged image. Restores keep the original row,
-- so its download ID and associations survive the entire recovery window.
CREATE FUNCTION cleanup_purged_blob_identity() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    DELETE FROM transfer_records WHERE owner_id=OLD.owner_id AND project_id=OLD.project_id
        AND kind='blob' AND local_id=OLD.id;
    RETURN OLD;
END $$;
CREATE TRIGGER cleanup_blob_identity AFTER DELETE ON transfer_blobs
    FOR EACH ROW EXECUTE FUNCTION cleanup_purged_blob_identity();
