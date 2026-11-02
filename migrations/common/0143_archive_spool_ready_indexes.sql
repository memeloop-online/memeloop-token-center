CREATE INDEX IF NOT EXISTS request_archive_spool_pending_ready
    ON request_archive_spools (next_attempt_at, request_id)
    WHERE state = 'pending' AND cleaned_at IS NULL;
CREATE INDEX IF NOT EXISTS request_archive_spool_uploading_ready
    ON request_archive_spools (lease_expires_at, request_id)
    WHERE state = 'uploading' AND cleaned_at IS NULL;
CREATE INDEX IF NOT EXISTS response_archive_spool_pending_ready
    ON response_archive_spools (next_attempt_at, request_id)
    WHERE state = 'pending' AND cleaned_at IS NULL;
CREATE INDEX IF NOT EXISTS response_archive_spool_uploading_ready
    ON response_archive_spools (lease_expires_at, request_id)
    WHERE state = 'uploading' AND cleaned_at IS NULL;
