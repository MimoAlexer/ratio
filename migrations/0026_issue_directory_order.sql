-- Matches the default active issue list's status groups, update order, and UUID tie-break.
CREATE INDEX issue_active_directory_order
    ON issues (
        owner_id,
        (CASE status WHEN 'in_progress' THEN 0 WHEN 'ready' THEN 1
                     WHEN 'backlog' THEN 2 WHEN 'done' THEN 3 ELSE 4 END),
        updated_at DESC,
        id
    ) INCLUDE (project_id)
    WHERE NOT archived;
