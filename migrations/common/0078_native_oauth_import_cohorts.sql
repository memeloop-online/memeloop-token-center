CREATE TABLE native_oauth_import_cohorts (
    id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    contract TEXT NOT NULL CHECK (contract = 'atomic_kimi_cohort_v2'),
    expected_current_cohort_sha256 TEXT NOT NULL CHECK (
        LENGTH(expected_current_cohort_sha256) = 64
        AND expected_current_cohort_sha256 = LOWER(expected_current_cohort_sha256)
    ),
    new_cohort_sha256 TEXT NOT NULL CHECK (
        LENGTH(new_cohort_sha256) = 64
        AND new_cohort_sha256 = LOWER(new_cohort_sha256)
    ),
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    UNIQUE (tenant_id, contract),
    UNIQUE (id, tenant_id),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE
);

CREATE TABLE native_oauth_import_receipts (
    cohort_id TEXT NOT NULL,
    tenant_id TEXT NOT NULL,
    ordinal BIGINT NOT NULL CHECK (ordinal IN (1, 2)),
    source_identity_hash TEXT NOT NULL CHECK (
        LENGTH(source_identity_hash) = 64
        AND source_identity_hash = LOWER(source_identity_hash)
    ),
    source_document_sha256 TEXT NOT NULL CHECK (
        LENGTH(source_document_sha256) = 64
        AND source_document_sha256 = LOWER(source_document_sha256)
    ),
    payload_digest TEXT NOT NULL CHECK (
        LENGTH(payload_digest) = 64
        AND payload_digest = LOWER(payload_digest)
    ),
    upstream_account_id TEXT NOT NULL UNIQUE,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, source_identity_hash),
    UNIQUE (cohort_id, ordinal),
    FOREIGN KEY (cohort_id, tenant_id)
        REFERENCES native_oauth_import_cohorts(id, tenant_id) ON DELETE CASCADE,
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    FOREIGN KEY (upstream_account_id) REFERENCES upstream_accounts(id)
        ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX native_oauth_import_receipts_tenant_account_idx
    ON native_oauth_import_receipts (tenant_id, upstream_account_id);
