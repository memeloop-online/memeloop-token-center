CREATE TABLE group_allocation_revisions (
    tenant_id TEXT PRIMARY KEY,
    revision BIGINT NOT NULL,
    refreshed_at BIGINT NOT NULL DEFAULT 0
);
CREATE TABLE group_allocation_usage (
    tenant_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    hour_bucket BIGINT NOT NULL,
    requests BIGINT NOT NULL,
    tokens BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, account_id, hour_bucket)
);
CREATE TABLE group_allocation_sessions (
    tenant_id TEXT NOT NULL,
    session_key TEXT NOT NULL,
    request_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    observed_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, session_key)
);
CREATE INDEX group_allocation_sessions_expiry ON group_allocation_sessions(expires_at);
CREATE TABLE group_allocation_reservations (
    tenant_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    session_key TEXT NOT NULL,
    generation BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, request_id)
);
CREATE INDEX group_allocation_reservations_account ON group_allocation_reservations(tenant_id, account_id, created_at);
CREATE INDEX group_allocation_reservations_request ON group_allocation_reservations(request_id);
