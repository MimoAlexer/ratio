-- Only the local Luma fixture owns these two reserved IDs. The migration adds
-- the map to an existing sample once. Fresh demo installs reuse this statement.
WITH demo AS (
    SELECT p.id AS project_id, u.id AS owner_id, u.plan, u.content_bytes
    FROM users u JOIN projects p ON p.owner_id = u.id
    WHERE u.id = '173266c4-a3c1-4ccc-ac46-000000000001'
      AND u.email = 'demo@workspace.test'
      AND p.id = '173266c4-a3c1-4ccc-ac46-000000000002'
      AND p.name = 'Luma' AND p.key = 'LU' AND NOT p.archived
    FOR UPDATE OF u
),
shapes(slot, kind, label, body, x, y, w, h, source_kind, source_id) AS (
    VALUES
        ('1101', 'service',  'Vault archive', 'Notes and attachments',
            -416,   0, 184,  80, 'note',  '173266c4-a3c1-4ccc-ac46-00000000000c'::uuid),
        ('1102', 'decision', 'Safe paths?', 'Within vault root',
            -152, -16, 192, 112, NULL, NULL),
        ('1103', 'process',  'Index notes', 'Stable note IDs',
             136, -96, 184,  80, 'issue', '173266c4-a3c1-4ccc-ac46-000000000076'),
        ('1104', 'process',  'Reject import', 'Stop before writing',
             136, 112, 184,  80, 'issue', '173266c4-a3c1-4ccc-ac46-000000000086'),
        ('1105', 'database', 'Backlink index', 'Incoming note links',
             416, -96, 200,  80, 'issue', '173266c4-a3c1-4ccc-ac46-000000000084'),
        ('1106', 'note',     'Link resolution', 'Recheck after rename',
             416, 112, 200,  80, 'note',  '173266c4-a3c1-4ccc-ac46-00000000000b')
),
connections(slot, start, finish, label) AS (
    VALUES ('1201', '1101', '1102', ''), ('1202', '1102', '1103', 'Yes'),
           ('1203', '1102', '1104', 'No'), ('1204', '1103', '1105', ''),
           ('1205', '1106', '1105', 'Rules')
),
scene AS (
    SELECT demo.*, jsonb_build_object(
        'version', 1,
        'nodes', (SELECT jsonb_agg(jsonb_strip_nulls(jsonb_build_object(
            'id', '173266c4-a3c1-4ccc-ac46-00000000' || s.slot,
            'kind', s.kind, 'label', s.label, 'body', s.body,
            'x', s.x, 'y', s.y, 'w', s.w, 'h', s.h,
            'source', CASE WHEN i.id IS NOT NULL OR n.id IS NOT NULL
                      THEN jsonb_build_object('kind', s.source_kind, 'id', s.source_id) END
        )) ORDER BY s.slot)
        FROM shapes s
        LEFT JOIN issues i ON s.source_kind = 'issue'
            AND (i.owner_id, i.project_id, i.id) = (demo.owner_id, demo.project_id, s.source_id)
            AND NOT i.archived
        LEFT JOIN notes n ON s.source_kind = 'note'
            AND (n.owner_id, n.project_id, n.id) = (demo.owner_id, demo.project_id, s.source_id)),
        'edges', (SELECT jsonb_agg(jsonb_build_object(
            'id', '173266c4-a3c1-4ccc-ac46-00000000' || slot,
            'from', '173266c4-a3c1-4ccc-ac46-00000000' || start,
            'to', '173266c4-a3c1-4ccc-ac46-00000000' || finish,
            'label', label
        ) ORDER BY slot) FROM connections),
        'strokes', '[]'::jsonb
    ) AS document
    FROM demo
)
INSERT INTO canvas_boards (id, owner_id, project_id, title, document)
SELECT '173266c4-a3c1-4ccc-ac46-000000001000', owner_id, project_id, 'Vault indexing', document
FROM scene
WHERE content_bytes + octet_length('Vault indexing') + octet_length(document::text) <=
      CASE plan WHEN 'max' THEN 5000000000::bigint WHEN 'plus' THEN 1000000000::bigint
                ELSE 100000000::bigint END
ON CONFLICT (id) DO NOTHING;
