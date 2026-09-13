-- Request and response spools share response_archive_spool_budget.
CREATE TABLE request_archive_spools (
    request_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    reservation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('capturing', 'pending', 'uploading', 'bound', 'gap')),
    chunk_count BIGINT NOT NULL DEFAULT 0 CHECK (chunk_count BETWEEN 0 AND 65536),
    byte_count BIGINT NOT NULL DEFAULT 0 CHECK (byte_count BETWEEN 0 AND 67108864),
    cipher_bytes BIGINT NOT NULL DEFAULT 0 CHECK (cipher_bytes BETWEEN 0 AND 268435456),
    lease_owner TEXT,
    lease_token TEXT,
    lease_expires_at BIGINT,
    attempts BIGINT NOT NULL DEFAULT 0 CHECK (attempts BETWEEN 0 AND 10),
    next_attempt_at BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    cleaned_at BIGINT,
    last_error_code TEXT CHECK (last_error_code IS NULL OR last_error_code IN (
        'capacity', 'capture_timeout', 'capture_failed', 'upload_failed',
        'decrypt_failed', 'lease_lost', 'invalid_chunk', 'internal'
    )),
    bound_locator TEXT
);
CREATE TABLE request_archive_spool_chunks (
    request_id TEXT NOT NULL REFERENCES request_archive_spools(request_id),
    seq BIGINT NOT NULL CHECK (seq >= 0),
    ciphertext TEXT NOT NULL,
    byte_count BIGINT NOT NULL CHECK (byte_count > 0),
    PRIMARY KEY (request_id, seq)
);
CREATE INDEX request_archive_spool_claim ON request_archive_spools(state, next_attempt_at, lease_expires_at);
CREATE INDEX request_archive_spool_expiry ON request_archive_spools(expires_at, request_id) WHERE cleaned_at IS NULL;
CREATE INDEX request_archive_spool_bound_gc ON request_archive_spools(updated_at, request_id) WHERE cleaned_at IS NULL AND state = 'bound';
CREATE INDEX request_archive_spool_exhausted_gc ON request_archive_spools(lease_expires_at, request_id) WHERE cleaned_at IS NULL AND state = 'uploading' AND attempts >= 10;
