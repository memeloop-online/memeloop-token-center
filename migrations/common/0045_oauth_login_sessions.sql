CREATE TABLE IF NOT EXISTS oauth_login_sessions (
    id TEXT PRIMARY KEY,
    flow_kind TEXT NOT NULL CHECK (flow_kind = 'openai_codex_device'),
    tenant_external_id TEXT NOT NULL,
    operator_service_id TEXT,
    operator_is_bootstrap BOOLEAN NOT NULL,
    state_ciphertext TEXT NOT NULL,
    ready_ciphertext TEXT,
    next_poll_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('pending', 'polling', 'ready', 'finalizing', 'consumed', 'failed')),
    lease_owner TEXT,
    lease_expires_at BIGINT,
    result_account_id TEXT,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    CHECK (
        (operator_is_bootstrap = TRUE AND operator_service_id IS NULL)
        OR (operator_is_bootstrap = FALSE AND operator_service_id IS NOT NULL)
    ),
    CHECK (
        (status IN ('polling', 'finalizing') AND lease_owner IS NOT NULL AND lease_expires_at IS NOT NULL)
        OR (status NOT IN ('polling', 'finalizing') AND lease_owner IS NULL AND lease_expires_at IS NULL)
    ),
    CHECK ((status IN ('ready', 'finalizing', 'consumed')) = (ready_ciphertext IS NOT NULL)),
    CHECK ((status = 'consumed') = (result_account_id IS NOT NULL)),
    FOREIGN KEY (result_account_id) REFERENCES upstream_accounts(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS oauth_login_sessions_expiry_idx
    ON oauth_login_sessions (expires_at, id);
CREATE INDEX IF NOT EXISTS oauth_login_sessions_poll_idx
    ON oauth_login_sessions (status, next_poll_at, lease_expires_at);

-- Historical subscription bridge rows remain queryable for audit and
-- migration, but the retired runtime must never select or contact them.
UPDATE model_routes
SET enabled = 0
WHERE upstream_account_id IN (
    SELECT id FROM upstream_accounts WHERE driver = 'cpa-subscription-bridge'
);

UPDATE upstream_accounts
SET status = 'disabled'
WHERE driver = 'cpa-subscription-bridge';
