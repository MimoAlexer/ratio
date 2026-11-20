ALTER TABLE workflow_views
    ADD COLUMN sort_order text NOT NULL DEFAULT 'updated'
        CHECK (sort_order IN ('updated', 'priority', 'newest', 'title')),
    ADD COLUMN grouping text NOT NULL DEFAULT 'status'
        CHECK (grouping IN ('status', 'priority', 'none')),
    ADD CONSTRAINT workflow_views_board_groups CHECK (layout <> 'board' OR grouping <> 'none');
