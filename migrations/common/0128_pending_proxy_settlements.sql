CREATE TABLE pending_proxy_settlements (
    request_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    account_id TEXT NOT NULL,
    reservation_id TEXT NOT NULL UNIQUE,
    owner_token TEXT NOT NULL,
    fence TEXT NOT NULL,
    contract_digest TEXT NOT NULL,
    contract_json TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'applied')),
    unknown_reason TEXT,
    receipt_id TEXT,
    receipt_digest TEXT,
    cost_micros BIGINT,
    status_code BIGINT,
    error_code TEXT,
    response_object TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    applied_at BIGINT,
    CHECK (
        (state = 'pending' AND receipt_id IS NULL AND receipt_digest IS NULL AND applied_at IS NULL)
        OR
        (state = 'applied' AND receipt_id IS NOT NULL AND receipt_digest IS NOT NULL
         AND cost_micros IS NOT NULL AND status_code IS NOT NULL
         AND response_object IS NOT NULL AND applied_at IS NOT NULL)
    )
);
CREATE INDEX pending_proxy_settlements_recovery_idx
    ON pending_proxy_settlements (state, updated_at, request_id);
