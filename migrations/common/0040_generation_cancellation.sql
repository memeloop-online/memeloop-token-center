DROP INDEX IF EXISTS generation_jobs_claim_idx;
CREATE INDEX generation_jobs_claim_idx
    ON generation_jobs (next_attempt_at, created_at, id)
    WHERE status IN ('queued', 'running', 'submitting', 'cancelling');
