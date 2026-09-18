-- Missing models are availability evidence, not an operator routing edit.
-- Keep this separately from the active snapshot so existing routing joins
-- cannot accidentally admit a removed model.
CREATE TABLE upstream_model_catalog_disabled (
    tenant_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    protocol TEXT NOT NULL,
    disabled_at BIGINT NOT NULL,
    PRIMARY KEY (upstream_account_id, model_id, protocol),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, upstream_account_id) REFERENCES upstream_accounts(tenant_id, id) ON DELETE CASCADE
);
