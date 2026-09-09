-- A physical upstream-account deletion must not erase the stable identity
-- recorded by immutable request and generation history.  This compact
-- snapshot intentionally excludes configuration and all credential material.
CREATE TABLE deleted_upstream_account_snapshots (
    upstream_account_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    name TEXT NOT NULL,
    driver TEXT NOT NULL,
    auth_kind TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    deleted_at BIGINT NOT NULL,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE
);

CREATE INDEX deleted_upstream_account_snapshots_tenant_deleted_idx
    ON deleted_upstream_account_snapshots (tenant_id, deleted_at DESC, upstream_account_id);
