-- Recovery access is intentionally much more constrained than ordinary
-- control-plane reads. These durable buckets make the limit consistent across
-- replicas. The actor marker is either a service UUID or the fixed bootstrap
-- identity; neither table ever contains credential material.
CREATE TABLE key_credential_recovery_rate_limits (
    tenant_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    bucket_key TEXT NOT NULL,
    window_started_at BIGINT NOT NULL,
    attempts BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, actor_id, bucket_key)
);

-- This access audit is separate from the recovery-envelope lifecycle audit so
-- denied and rate-limited reads can be retained without weakening the latter's
-- compact action contract. Rate-limit rejections are coalesced by application
-- code after the first rejection in a window to keep abuse from growing this
-- table without bound.
CREATE TABLE key_credential_recovery_access_audit (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    actor_type TEXT NOT NULL CHECK (actor_type IN ('bootstrap', 'service')),
    actor_service_id TEXT,
    outcome TEXT NOT NULL CHECK (outcome IN ('retrieved', 'rate_limited', 'scope_denied', 'tenant_denied', 'inactive', 'unavailable', 'integrity_failed')),
    created_at BIGINT NOT NULL,
    CHECK ((actor_type = 'bootstrap' AND actor_service_id IS NULL) OR (actor_type = 'service' AND actor_service_id IS NOT NULL))
);

CREATE INDEX key_credential_recovery_access_actor_created_idx
    ON key_credential_recovery_access_audit
       (tenant_id, actor_type, actor_service_id, key_id, created_at DESC, id DESC);
