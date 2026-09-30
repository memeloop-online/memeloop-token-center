CREATE TABLE ledger_resource_route_switches (
    operation_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    route_id TEXT NOT NULL,
    source_upstream_account_id TEXT NOT NULL,
    target_upstream_account_id TEXT NOT NULL,
    expected_route_updated_at BIGINT NOT NULL,
    expected_source_updated_at BIGINT NOT NULL,
    expected_target_updated_at BIGINT NOT NULL,
    expected_grant_revision BIGINT NOT NULL,
    before_snapshot_json TEXT NOT NULL,
    after_snapshot_json TEXT NOT NULL,
    status TEXT NOT NULL,
    actor_service_id TEXT,
    created_at BIGINT NOT NULL,
    applied_at BIGINT,
    rolled_back_at BIGINT,
    rollback_updated_at BIGINT,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE RESTRICT,
    CHECK (source_upstream_account_id <> target_upstream_account_id),
    CHECK (expected_route_updated_at >= 0),
    CHECK (expected_source_updated_at >= 0),
    CHECK (expected_target_updated_at >= 0),
    CHECK (expected_grant_revision >= 0),
    CHECK (status IN ('planned', 'applied', 'rolled_back'))
);

CREATE INDEX ledger_resource_route_switches_tenant_created_idx
    ON ledger_resource_route_switches (tenant_id, created_at DESC, operation_id);
