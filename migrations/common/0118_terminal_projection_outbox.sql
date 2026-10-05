CREATE TABLE terminal_projection_outbox (
    request_id TEXT PRIMARY KEY,
    reservation_id TEXT NOT NULL UNIQUE,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    completed_at BIGINT NOT NULL,
    model TEXT NOT NULL,
    protocol TEXT NOT NULL,
    status_code BIGINT NOT NULL,
    error_code TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    duration_ms BIGINT NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cached_input_tokens BIGINT NOT NULL,
    cache_write_tokens BIGINT NOT NULL,
    generation_units BIGINT NOT NULL,
    billing_unit TEXT NOT NULL,
    service_tier TEXT NOT NULL,
    currency TEXT NOT NULL,
    cost_micros BIGINT NOT NULL,
    usage_basis TEXT NOT NULL,
    session_id TEXT NOT NULL,
    terminal_cause_code TEXT,
    lease_owner TEXT,
    lease_expires_at BIGINT,
    attempts BIGINT NOT NULL DEFAULT 0,
    projected_at BIGINT,
    statistics_outcome TEXT CHECK (statistics_outcome IN ('applied', 'pruned'))
);
CREATE INDEX terminal_projection_pending_idx
    ON terminal_projection_outbox (projected_at, lease_expires_at, completed_at, request_id);

CREATE TABLE observability_prune_boundaries (
    scope TEXT PRIMARY KEY CHECK (scope = 'global'),
    before_day BIGINT NOT NULL,
    recorded_at BIGINT NOT NULL
);

ALTER TABLE conversation_projection_outbox ADD COLUMN key_snapshot_json TEXT;
ALTER TABLE conversation_projection_outbox ADD COLUMN statistics_outcome TEXT;
