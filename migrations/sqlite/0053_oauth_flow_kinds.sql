CREATE TABLE oauth_login_sessions_v2 (
    id TEXT PRIMARY KEY,
    flow_kind TEXT NOT NULL CHECK (flow_kind IN (
        'openai_codex_device',
        'cursor_pkce',
        'provider_adapter_cursor_pkce',
        'claude_manual_pkce',
        'github_copilot_device'
    )),
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

INSERT INTO oauth_login_sessions_v2 (
    id, flow_kind, tenant_external_id, operator_service_id, operator_is_bootstrap,
    state_ciphertext, ready_ciphertext, next_poll_at, expires_at, status,
    lease_owner, lease_expires_at, result_account_id, created_at, updated_at
)
SELECT
    id, flow_kind, tenant_external_id, operator_service_id, operator_is_bootstrap,
    state_ciphertext, ready_ciphertext, next_poll_at, expires_at, status,
    lease_owner, lease_expires_at, result_account_id, created_at, updated_at
FROM oauth_login_sessions;

DROP TABLE oauth_login_sessions;
ALTER TABLE oauth_login_sessions_v2 RENAME TO oauth_login_sessions;
CREATE INDEX oauth_login_sessions_expiry_idx
    ON oauth_login_sessions (expires_at, id);
CREATE INDEX oauth_login_sessions_poll_idx
    ON oauth_login_sessions (status, next_poll_at, lease_expires_at);
