-- Native single-account provenance is independent of the historical Kimi cohort.
-- Subject hashes identify a source-declared provider identity, not a verified JWT.
CREATE TABLE native_oauth_source_bindings (
    tenant_id TEXT NOT NULL REFERENCES tenants(id),
    provider_driver TEXT NOT NULL CHECK (provider_driver = 'cursor'),
    source_identity_hash TEXT NOT NULL,
    provider_subject_hash TEXT NOT NULL,
    source_document_sha256 TEXT NOT NULL,
    payload_digest TEXT NOT NULL,
    source_layout TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL UNIQUE REFERENCES upstream_accounts(id),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, provider_driver, source_identity_hash),
    UNIQUE (tenant_id, provider_driver, provider_subject_hash)
);
