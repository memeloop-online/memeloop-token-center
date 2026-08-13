CREATE TABLE IF NOT EXISTS legacy_key_credentials (
    id TEXT PRIMARY KEY,
    key_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    secret_hash BYTEA NOT NULL UNIQUE,
    fingerprint TEXT NOT NULL,
    source_hash TEXT NOT NULL UNIQUE,
    created_at BIGINT NOT NULL,
    revoked_at BIGINT
);
CREATE INDEX IF NOT EXISTS legacy_key_credentials_key_active_idx
    ON legacy_key_credentials (key_id, generation DESC)
    WHERE revoked_at IS NULL;
