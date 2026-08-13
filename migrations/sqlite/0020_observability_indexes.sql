CREATE INDEX IF NOT EXISTS request_records_tenant_route_time_idx
    ON request_records (tenant_id, model_route_id, created_at DESC, id DESC)
    WHERE model_route_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS request_records_key_route_time_idx
    ON request_records (key_id, model_route_id, created_at DESC, id DESC)
    WHERE model_route_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS request_records_tenant_duration_time_idx
    ON request_records (tenant_id, duration_ms, created_at DESC, id DESC)
    WHERE duration_ms IS NOT NULL;
CREATE INDEX IF NOT EXISTS request_records_tenant_cost_time_idx
    ON request_records (tenant_id, cost_micros, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS key_records_tenant_alias_prefix_idx
    ON key_records (tenant_id, LOWER(alias), id);
CREATE INDEX IF NOT EXISTS principals_tenant_external_prefix_idx
    ON principals (tenant_id, LOWER(external_id), id);
CREATE INDEX IF NOT EXISTS generation_jobs_tenant_time_idx
    ON generation_jobs (tenant_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS generation_jobs_tenant_upstream_time_idx
    ON generation_jobs (tenant_id, upstream_account_id, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS generation_jobs_tenant_cost_time_idx
    ON generation_jobs (tenant_id, cost_micros, created_at DESC, id DESC);
CREATE INDEX IF NOT EXISTS generation_jobs_global_time_idx
    ON generation_jobs (created_at DESC, id DESC);
