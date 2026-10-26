-- Execution evidence is independent of the financial terminal CAS.
CREATE TABLE proxy_operational_terminals (
    request_id TEXT PRIMARY KEY,
    request_created_at BIGINT NOT NULL,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    reservation_id TEXT NOT NULL UNIQUE,
    owner_token TEXT NOT NULL,
    fence TEXT NOT NULL,
    contract_digest TEXT NOT NULL,
    receipt_id TEXT NOT NULL,
    operational_digest TEXT NOT NULL,
    status_code BIGINT NOT NULL CHECK (status_code BETWEEN 200 AND 599),
    error_code TEXT,
    terminal_cause_code TEXT,
    stage TEXT,
    completed_at BIGINT NOT NULL,
    duration_ms BIGINT NOT NULL CHECK (duration_ms >= 0),
    first_output_ms BIGINT,
    generation_duration_ms BIGINT,
    response_object TEXT NOT NULL,
    upstream_account_id TEXT,
    model_route_id TEXT,
    recorded_at BIGINT NOT NULL,
    CHECK (completed_at >= request_created_at)
);
CREATE INDEX proxy_operational_terminals_key_time_idx
    ON proxy_operational_terminals (tenant_id, key_id, request_created_at, request_id);
CREATE INDEX proxy_operational_terminals_time_idx
    ON proxy_operational_terminals (request_created_at, tenant_id, key_id, request_id);
