-- Provisional migration slot: renumber after the pending schema stack merges.
-- Retain stable route identities for request and generation history.
ALTER TABLE model_routes ADD COLUMN archived_at BIGINT
    CHECK (archived_at IS NULL OR enabled = 0);
CREATE INDEX model_routes_visible_tenant_cursor
    ON model_routes (tenant_id, created_at DESC, id DESC)
    WHERE archived_at IS NULL;
