-- Preserve the immutable dimensions required to interpret generation units.
-- Unknown is explicit: historical ComfyUI failures have no output MIME type,
-- so their modality cannot be recovered without guessing.
ALTER TABLE generation_stats_facts ADD COLUMN modality TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE generation_stats_facts ADD COLUMN billing_unit TEXT NOT NULL DEFAULT 'unknown';
ALTER TABLE generation_stats_facts ADD COLUMN model_route_id TEXT NOT NULL DEFAULT '';

UPDATE generation_stats_facts
   SET billing_unit = COALESCE(
           NULLIF((SELECT job.billing_unit_snapshot
                     FROM generation_jobs job
                    WHERE job.id = generation_stats_facts.job_id), ''),
           'unknown'
       ),
       model_route_id = COALESCE(
           (SELECT job.model_route_id
              FROM generation_jobs job
             WHERE job.id = generation_stats_facts.job_id),
           ''
       ),
       modality = CASE
           WHEN EXISTS (
               SELECT 1 FROM generation_assets asset
                WHERE asset.job_id = generation_stats_facts.job_id
                  AND asset.mime_type LIKE 'video/%'
           ) THEN 'video'
           WHEN EXISTS (
               SELECT 1 FROM generation_assets asset
                WHERE asset.job_id = generation_stats_facts.job_id
                  AND asset.mime_type LIKE 'image/%'
           ) THEN 'image'
           WHEN (SELECT job.driver FROM generation_jobs job
                  WHERE job.id = generation_stats_facts.job_id) = 'volcengine-seedance'
               THEN 'video'
           ELSE 'unknown'
       END;

CREATE TABLE generation_usage_dimensions_hourly (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    hour_bucket BIGINT NOT NULL,
    model TEXT NOT NULL,
    status_class TEXT NOT NULL,
    error_code TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    modality TEXT NOT NULL,
    billing_unit TEXT NOT NULL,
    currency TEXT NOT NULL,
    units BIGINT NOT NULL,
    PRIMARY KEY (
        tenant_id, key_id, hour_bucket, model, status_class, error_code,
        upstream_account_id, model_route_id, modality, billing_unit, currency
    )
);

CREATE TABLE generation_usage_dimensions_daily (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    day_bucket BIGINT NOT NULL,
    model TEXT NOT NULL,
    status_class TEXT NOT NULL,
    error_code TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    modality TEXT NOT NULL,
    billing_unit TEXT NOT NULL,
    currency TEXT NOT NULL,
    units BIGINT NOT NULL,
    PRIMARY KEY (
        tenant_id, key_id, day_bucket, model, status_class, error_code,
        upstream_account_id, model_route_id, modality, billing_unit, currency
    )
);

INSERT INTO generation_usage_dimensions_hourly (
    tenant_id, key_id, hour_bucket, model, status_class, error_code,
    upstream_account_id, model_route_id, modality, billing_unit, currency, units
)
SELECT tenant_id, key_id, created_at / 3600000, model, status_class, error_code,
       upstream_account_id, model_route_id, modality, billing_unit, currency,
       SUM(billed_units)
  FROM generation_stats_facts
 GROUP BY tenant_id, key_id, created_at / 3600000, model, status_class,
          error_code, upstream_account_id, model_route_id, modality,
          billing_unit, currency;

INSERT INTO generation_usage_dimensions_daily (
    tenant_id, key_id, day_bucket, model, status_class, error_code,
    upstream_account_id, model_route_id, modality, billing_unit, currency, units
)
SELECT tenant_id, key_id, created_at / 86400000, model, status_class, error_code,
       upstream_account_id, model_route_id, modality, billing_unit, currency,
       SUM(billed_units)
  FROM generation_stats_facts
 GROUP BY tenant_id, key_id, created_at / 86400000, model, status_class,
          error_code, upstream_account_id, model_route_id, modality,
          billing_unit, currency;

CREATE INDEX generation_usage_dimensions_hourly_tenant_time_idx
    ON generation_usage_dimensions_hourly (tenant_id, hour_bucket);
CREATE INDEX generation_usage_dimensions_hourly_key_time_idx
    ON generation_usage_dimensions_hourly (key_id, hour_bucket);
CREATE INDEX generation_usage_dimensions_hourly_tenant_model_time_idx
    ON generation_usage_dimensions_hourly (tenant_id, model, hour_bucket);
CREATE INDEX generation_usage_dimensions_hourly_tenant_route_time_idx
    ON generation_usage_dimensions_hourly (tenant_id, model_route_id, hour_bucket)
    WHERE model_route_id <> '';

CREATE INDEX generation_usage_dimensions_daily_tenant_time_idx
    ON generation_usage_dimensions_daily (tenant_id, day_bucket);
CREATE INDEX generation_usage_dimensions_daily_key_time_idx
    ON generation_usage_dimensions_daily (key_id, day_bucket);
CREATE INDEX generation_usage_dimensions_daily_tenant_model_time_idx
    ON generation_usage_dimensions_daily (tenant_id, model, day_bucket);
CREATE INDEX generation_usage_dimensions_daily_tenant_route_time_idx
    ON generation_usage_dimensions_daily (tenant_id, model_route_id, day_bucket)
    WHERE model_route_id <> '';
