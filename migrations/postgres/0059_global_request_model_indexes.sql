-- Bound global model-filtered request history to matching rows before the
-- per-source Top-N merge.  request_records is partitioned, so creating this
-- parent index also installs the same ordered index on every existing leaf and
-- makes future partitions inherit it.
CREATE INDEX IF NOT EXISTS request_records_global_model_time_idx
    ON request_records (model, created_at DESC, id DESC);

-- Generation history participates in the same operator request list under its
-- public model name and therefore needs an equivalent global access path.
CREATE INDEX IF NOT EXISTS generation_jobs_global_model_time_idx
    ON generation_jobs (public_model, created_at DESC, id DESC);

COMMENT ON INDEX request_records_global_model_time_idx IS
    'Global model-filtered newest-request access path for operator history.';
COMMENT ON INDEX generation_jobs_global_model_time_idx IS
    'Global public-model-filtered newest-generation access path for operator history.';
