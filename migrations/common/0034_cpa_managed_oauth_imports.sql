CREATE TABLE IF NOT EXISTS upstream_account_imports (
    tenant_id TEXT NOT NULL,
    import_kind TEXT NOT NULL CHECK (import_kind = 'cpa_managed_oauth'),
    source_key TEXT NOT NULL CHECK (
        LENGTH(source_key) = 64 AND source_key = LOWER(source_key)
    ),
    contract_version BIGINT NOT NULL CHECK (contract_version = 1),
    payload_digest TEXT NOT NULL CHECK (
        LENGTH(payload_digest) = 64 AND payload_digest = LOWER(payload_digest)
    ),
    upstream_account_id TEXT NOT NULL UNIQUE,
    created_at BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, import_kind, source_key),
    FOREIGN KEY (tenant_id) REFERENCES tenants(id) ON DELETE CASCADE,
    FOREIGN KEY (upstream_account_id) REFERENCES upstream_accounts(id)
        ON DELETE NO ACTION DEFERRABLE INITIALLY DEFERRED
);

CREATE INDEX IF NOT EXISTS upstream_account_imports_created_idx
    ON upstream_account_imports (created_at, upstream_account_id);
