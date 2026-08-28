-- Keep the portable operator request-list plan bounded when a model filter is
-- applied without a tenant scope.
CREATE INDEX IF NOT EXISTS request_records_global_model_time_idx
    ON request_records (model, created_at DESC, id DESC);

CREATE INDEX IF NOT EXISTS generation_jobs_global_model_time_idx
    ON generation_jobs (public_model, created_at DESC, id DESC);
