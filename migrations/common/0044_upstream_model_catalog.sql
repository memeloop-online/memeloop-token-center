CREATE TABLE upstream_model_catalog_snapshots (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    source_kind TEXT NOT NULL,
    fetched_at BIGINT NOT NULL,
    model_count BIGINT NOT NULL,
    UNIQUE (tenant_id, upstream_account_id, id),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, upstream_account_id) REFERENCES upstream_accounts(tenant_id, id) ON DELETE CASCADE
);

CREATE INDEX upstream_model_catalog_snapshots_account_idx
    ON upstream_model_catalog_snapshots (tenant_id, upstream_account_id, fetched_at DESC);

CREATE TABLE upstream_models (
    snapshot_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    protocol TEXT NOT NULL,
    context_window BIGINT,
    reservation_token_bound BIGINT,
    reservation_bound_source TEXT,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (snapshot_id, model_id, protocol),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, upstream_account_id) REFERENCES upstream_accounts(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, upstream_account_id, snapshot_id) REFERENCES upstream_model_catalog_snapshots(tenant_id, upstream_account_id, id) ON DELETE CASCADE
);

CREATE INDEX upstream_models_account_model_idx
    ON upstream_models (tenant_id, upstream_account_id, model_id, protocol);

CREATE TABLE upstream_model_catalog_state (
    upstream_account_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    current_snapshot_id TEXT,
    credential_generation BIGINT NOT NULL,
    status TEXT NOT NULL,
    last_attempt_at BIGINT NOT NULL,
    last_success_at BIGINT,
    expires_at BIGINT,
    last_error_code TEXT,
    sync_lease_id TEXT,
    sync_lease_expires_at BIGINT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, upstream_account_id) REFERENCES upstream_accounts(tenant_id, id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id, upstream_account_id, current_snapshot_id) REFERENCES upstream_model_catalog_snapshots(tenant_id, upstream_account_id, id) ON DELETE CASCADE
);

CREATE INDEX upstream_model_catalog_state_tenant_status_idx
    ON upstream_model_catalog_state (tenant_id, status, last_success_at DESC);
