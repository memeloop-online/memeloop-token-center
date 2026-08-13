CREATE TABLE IF NOT EXISTS credential_rotation_replays (
    idempotency_key TEXT PRIMARY KEY,
    resource_kind TEXT NOT NULL,
    resource_id TEXT NOT NULL,
    request_hash TEXT NOT NULL,
    response_ciphertext TEXT,
    expires_at BIGINT NOT NULL,
    created_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS credential_rotation_replays_resource_idx
    ON credential_rotation_replays (resource_kind, resource_id, created_at DESC);

CREATE INDEX IF NOT EXISTS credential_rotation_replays_expiry_idx
    ON credential_rotation_replays (expires_at)
    WHERE response_ciphertext IS NOT NULL;
