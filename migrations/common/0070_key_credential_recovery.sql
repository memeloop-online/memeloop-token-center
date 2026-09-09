-- Durable, separately-addressed recovery envelopes let an authorized control
-- client re-copy a still-active credential without changing its key identity
-- or generation. The envelope is authenticated by application code against
-- its key and generation before it is returned; this table never replaces the
-- one-way authentication hash in key_credentials.
CREATE TABLE IF NOT EXISTS key_credential_recovery_secrets (
    credential_id TEXT PRIMARY KEY REFERENCES key_credentials(id) ON DELETE CASCADE,
    key_id TEXT NOT NULL REFERENCES key_records(id) ON DELETE CASCADE,
    credential_generation BIGINT NOT NULL,
    ciphertext TEXT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (key_id, credential_generation)
);

CREATE INDEX IF NOT EXISTS key_credential_recovery_secrets_key_idx
    ON key_credential_recovery_secrets (key_id, credential_generation);

-- Recovery activity is durable and intentionally contains only stable IDs,
-- actor identity, action, and time. Secret plaintext, HMACs, fingerprints,
-- provenance digests, and ciphertext are never audit columns.
CREATE TABLE IF NOT EXISTS key_credential_recovery_audit (
    id TEXT PRIMARY KEY,
    key_id TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('stored', 'retrieved', 'removed')),
    actor_service_id TEXT,
    created_at BIGINT NOT NULL
);

CREATE INDEX IF NOT EXISTS key_credential_recovery_audit_key_created_idx
    ON key_credential_recovery_audit (key_id, created_at DESC, id DESC);
