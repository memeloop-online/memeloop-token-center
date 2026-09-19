-- Request archive degradation remains part of the existing spool state
-- machine. Gap rows retain only bounded, non-recoverable evidence; they never
-- become upload candidates and therefore cannot be backfilled after recovery.
ALTER TABLE request_archive_spools ADD COLUMN gap_reason TEXT CHECK (gap_reason IS NULL OR gap_reason IN ('capacity', 'retention_limit'));
ALTER TABLE request_archive_spools ADD COLUMN body_byte_count BIGINT CHECK (body_byte_count IS NULL OR body_byte_count >= 0);
ALTER TABLE request_archive_spools ADD COLUMN body_blake3 TEXT CHECK (body_blake3 IS NULL OR LENGTH(body_blake3) = 64);
