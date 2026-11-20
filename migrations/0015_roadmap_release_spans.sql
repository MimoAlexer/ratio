CREATE TABLE roadmap_release_spans (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    release_id uuid NOT NULL,
    start_on date,
    end_on date,
    revision bigint NOT NULL DEFAULT 1 CHECK (revision > 0),
    PRIMARY KEY (owner_id,project_id,release_id),
    CHECK ((start_on IS NULL) = (end_on IS NULL)),
    CHECK (start_on <= end_on),
    FOREIGN KEY (owner_id,project_id,release_id) REFERENCES releases(owner_id,project_id,id) ON DELETE CASCADE
);
CREATE TRIGGER invalidate_import AFTER INSERT OR UPDATE OR DELETE ON roadmap_release_spans
    FOR EACH ROW EXECUTE FUNCTION invalidate_transfer_preview();
