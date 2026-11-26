-- Exact supplier bearer rejection evidence for the existing OAuth worker.
-- One row per account; never store tokens, supplier payloads or URLs.
CREATE TABLE upstream_oauth_rejected_generations (
    upstream_account_id TEXT PRIMARY KEY REFERENCES upstream_accounts(id) ON DELETE CASCADE,
    credential_generation BIGINT NOT NULL,
    observed_at BIGINT NOT NULL
);
