-- Generation currency is historical billing data. Snapshot it alongside request
-- currency so edge-bucket analysis never depends on a mutable dimension join.
ALTER TABLE generation_stats_facts ADD COLUMN currency TEXT NOT NULL DEFAULT '';
ALTER TABLE generation_daily_aggregates ADD COLUMN currency TEXT NOT NULL DEFAULT '';

UPDATE request_records
   SET currency = COALESCE(
       (SELECT k.currency FROM key_records k
         WHERE k.id = request_records.key_id
           AND k.tenant_id = request_records.tenant_id),
       ''
   )
 WHERE currency = '';

UPDATE request_stats_facts
   SET currency = COALESCE(
       (SELECT k.currency FROM key_records k
         WHERE k.id = request_stats_facts.key_id
           AND k.tenant_id = request_stats_facts.tenant_id),
       ''
   )
 WHERE currency = '';

UPDATE request_daily_aggregates
   SET currency = COALESCE(
       (SELECT k.currency FROM key_records k
         WHERE k.id = request_daily_aggregates.key_id
           AND k.tenant_id = request_daily_aggregates.tenant_id),
       ''
   )
 WHERE currency = '';

UPDATE generation_stats_facts
   SET currency = COALESCE(
       (SELECT k.currency FROM key_records k
         WHERE k.id = generation_stats_facts.key_id
           AND k.tenant_id = generation_stats_facts.tenant_id),
       ''
   );

UPDATE generation_daily_aggregates
   SET currency = COALESCE(
       (SELECT k.currency FROM key_records k
         WHERE k.id = generation_daily_aggregates.key_id
           AND k.tenant_id = generation_daily_aggregates.tenant_id),
       ''
   );

UPDATE usage_analysis_hourly
   SET currency = COALESCE(
       (SELECT k.currency FROM key_records k
         WHERE k.id = usage_analysis_hourly.key_id
           AND k.tenant_id = usage_analysis_hourly.tenant_id),
       ''
   )
 WHERE currency = '';

UPDATE usage_analysis_daily
   SET currency = COALESCE(
       (SELECT k.currency FROM key_records k
         WHERE k.id = usage_analysis_daily.key_id
           AND k.tenant_id = usage_analysis_daily.tenant_id),
       ''
   )
 WHERE currency = '';

-- Fail the migration instead of leaving a latent endpoint-wide 500. A missing
-- stable key relationship must be repaired explicitly and must not be guessed.
CREATE TEMP TABLE observability_currency_migration_guard (
    invalid BIGINT NOT NULL CHECK (invalid = 0)
);

INSERT INTO observability_currency_migration_guard (invalid)
SELECT 1
 WHERE EXISTS (SELECT 1 FROM request_records WHERE currency = '')
    OR EXISTS (SELECT 1 FROM request_stats_facts WHERE currency = '')
    OR EXISTS (SELECT 1 FROM request_daily_aggregates WHERE currency = '')
    OR EXISTS (SELECT 1 FROM generation_stats_facts WHERE currency = '')
    OR EXISTS (SELECT 1 FROM generation_daily_aggregates WHERE currency = '')
    OR EXISTS (SELECT 1 FROM usage_analysis_hourly WHERE currency = '')
    OR EXISTS (SELECT 1 FROM usage_analysis_daily WHERE currency = '');

DROP TABLE observability_currency_migration_guard;

-- The original generation rollup key predated currency snapshots. Rebuild it
-- so a historical key can never merge costs denominated in different
-- currencies into one aggregate row.
CREATE TABLE generation_daily_aggregates_v39 (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    day_bucket BIGINT NOT NULL,
    model TEXT NOT NULL,
    status_class TEXT NOT NULL,
    error_code TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    requests BIGINT NOT NULL,
    billed_units BIGINT NOT NULL,
    cost_micros BIGINT NOT NULL,
    currency TEXT NOT NULL,
    PRIMARY KEY (
        tenant_id,
        key_id,
        day_bucket,
        model,
        status_class,
        error_code,
        upstream_account_id,
        currency
    )
);

INSERT INTO generation_daily_aggregates_v39 (
    tenant_id, key_id, day_bucket, model, status_class, error_code,
    upstream_account_id, requests, billed_units, cost_micros, currency
)
SELECT tenant_id, key_id, day_bucket, model, status_class, error_code,
       upstream_account_id, SUM(requests), SUM(billed_units), SUM(cost_micros), currency
  FROM generation_daily_aggregates
 GROUP BY tenant_id, key_id, day_bucket, model, status_class, error_code,
          upstream_account_id, currency;

DROP TABLE generation_daily_aggregates;
ALTER TABLE generation_daily_aggregates_v39 RENAME TO generation_daily_aggregates;

CREATE INDEX generation_daily_aggregates_tenant_day_idx
    ON generation_daily_aggregates (tenant_id, day_bucket, model, status_class);
CREATE INDEX generation_daily_aggregates_key_day_idx
    ON generation_daily_aggregates (key_id, day_bucket, model, status_class);

-- The leading filter dimension precedes the time range. This avoids scanning a
-- high-cardinality tenant's entire 31/93-day rollup for exact drill-downs.
CREATE INDEX usage_analysis_hourly_tenant_model_time_idx
    ON usage_analysis_hourly (tenant_id, model, hour_bucket);
CREATE INDEX usage_analysis_hourly_tenant_error_time_idx
    ON usage_analysis_hourly (tenant_id, error_code, hour_bucket)
    WHERE error_code <> '';
CREATE INDEX usage_analysis_hourly_tenant_route_time_idx
    ON usage_analysis_hourly (tenant_id, model_route_id, hour_bucket)
    WHERE model_route_id <> '';

CREATE INDEX usage_analysis_daily_tenant_model_time_idx
    ON usage_analysis_daily (tenant_id, model, day_bucket);
CREATE INDEX usage_analysis_daily_tenant_error_time_idx
    ON usage_analysis_daily (tenant_id, error_code, day_bucket)
    WHERE error_code <> '';
CREATE INDEX usage_analysis_daily_tenant_route_time_idx
    ON usage_analysis_daily (tenant_id, model_route_id, day_bucket)
    WHERE model_route_id <> '';
