-- Async generation admission freezes the selected route and candidate.  The
-- route id intentionally has no foreign key: accepted work must remain
-- attributable and executable after an operator edits or deletes the route.
-- Existing rows remain nullable because historical routing can only be
-- recovered when there is exactly one unambiguous match.
ALTER TABLE generation_jobs ADD COLUMN model_route_id TEXT;

CREATE TEMPORARY TABLE generation_route_snapshot_backfill AS
SELECT job.id AS job_id, MIN(route.id) AS model_route_id
FROM generation_jobs job
JOIN model_routes route
  ON route.tenant_id = job.tenant_id
 AND route.public_model = job.public_model
 AND route.protocol = 'generation'
JOIN model_route_upstream_accounts candidate
  ON candidate.tenant_id = route.tenant_id
 AND candidate.model_route_id = route.id
 AND candidate.upstream_account_id = job.upstream_account_id
 AND candidate.upstream_model = job.upstream_model
JOIN upstream_accounts account
  ON account.tenant_id = route.tenant_id
 AND account.id = candidate.upstream_account_id
 AND account.driver = job.driver
WHERE job.model_route_id IS NULL
GROUP BY job.id
HAVING COUNT(DISTINCT route.id) = 1;

CREATE UNIQUE INDEX generation_route_snapshot_backfill_job_idx
    ON generation_route_snapshot_backfill (job_id);

UPDATE generation_jobs
SET model_route_id = (
    SELECT backfill.model_route_id
    FROM generation_route_snapshot_backfill backfill
    WHERE backfill.job_id = generation_jobs.id
)
WHERE model_route_id IS NULL
  AND EXISTS (
    SELECT 1
    FROM generation_route_snapshot_backfill backfill
    WHERE backfill.job_id = generation_jobs.id
  );

DROP TABLE generation_route_snapshot_backfill;

CREATE INDEX generation_jobs_model_route_created_idx
    ON generation_jobs (model_route_id, created_at DESC, id DESC)
    WHERE model_route_id IS NOT NULL;
