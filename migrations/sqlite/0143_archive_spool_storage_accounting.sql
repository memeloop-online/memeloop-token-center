CREATE TABLE response_archive_spool_budget_v143 (
    singleton BIGINT PRIMARY KEY CHECK (singleton = 1),
    cipher_bytes BIGINT NOT NULL CHECK (cipher_bytes >= 0),
    request_cipher_bytes BIGINT NOT NULL DEFAULT 0 CHECK (request_cipher_bytes >= 0 AND request_cipher_bytes <= cipher_bytes)
);
INSERT INTO response_archive_spool_budget_v143 SELECT singleton, cipher_bytes, request_cipher_bytes FROM response_archive_spool_budget;
DROP TABLE response_archive_spool_budget;
ALTER TABLE response_archive_spool_budget_v143 RENAME TO response_archive_spool_budget;

CREATE TABLE archive_budget_reservations_v143 (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL,
    purpose TEXT NOT NULL CHECK (purpose IN ('request', 'response')),
    cipher_bytes BIGINT NOT NULL CHECK (cipher_bytes >= 0),
    expires_at BIGINT NOT NULL
);
INSERT INTO archive_budget_reservations_v143 SELECT id, request_id, purpose, cipher_bytes, expires_at FROM archive_budget_reservations;
DROP TABLE archive_budget_reservations;
ALTER TABLE archive_budget_reservations_v143 RENAME TO archive_budget_reservations;
CREATE INDEX archive_budget_reservations_expiry ON archive_budget_reservations(expires_at, id);

CREATE TABLE response_archive_spools_v143 (
    request_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    reservation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('capturing', 'pending', 'uploading', 'bound', 'gap')),
    chunk_count BIGINT NOT NULL DEFAULT 0 CHECK (chunk_count >= 0),
    byte_count BIGINT NOT NULL DEFAULT 0 CHECK (byte_count >= 0),
    cipher_bytes BIGINT NOT NULL DEFAULT 0 CHECK (cipher_bytes >= 0),
    lease_owner TEXT,
    lease_token TEXT,
    lease_expires_at BIGINT,
    attempts BIGINT NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    cleaned_at BIGINT,
    last_error_code TEXT CHECK (last_error_code IS NULL OR last_error_code IN (
        'capacity', 'capture_timeout', 'capture_failed', 'upload_failed', 'upload_timeout',
        'decrypt_failed', 'lease_lost', 'invalid_chunk', 'internal'
    )),
    bound_locator TEXT,
    body_blake3 TEXT CHECK (body_blake3 IS NULL OR LENGTH(body_blake3) = 64)
);
INSERT INTO response_archive_spools_v143
SELECT request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes,
    lease_owner, lease_token, lease_expires_at, attempts, next_attempt_at, created_at,
    updated_at, expires_at, cleaned_at, last_error_code, bound_locator, NULL FROM response_archive_spools;
CREATE TABLE response_archive_spool_chunks_v143 (
    request_id TEXT NOT NULL REFERENCES response_archive_spools_v143(request_id),
    seq BIGINT NOT NULL CHECK (seq >= 0),
    ciphertext TEXT NOT NULL,
    byte_count BIGINT NOT NULL CHECK (byte_count > 0),
    PRIMARY KEY (request_id, seq)
);
INSERT INTO response_archive_spool_chunks_v143 SELECT request_id, seq, ciphertext, byte_count FROM response_archive_spool_chunks;
DROP TABLE response_archive_spool_chunks;
DROP TABLE response_archive_spools;
ALTER TABLE response_archive_spools_v143 RENAME TO response_archive_spools;
ALTER TABLE response_archive_spool_chunks_v143 RENAME TO response_archive_spool_chunks;
CREATE INDEX response_archive_spool_claim ON response_archive_spools(state, next_attempt_at, lease_expires_at);
CREATE INDEX response_archive_spool_expiry ON response_archive_spools(expires_at, request_id) WHERE cleaned_at IS NULL;
CREATE INDEX response_archive_spool_bound_gc ON response_archive_spools(updated_at, request_id) WHERE cleaned_at IS NULL AND state = 'bound';

CREATE TABLE request_archive_spools_v143 (
    request_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    reservation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('capturing', 'pending', 'uploading', 'bound', 'gap')),
    chunk_count BIGINT NOT NULL DEFAULT 0 CHECK (chunk_count >= 0),
    byte_count BIGINT NOT NULL DEFAULT 0 CHECK (byte_count >= 0),
    cipher_bytes BIGINT NOT NULL DEFAULT 0 CHECK (cipher_bytes >= 0),
    lease_owner TEXT,
    lease_token TEXT,
    lease_expires_at BIGINT,
    attempts BIGINT NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at BIGINT NOT NULL,
    created_at BIGINT NOT NULL,
    updated_at BIGINT NOT NULL,
    expires_at BIGINT NOT NULL,
    cleaned_at BIGINT,
    last_error_code TEXT CHECK (last_error_code IS NULL OR last_error_code IN (
        'capacity', 'capture_timeout', 'capture_failed', 'upload_failed', 'upload_timeout',
        'decrypt_failed', 'lease_lost', 'invalid_chunk', 'internal'
    )),
    bound_locator TEXT,
    gap_reason TEXT CHECK (gap_reason IS NULL OR gap_reason IN ('capacity', 'retention_limit')),
    body_byte_count BIGINT CHECK (body_byte_count IS NULL OR body_byte_count >= 0),
    body_blake3 TEXT CHECK (body_blake3 IS NULL OR LENGTH(body_blake3) = 64)
);
INSERT INTO request_archive_spools_v143
SELECT request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes,
    lease_owner, lease_token, lease_expires_at, attempts, next_attempt_at, created_at,
    updated_at, expires_at, cleaned_at, last_error_code, bound_locator,
    gap_reason, body_byte_count, body_blake3 FROM request_archive_spools;
CREATE TABLE request_archive_spool_chunks_v143 (
    request_id TEXT NOT NULL REFERENCES request_archive_spools_v143(request_id),
    seq BIGINT NOT NULL CHECK (seq >= 0),
    ciphertext TEXT NOT NULL,
    byte_count BIGINT NOT NULL CHECK (byte_count > 0),
    PRIMARY KEY (request_id, seq)
);
INSERT INTO request_archive_spool_chunks_v143 SELECT request_id, seq, ciphertext, byte_count FROM request_archive_spool_chunks;
DROP TABLE request_archive_spool_chunks;
DROP TABLE request_archive_spools;
ALTER TABLE request_archive_spools_v143 RENAME TO request_archive_spools;
ALTER TABLE request_archive_spool_chunks_v143 RENAME TO request_archive_spool_chunks;
CREATE INDEX request_archive_spool_claim ON request_archive_spools(state, next_attempt_at, lease_expires_at);
CREATE INDEX request_archive_spool_expiry ON request_archive_spools(expires_at, request_id) WHERE cleaned_at IS NULL;
CREATE INDEX request_archive_spool_bound_gc ON request_archive_spools(updated_at, request_id) WHERE cleaned_at IS NULL AND state = 'bound';
