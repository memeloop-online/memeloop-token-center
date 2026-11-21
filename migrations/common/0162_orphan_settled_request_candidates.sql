-- Bound the legacy settled-reservation recovery lane by unfinished requests.
CREATE INDEX request_records_orphan_settled_candidates
ON request_records (created_at, tenant_id, key_id, id)
WHERE completed_at IS NULL AND submission_started_at IS NULL;
