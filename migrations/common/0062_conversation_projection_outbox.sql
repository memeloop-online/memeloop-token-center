-- Terminal metered-unlimited requests must not contend on the mutable
-- conversation cluster projections. Keep the complete observation payload in
-- an insert-only durable outbox and let the worker materialize it later.
CREATE TABLE IF NOT EXISTS conversation_projection_outbox (
    request_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    request_json TEXT NOT NULL,
    hints_json TEXT NOT NULL,
    client_name TEXT,
    upstream_response_id TEXT,
    observed_at BIGINT NOT NULL,
    lease_owner TEXT,
    lease_expires_at BIGINT,
    attempts BIGINT NOT NULL DEFAULT 0,
    projected_at BIGINT
);

CREATE INDEX IF NOT EXISTS conversation_projection_outbox_pending_idx
    ON conversation_projection_outbox (
        projected_at,
        lease_expires_at,
        observed_at,
        request_id
    );
