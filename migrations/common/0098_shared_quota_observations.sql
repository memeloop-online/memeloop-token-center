CREATE TABLE upstream_quota_observations (
    upstream_account_id TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    tenant_id TEXT NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    credential_generation BIGINT NOT NULL,
    config_revision BIGINT NOT NULL,
    observation_json TEXT,
    valid_until BIGINT NOT NULL DEFAULT 0,
    last_attempt_at BIGINT NOT NULL DEFAULT 0,
    next_refresh_at BIGINT NOT NULL DEFAULT 0,
    lease_id TEXT,
    lease_until BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX upstream_quota_observations_tenant ON upstream_quota_observations(tenant_id, valid_until);
