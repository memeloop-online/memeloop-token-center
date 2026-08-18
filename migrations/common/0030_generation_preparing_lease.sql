CREATE INDEX IF NOT EXISTS generation_jobs_preparing_lease_idx
    ON generation_jobs (lease_expires_at, id)
    WHERE status = 'preparing';
