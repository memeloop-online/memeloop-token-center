-- Keep next-request routing evidence outside the large partitioned request
-- history. Rows are append-only per request for one day: concurrent requests
-- in the same session never contend on one "latest" row, while the scoped
-- index still makes the newest terminal a bounded lookup. This migration never
-- builds an index across existing request partitions.
CREATE TABLE session_routing_terminals (
    tenant_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    explicit_session_id TEXT NOT NULL,
    model TEXT NOT NULL,
    protocol TEXT NOT NULL,
    request_id TEXT NOT NULL,
    observed_at BIGINT NOT NULL,
    status_code BIGINT NOT NULL,
    error_code TEXT,
    model_route_id TEXT,
    upstream_account_id TEXT,
    expires_at BIGINT NOT NULL,
    PRIMARY KEY (request_id)
);

CREATE INDEX session_routing_terminals_scope_idx
    ON session_routing_terminals (
        tenant_id,
        principal_id,
        key_id,
        explicit_session_id,
        model,
        protocol,
        observed_at DESC,
        request_id DESC
    );

CREATE INDEX session_routing_terminals_expiry_idx
    ON session_routing_terminals (expires_at, request_id);
