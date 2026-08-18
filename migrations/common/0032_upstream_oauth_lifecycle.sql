ALTER TABLE upstream_accounts ADD COLUMN oauth_driver TEXT;
ALTER TABLE upstream_accounts ADD COLUMN oauth_refresh_url TEXT;

CREATE TABLE IF NOT EXISTS upstream_oauth_refresh_leases (
    account_id TEXT PRIMARY KEY,
    credential_generation BIGINT NOT NULL,
    idempotency_key TEXT NOT NULL UNIQUE,
    pending_credential_ciphertext TEXT,
    pending_expires_at BIGINT,
    lease_expires_at BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    FOREIGN KEY (account_id) REFERENCES upstream_accounts(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS upstream_oauth_refresh_leases_expiry_idx
    ON upstream_oauth_refresh_leases (lease_expires_at);
