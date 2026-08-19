CREATE TABLE IF NOT EXISTS plugin_configurations (
    plugin_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('global', 'tenant')),
    tenant_id TEXT REFERENCES tenants(id),
    value_json TEXT NOT NULL,
    schema_digest TEXT NOT NULL,
    version BIGINT NOT NULL CHECK (version > 0),
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (plugin_id, scope_key),
    CHECK (
        (scope_kind = 'global' AND scope_key = 'global' AND tenant_id IS NULL)
        OR
        (scope_kind = 'tenant' AND tenant_id IS NOT NULL AND scope_key = 'tenant:' || tenant_id)
    )
);

CREATE INDEX IF NOT EXISTS plugin_configurations_tenant_idx
    ON plugin_configurations (tenant_id, plugin_id)
    WHERE tenant_id IS NOT NULL;

CREATE TABLE IF NOT EXISTS plugin_configuration_operations (
    plugin_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    result_version BIGINT,
    result_value_json TEXT,
    result_schema_digest TEXT,
    result_updated_at BIGINT,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (plugin_id, scope_key, idempotency_key),
    CHECK (
        (result_version IS NULL AND result_value_json IS NULL AND result_schema_digest IS NULL AND result_updated_at IS NULL)
        OR
        (result_version > 0 AND result_value_json IS NOT NULL AND result_schema_digest IS NOT NULL AND result_updated_at IS NOT NULL)
    )
);
