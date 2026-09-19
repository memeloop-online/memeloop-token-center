CREATE TABLE IF NOT EXISTS plugin_service_data_snapshots (
    runtime_revision BIGINT NOT NULL,
    plugin_id TEXT NOT NULL,
    endpoint_id TEXT NOT NULL,
    endpoint_revision TEXT NOT NULL,
    data_json TEXT,
    source TEXT,
    origin TEXT,
    fetched_at BIGINT,
    last_attempt_at BIGINT NOT NULL DEFAULT 0,
    next_attempt_at BIGINT NOT NULL DEFAULT 0,
    consecutive_failures BIGINT NOT NULL DEFAULT 0,
    last_error_code TEXT,
    lease_owner TEXT,
    lease_until BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (runtime_revision, plugin_id, endpoint_id, endpoint_revision),
    CHECK (runtime_revision >= 0),
    CHECK (LENGTH(plugin_id) BETWEEN 1 AND 64),
    CHECK (LENGTH(endpoint_id) BETWEEN 1 AND 64),
    CHECK (
        LENGTH(endpoint_revision) = 64
        AND endpoint_revision = LOWER(endpoint_revision)
    ),
    CHECK (data_json IS NULL OR LENGTH(data_json) <= 1048576),
    CHECK (source IS NULL OR LENGTH(source) BETWEEN 1 AND 64),
    CHECK (origin IS NULL OR LENGTH(origin) BETWEEN 1 AND 2048),
    CHECK (
        (data_json IS NULL AND source IS NULL AND origin IS NULL AND fetched_at IS NULL)
        OR
        (data_json IS NOT NULL AND source IS NOT NULL AND origin IS NOT NULL AND fetched_at IS NOT NULL)
    ),
    CHECK (fetched_at IS NULL OR fetched_at >= 0),
    CHECK (last_attempt_at >= 0),
    CHECK (next_attempt_at >= 0),
    CHECK (consecutive_failures BETWEEN 0 AND 1000000),
    CHECK (
        last_error_code IS NULL
        OR last_error_code IN (
            'timeout',
            'network',
            'http_status',
            'content_type',
            'body_limit',
            'invalid_json',
            'schema_validation',
            'component_execution',
            'component_output',
            'database'
        )
    ),
    CHECK (
        (lease_owner IS NULL AND lease_until = 0)
        OR (LENGTH(lease_owner) BETWEEN 1 AND 128 AND lease_until > 0)
    )
);

CREATE INDEX IF NOT EXISTS plugin_service_data_snapshots_due
    ON plugin_service_data_snapshots (
        next_attempt_at,
        lease_until,
        runtime_revision,
        plugin_id,
        endpoint_id,
        endpoint_revision
    );
