-- Derived links retain their authored target and stable destination separately.
CREATE TABLE note_links (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    source_id uuid NOT NULL,
    ordinal integer NOT NULL CHECK (ordinal >= 0),
    raw text NOT NULL,
    target text NOT NULL,
    fragment text,
    label text NOT NULL,
    kind text NOT NULL CHECK (kind IN ('wiki','markdown')),
    dest_note uuid,
    dest_issue uuid,
    manual boolean NOT NULL DEFAULT false,
    PRIMARY KEY (source_id, ordinal),
    CHECK (num_nonnulls(dest_note,dest_issue) <= 1),
    FOREIGN KEY (owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,source_id) REFERENCES notes(project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,dest_note) REFERENCES notes(project_id,id) ON DELETE SET NULL (dest_note),
    FOREIGN KEY (project_id,dest_issue) REFERENCES issues(project_id,id) ON DELETE SET NULL (dest_issue)
);
CREATE INDEX note_links_target_note ON note_links(owner_id,dest_note) WHERE dest_note IS NOT NULL;
CREATE INDEX note_links_target_issue ON note_links(owner_id,dest_issue) WHERE dest_issue IS NOT NULL;
CREATE INDEX note_links_unresolved ON note_links(owner_id,project_id) WHERE dest_note IS NULL AND dest_issue IS NULL;

CREATE TABLE note_issue_sources (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    note_id uuid NOT NULL,
    issue_id uuid PRIMARY KEY,
    source_anchor text NOT NULL,
    excerpt text NOT NULL,
    note_revision bigint NOT NULL CHECK (note_revision > 0),
    FOREIGN KEY (owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,note_id) REFERENCES notes(project_id,id) ON DELETE CASCADE,
    FOREIGN KEY (project_id,issue_id) REFERENCES issues(project_id,id) ON DELETE CASCADE
);
CREATE INDEX note_issue_sources_note ON note_issue_sources(owner_id,note_id);
