-- 64MiB 曾经是单份明文上限。原始请求 JSON 和原始返回 JSON 现在整份归档，
-- 不再按这个数截断。分片数上限仍是 65536，每片 64KiB，所以明文 byte_count
-- 上限改为 4GiB。密文预算仍由 response_archive_spool_budget 限制。

CREATE TABLE response_archive_spools_v139 (
    request_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    reservation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('capturing', 'pending', 'uploading', 'bound', 'gap')),
    chunk_count BIGINT NOT NULL DEFAULT 0 CHECK (chunk_count BETWEEN 0 AND 65536),
    byte_count BIGINT NOT NULL DEFAULT 0 CHECK (byte_count BETWEEN 0 AND 4294967296),
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

INSERT INTO response_archive_spools_v139 (
    request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes,
    lease_owner, lease_token, lease_expires_at, attempts, next_attempt_at, created_at,
    updated_at, expires_at, cleaned_at, last_error_code, bound_locator
)
SELECT
    request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes,
    lease_owner, lease_token, lease_expires_at, attempts, next_attempt_at, created_at,
    updated_at, expires_at, cleaned_at, last_error_code, bound_locator
FROM response_archive_spools;

CREATE TABLE response_archive_spool_chunks_v139 (
    request_id TEXT NOT NULL REFERENCES response_archive_spools_v139(request_id),
    seq BIGINT NOT NULL CHECK (seq >= 0),
    ciphertext TEXT NOT NULL,
    byte_count BIGINT NOT NULL CHECK (byte_count > 0),
    PRIMARY KEY (request_id, seq)
);

INSERT INTO response_archive_spool_chunks_v139 (request_id, seq, ciphertext, byte_count)
SELECT request_id, seq, ciphertext, byte_count FROM response_archive_spool_chunks;

DROP TABLE response_archive_spool_chunks;
DROP TABLE response_archive_spools;

ALTER TABLE response_archive_spools_v139 RENAME TO response_archive_spools;
ALTER TABLE response_archive_spool_chunks_v139 RENAME TO response_archive_spool_chunks;

CREATE INDEX response_archive_spool_claim ON response_archive_spools(state, next_attempt_at, lease_expires_at);
CREATE INDEX response_archive_spool_expiry ON response_archive_spools(expires_at, request_id) WHERE cleaned_at IS NULL;
CREATE INDEX response_archive_spool_bound_gc ON response_archive_spools(updated_at, request_id) WHERE cleaned_at IS NULL AND state = 'bound';
CREATE INDEX response_archive_spool_exhausted_gc ON response_archive_spools(lease_expires_at, request_id) WHERE cleaned_at IS NULL AND state = 'uploading' AND attempts >= 10;

CREATE TABLE request_archive_spools_v139 (
    request_id TEXT PRIMARY KEY,
    tenant_id TEXT NOT NULL,
    reservation_id TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('capturing', 'pending', 'uploading', 'bound', 'gap')),
    chunk_count BIGINT NOT NULL DEFAULT 0 CHECK (chunk_count BETWEEN 0 AND 65536),
    byte_count BIGINT NOT NULL DEFAULT 0 CHECK (byte_count BETWEEN 0 AND 4294967296),
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
    bound_locator TEXT,
    gap_reason TEXT CHECK (gap_reason IS NULL OR gap_reason IN ('capacity', 'retention_limit')),
    body_byte_count BIGINT CHECK (body_byte_count IS NULL OR body_byte_count >= 0),
    body_blake3 TEXT CHECK (body_blake3 IS NULL OR LENGTH(body_blake3) = 64)
);

INSERT INTO request_archive_spools_v139 (
    request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes,
    lease_owner, lease_token, lease_expires_at, attempts, next_attempt_at, created_at,
    updated_at, expires_at, cleaned_at, last_error_code, bound_locator,
    gap_reason, body_byte_count, body_blake3
)
SELECT
    request_id, tenant_id, reservation_id, state, chunk_count, byte_count, cipher_bytes,
    lease_owner, lease_token, lease_expires_at, attempts, next_attempt_at, created_at,
    updated_at, expires_at, cleaned_at, last_error_code, bound_locator,
    gap_reason, body_byte_count, body_blake3
FROM request_archive_spools;

CREATE TABLE request_archive_spool_chunks_v139 (
    request_id TEXT NOT NULL REFERENCES request_archive_spools_v139(request_id),
    seq BIGINT NOT NULL CHECK (seq >= 0),
    ciphertext TEXT NOT NULL,
    byte_count BIGINT NOT NULL CHECK (byte_count > 0),
    PRIMARY KEY (request_id, seq)
);

INSERT INTO request_archive_spool_chunks_v139 (request_id, seq, ciphertext, byte_count)
SELECT request_id, seq, ciphertext, byte_count FROM request_archive_spool_chunks;

DROP TABLE request_archive_spool_chunks;
DROP TABLE request_archive_spools;

ALTER TABLE request_archive_spools_v139 RENAME TO request_archive_spools;
ALTER TABLE request_archive_spool_chunks_v139 RENAME TO request_archive_spool_chunks;

CREATE INDEX request_archive_spool_claim ON request_archive_spools(state, next_attempt_at, lease_expires_at);
CREATE INDEX request_archive_spool_expiry ON request_archive_spools(expires_at, request_id) WHERE cleaned_at IS NULL;
CREATE INDEX request_archive_spool_bound_gc ON request_archive_spools(updated_at, request_id) WHERE cleaned_at IS NULL AND state = 'bound';
CREATE INDEX request_archive_spool_exhausted_gc ON request_archive_spools(lease_expires_at, request_id) WHERE cleaned_at IS NULL AND state = 'uploading' AND attempts >= 10;
