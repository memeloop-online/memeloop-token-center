-- Ownership is independent of public model names: manual routes and multiple
-- candidates for one public model remain legal. A deleted route leaves a
-- tombstone so the next sync cannot undo an operator's deletion.
CREATE TABLE managed_model_routes (
    tenant_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    upstream_model TEXT NOT NULL,
    protocol TEXT NOT NULL,
    model_route_id TEXT UNIQUE,
    managed_updated_at BIGINT NOT NULL,
    disabled_reason TEXT,
    operator_override BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, upstream_account_id, upstream_model, protocol),
    FOREIGN KEY (tenant_id, upstream_account_id)
        REFERENCES upstream_accounts(tenant_id, id) ON DELETE CASCADE,
    -- Route deletion clears model_route_id explicitly in the same transaction,
    -- preserving a tombstone without weakening the tenant boundary.
    FOREIGN KEY (tenant_id, model_route_id) REFERENCES model_routes(tenant_id, id),
    CHECK (operator_override IN (0, 1)),
    CHECK (disabled_reason IS NULL OR disabled_reason = 'catalog_missing')
);
