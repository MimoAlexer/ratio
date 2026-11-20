CREATE TABLE canvas_boards (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    title text NOT NULL CHECK (length(btrim(title)) BETWEEN 1 AND 120),
    document jsonb NOT NULL CHECK (jsonb_typeof(document) = 'object' AND octet_length(document::text) <= 6291456),
    revision bigint NOT NULL DEFAULT 1 CHECK (revision >= 1),
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE (owner_id,project_id,id),
    FOREIGN KEY (owner_id,project_id) REFERENCES projects(owner_id,id) ON DELETE CASCADE
);
CREATE INDEX canvas_project_boards ON canvas_boards(owner_id,project_id,updated_at DESC,id);
CREATE TABLE canvas_checkpoints (
    id uuid PRIMARY KEY DEFAULT gen_random_uuid(),
    owner_id uuid NOT NULL,
    project_id uuid NOT NULL,
    board_id uuid NOT NULL,
    name text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 80),
    title text NOT NULL CHECK (length(btrim(title)) BETWEEN 1 AND 120),
    document jsonb NOT NULL CHECK (jsonb_typeof(document) = 'object' AND octet_length(document::text) <= 6291456),
    automatic boolean NOT NULL DEFAULT false,
    created_at timestamptz NOT NULL DEFAULT now(),
    FOREIGN KEY (owner_id,project_id,board_id) REFERENCES canvas_boards(owner_id,project_id,id) ON DELETE CASCADE
);
CREATE INDEX canvas_board_checkpoints ON canvas_checkpoints(owner_id,project_id,board_id,created_at DESC,id);

DO $$
DECLARE
    relation text;
BEGIN
    FOREACH relation IN ARRAY ARRAY['canvas_boards','canvas_checkpoints'] LOOP
        EXECUTE format('CREATE TRIGGER workspace_device_insert AFTER INSERT ON %I REFERENCING NEW TABLE AS added
                        FOR EACH STATEMENT EXECUTE FUNCTION record_workspace_device_change()', relation);
        EXECUTE format('CREATE TRIGGER workspace_device_edit AFTER UPDATE ON %I REFERENCING OLD TABLE AS removed NEW TABLE AS added
                        FOR EACH STATEMENT EXECUTE FUNCTION record_workspace_device_change(''id'')', relation);
        EXECUTE format('CREATE TRIGGER workspace_device_delete AFTER DELETE ON %I REFERENCING OLD TABLE AS removed
                        FOR EACH STATEMENT EXECUTE FUNCTION record_workspace_device_change()', relation);
    END LOOP;
END $$;
