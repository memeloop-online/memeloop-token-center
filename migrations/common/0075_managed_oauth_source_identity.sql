ALTER TABLE upstream_account_imports
    ADD COLUMN source_identity_hash TEXT CHECK (
        source_identity_hash IS NULL OR (
            LENGTH(source_identity_hash) = 64
            AND source_identity_hash = LOWER(source_identity_hash)
        )
    );

ALTER TABLE upstream_account_imports
    ADD COLUMN source_document_sha256 TEXT CHECK (
        source_document_sha256 IS NULL OR (
            LENGTH(source_document_sha256) = 64
            AND source_document_sha256 = LOWER(source_document_sha256)
        )
    );

CREATE UNIQUE INDEX upstream_account_imports_source_identity_idx
    ON upstream_account_imports (tenant_id, import_kind, source_identity_hash);
