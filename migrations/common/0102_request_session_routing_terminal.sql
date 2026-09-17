-- Keep next-request routing evidence outside the large partitioned request
-- history. The table contains only the latest terminal per explicit routing
-- scope, so reads are one primary-key lookup and this migration never builds
-- an index across existing request partitions.
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
    PRIMARY KEY (
        tenant_id,
        principal_id,
        key_id,
        explicit_session_id,
        model,
        protocol
    )
);

CREATE INDEX session_routing_terminals_expiry_idx
    ON session_routing_terminals (expires_at, tenant_id, key_id);
