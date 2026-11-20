-- Record a successful native restore without retaining another copy of private history.
CREATE TABLE transfer_history_receipts (
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    source_project uuid NOT NULL,
    record_type text NOT NULL,
    source_key text NOT NULL,
    source_hash text NOT NULL,
    local_hash text NOT NULL,
    PRIMARY KEY (owner_id,project_id,source_project,record_type,source_key),
    FOREIGN KEY (owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE
);
