CREATE TABLE IF NOT EXISTS service_principals (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL,
    credential_generation BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL
);
CREATE TABLE IF NOT EXISTS service_credentials (
    id TEXT PRIMARY KEY,
    service_principal_id TEXT NOT NULL,
    generation BIGINT NOT NULL,
    secret_hash BYTEA NOT NULL,
    fingerprint TEXT NOT NULL,
    scopes_json TEXT NOT NULL,
    tenant_external_id TEXT,
    created_at BIGINT NOT NULL,
    revoked_at BIGINT,
    UNIQUE(service_principal_id, generation)
);
CREATE INDEX IF NOT EXISTS service_credentials_active_idx
    ON service_credentials (service_principal_id, generation DESC)
    WHERE revoked_at IS NULL;
