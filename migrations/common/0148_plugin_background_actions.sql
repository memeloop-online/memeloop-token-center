-- Explicit account-scoped authority; a manifest or guest config cannot grant it.
CREATE TABLE plugin_background_action_grants (
    plugin_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    enabled BIGINT NOT NULL DEFAULT 0 CHECK (enabled IN (0, 1)),
    revision BIGINT NOT NULL DEFAULT 0,
    actor_service_id TEXT NOT NULL,
    actor_generation BIGINT NOT NULL,
    bootstrap_authority_hash TEXT NOT NULL DEFAULT '',
    next_attempt_at BIGINT NOT NULL DEFAULT 0,
    lease_owner TEXT,
    lease_until BIGINT NOT NULL DEFAULT 0,
    last_attempt_at BIGINT,
    last_error_code TEXT,
    last_result TEXT,
    last_operation_id TEXT,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (plugin_id, tenant_id, upstream_account_id)
);
CREATE INDEX plugin_background_actions_due
    ON plugin_background_action_grants (enabled, next_attempt_at, plugin_id, upstream_account_id);
