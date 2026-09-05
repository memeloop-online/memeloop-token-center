-- Route creation has multiple association tables, so semantic equality alone
-- cannot prove a retrier owns an existing rule. A short-lived operation claim
-- binds an HMACed Idempotency-Key to one canonical request and stable route ID.
-- Raw keys are never persisted. The expiry index supports bounded, lazy
-- cleanup on later route creates without scanning high-volume request tables.
CREATE TABLE model_route_create_operations (
    tenant_id TEXT NOT NULL,
    idempotency_key_hash BYTEA NOT NULL,
    request_fingerprint TEXT NOT NULL,
    route_id TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, idempotency_key_hash),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    CHECK (LENGTH(idempotency_key_hash) = 32),
    CHECK (LENGTH(request_fingerprint) = 64),
    CHECK (created_at >= 0),
    CHECK (expires_at > created_at)
);
CREATE INDEX model_route_create_operations_expiry_idx
    ON model_route_create_operations (expires_at, tenant_id);
CREATE INDEX model_route_create_operations_route_expiry_idx
    ON model_route_create_operations (tenant_id, route_id, expires_at);
