ALTER TABLE response_archive_spools ADD COLUMN IF NOT EXISTS body_blake3 TEXT
    CHECK (body_blake3 IS NULL OR LENGTH(body_blake3) = 64);

ALTER TABLE response_archive_spool_budget
    DROP CONSTRAINT IF EXISTS response_archive_spool_budget_cipher_bytes_check;
ALTER TABLE response_archive_spool_budget
    ADD CONSTRAINT response_archive_spool_budget_cipher_bytes_check CHECK (cipher_bytes >= 0);
ALTER TABLE archive_budget_reservations
    DROP CONSTRAINT IF EXISTS archive_budget_reservations_cipher_bytes_check;
ALTER TABLE archive_budget_reservations
    ADD CONSTRAINT archive_budget_reservations_cipher_bytes_check CHECK (cipher_bytes >= 0);

ALTER TABLE request_archive_spools
    DROP CONSTRAINT IF EXISTS request_archive_spools_cipher_bytes_check,
    DROP CONSTRAINT IF EXISTS request_archive_spools_chunk_count_check,
    DROP CONSTRAINT IF EXISTS request_archive_spools_byte_count_check,
    DROP CONSTRAINT IF EXISTS request_archive_spools_attempts_check;
ALTER TABLE request_archive_spools
    ADD CONSTRAINT request_archive_spools_cipher_bytes_check CHECK (cipher_bytes >= 0),
    ADD CONSTRAINT request_archive_spools_chunk_count_check CHECK (chunk_count >= 0),
    ADD CONSTRAINT request_archive_spools_byte_count_check CHECK (byte_count >= 0),
    ADD CONSTRAINT request_archive_spools_attempts_check CHECK (attempts >= 0);
ALTER TABLE response_archive_spools
    DROP CONSTRAINT IF EXISTS response_archive_spools_cipher_bytes_check,
    DROP CONSTRAINT IF EXISTS response_archive_spools_chunk_count_check,
    DROP CONSTRAINT IF EXISTS response_archive_spools_byte_count_check,
    DROP CONSTRAINT IF EXISTS response_archive_spools_attempts_check;
ALTER TABLE response_archive_spools
    ADD CONSTRAINT response_archive_spools_cipher_bytes_check CHECK (cipher_bytes >= 0),
    ADD CONSTRAINT response_archive_spools_chunk_count_check CHECK (chunk_count >= 0),
    ADD CONSTRAINT response_archive_spools_byte_count_check CHECK (byte_count >= 0),
    ADD CONSTRAINT response_archive_spools_attempts_check CHECK (attempts >= 0);

ALTER TABLE request_archive_spools
    DROP CONSTRAINT IF EXISTS request_archive_spools_last_error_code_check;
ALTER TABLE request_archive_spools
    ADD CONSTRAINT request_archive_spools_last_error_code_check CHECK (
        last_error_code IS NULL OR last_error_code IN (
            'capacity', 'capture_timeout', 'capture_failed', 'upload_failed', 'upload_timeout',
            'decrypt_failed', 'lease_lost', 'invalid_chunk', 'internal'
        )
    );
ALTER TABLE response_archive_spools
    DROP CONSTRAINT IF EXISTS response_archive_spools_last_error_code_check;
ALTER TABLE response_archive_spools
    ADD CONSTRAINT response_archive_spools_last_error_code_check CHECK (
        last_error_code IS NULL OR last_error_code IN (
            'capacity', 'capture_timeout', 'capture_failed', 'upload_failed', 'upload_timeout',
            'decrypt_failed', 'lease_lost', 'invalid_chunk', 'internal'
        )
    );
