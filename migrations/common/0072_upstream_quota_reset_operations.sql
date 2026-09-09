-- One durable dispatch claim, not a claim of supplier-side idempotency.
-- No plaintext confirmation token, OAuth secret or supplier body is stored.
CREATE TABLE upstream_quota_reset_operations (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    transport_updated_at BIGINT NOT NULL,
    actor_service_id TEXT NOT NULL,
    confirmation_hash TEXT NOT NULL,
    redeem_request_id TEXT NOT NULL UNIQUE,
    state TEXT NOT NULL CHECK (state IN ('prepared', 'submitted', 'accepted', 'unknown', 'expired')),
    available_credits BIGINT NOT NULL,
    applicable_credits BIGINT NOT NULL,
    observed_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    last_reconciled_at BIGINT,
    reconciled_available_credits BIGINT,
    reconciled_applicable_credits BIGINT,
    error_code TEXT
);
CREATE UNIQUE INDEX upstream_quota_reset_one_active
    ON upstream_quota_reset_operations (upstream_account_id)
    WHERE state IN ('prepared', 'submitted', 'accepted', 'unknown');
CREATE INDEX upstream_quota_reset_tenant_history
    ON upstream_quota_reset_operations (tenant_id, upstream_account_id, created_at DESC);
