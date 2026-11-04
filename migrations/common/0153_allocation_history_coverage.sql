CREATE TABLE group_allocation_history_state (
    tenant_id TEXT PRIMARY KEY,
    scan_from BIGINT NOT NULL,
    scan_to BIGINT NOT NULL,
    cursor_created BIGINT NOT NULL DEFAULT 0,
    cursor_key TEXT NOT NULL DEFAULT '',
    cursor_request TEXT NOT NULL DEFAULT '',
    cursor_source TEXT NOT NULL DEFAULT '',
    scanned BIGINT NOT NULL DEFAULT 0,
    ready BIGINT NOT NULL DEFAULT 0,
    capture_until BIGINT NOT NULL,
    lease TEXT NOT NULL DEFAULT '',
    lease_until BIGINT NOT NULL DEFAULT 0
);

-- A confirmed export contributes only compact routing counters.
CREATE TABLE group_allocation_history_receipts (
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    request_id TEXT NOT NULL,
    revision BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    account_id TEXT NOT NULL,
    requests BIGINT NOT NULL,
    tokens BIGINT NOT NULL,
    covered BIGINT NOT NULL,
    bootstrap BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (tenant_id, source, request_id, revision)
);
CREATE INDEX group_allocation_history_receipts_time
    ON group_allocation_history_receipts(tenant_id, created_at);

CREATE INDEX group_allocation_history_receipts_ready
    ON group_allocation_history_receipts(tenant_id, bootstrap DESC, source, request_id, revision);
CREATE INDEX group_allocation_history_receipts_expiry
    ON group_allocation_history_receipts(created_at);

CREATE TABLE group_allocation_history_contributions (
    tenant_id TEXT NOT NULL,
    source TEXT NOT NULL,
    request_id TEXT NOT NULL,
    revision BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    account_id TEXT NOT NULL,
    requests BIGINT NOT NULL,
    tokens BIGINT NOT NULL,
    covered BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, source, request_id)
);
CREATE INDEX group_allocation_history_contributions_time
    ON group_allocation_history_contributions(tenant_id, created_at);
CREATE INDEX group_allocation_history_contributions_expiry
    ON group_allocation_history_contributions(created_at);

CREATE TABLE group_allocation_history_hours (
    tenant_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    hour_bucket BIGINT NOT NULL,
    requests BIGINT NOT NULL,
    tokens BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, account_id, hour_bucket)
);
CREATE INDEX group_allocation_history_hours_expiry
    ON group_allocation_history_hours(hour_bucket);
