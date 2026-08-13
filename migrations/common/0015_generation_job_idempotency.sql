ALTER TABLE generation_jobs ADD COLUMN client_idempotency_key TEXT;
ALTER TABLE generation_jobs ADD COLUMN request_hash TEXT;

CREATE UNIQUE INDEX IF NOT EXISTS generation_jobs_key_idempotency_idx
    ON generation_jobs (key_id, client_idempotency_key);
