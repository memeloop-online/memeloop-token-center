-- NULL preserves the original legacy backend. This identity is immutable for
-- the lifetime of an attempt; enabling new writes requires compatible readers.
ALTER TABLE archive_staging_attempts ADD COLUMN backend_identity TEXT
    CHECK (backend_identity IS NULL OR length(backend_identity) = 64);
