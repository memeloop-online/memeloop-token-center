CREATE TABLE IF NOT EXISTS request_events (
    event_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    event_at BIGINT NOT NULL,
    event_kind TEXT NOT NULL,
    protocol TEXT NOT NULL,
    model TEXT NOT NULL,
    status_code BIGINT,
    duration_ms BIGINT,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    cost_micros BIGINT NOT NULL,
    error_code TEXT
);
CREATE INDEX IF NOT EXISTS request_events_tenant_cursor_idx
    ON request_events (tenant_id, event_at ASC, event_id ASC);
CREATE INDEX IF NOT EXISTS request_events_request_time_idx
    ON request_events (request_id, event_at ASC);

