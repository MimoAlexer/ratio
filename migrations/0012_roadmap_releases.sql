CREATE TABLE milestone_releases (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    milestone_id uuid NOT NULL,
    release_id uuid NOT NULL,
    PRIMARY KEY (owner_id,project_id,milestone_id),
    FOREIGN KEY (owner_id,project_id,milestone_id) REFERENCES milestones(owner_id,project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (owner_id,project_id,release_id) REFERENCES releases(owner_id,project_id,id) ON DELETE CASCADE
);
CREATE INDEX milestone_releases_by_release ON milestone_releases(owner_id,project_id,release_id);
CREATE TRIGGER invalidate_import AFTER INSERT OR UPDATE OR DELETE ON milestone_releases
    FOR EACH ROW EXECUTE FUNCTION invalidate_transfer_preview();
