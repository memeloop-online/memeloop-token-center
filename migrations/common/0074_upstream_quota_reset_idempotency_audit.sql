-- The caller's raw Idempotency-Key and the confirmation token remain secret.
-- Only keyed hashes are retained for exact replay matching.
ALTER TABLE upstream_quota_reset_operations
    ADD COLUMN prepare_idempotency_hash TEXT NOT NULL DEFAULT '';
ALTER TABLE upstream_quota_reset_operations
    ADD COLUMN confirm_idempotency_hash TEXT;
ALTER TABLE upstream_quota_reset_operations
    ADD COLUMN confirmed_by_service_id TEXT;
ALTER TABLE upstream_quota_reset_operations
    ADD COLUMN confirmed_at BIGINT;

CREATE UNIQUE INDEX upstream_quota_reset_prepare_replay
    ON upstream_quota_reset_operations (
        tenant_id,
        upstream_account_id,
        actor_service_id,
        prepare_idempotency_hash
    )
    WHERE prepare_idempotency_hash <> '';

CREATE TABLE upstream_quota_reset_audit (
    id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    event TEXT NOT NULL CHECK (
        event IN (
            'prepared',
            'confirmation_claimed',
            'dispatch_accepted',
            'dispatch_unknown',
            'reconciled'
        )
    ),
    actor_service_id TEXT,
    error_code TEXT,
    created_at BIGINT NOT NULL
);

CREATE INDEX upstream_quota_reset_audit_operation
    ON upstream_quota_reset_audit (operation_id, created_at, id);
