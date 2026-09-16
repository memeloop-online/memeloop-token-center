-- Rebuild every compact observability projection for one completed UTC day.
-- The caller supplies :day and runs this file with ON_ERROR_STOP enabled.
BEGIN;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;
SET LOCAL lock_timeout = '5s';
SELECT pg_advisory_xact_lock(
  hashtextextended('memeloop-token-center:request-stats', 734627102948314)
);

-- Prevent a request or generation settlement from committing between the source
-- snapshot and projection replacement. The short lock timeout makes live traffic
-- win over maintenance contention: operators can retry the completed day later.
LOCK TABLE request_stats_facts, request_daily_aggregates,
           generation_stats_facts, generation_daily_aggregates,
           usage_analysis_hourly, usage_analysis_daily
  IN SHARE ROW EXCLUSIVE MODE;

CREATE TEMP TABLE mtc_reconcile_day_bounds ON COMMIT DROP AS
SELECT (extract(epoch FROM (:'day'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AS start_ms,
       (extract(epoch FROM ((:'day'::date + 1)::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AS end_ms,
       (:'day'::date - DATE '1970-01-01')::bigint AS day_bucket;

CREATE TEMP TABLE mtc_reconcile_completed_day_guard (
  invalid boolean NOT NULL CHECK (invalid = false)
) ON COMMIT DROP;
INSERT INTO mtc_reconcile_completed_day_guard (invalid)
SELECT true
  FROM mtc_reconcile_day_bounds
 WHERE end_ms > (extract(epoch FROM (CURRENT_DATE::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint;

INSERT INTO request_stats_facts (
  request_id, tenant_id, key_id, created_at, model, protocol, status_class,
  error_code, upstream_account_id, model_route_id, duration_ms,
  input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
  service_tier, currency, cost_micros
)
SELECT r.id, r.tenant_id, r.key_id, r.created_at, r.model, r.protocol,
       CASE WHEN r.status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END,
       COALESCE(r.error_code, ''), COALESCE(r.upstream_account_id, ''),
       COALESCE(r.model_route_id, ''), COALESCE(r.duration_ms, 0),
       r.input_tokens, r.output_tokens, r.cached_input_tokens, r.cache_write_tokens,
       r.service_tier, COALESCE(NULLIF(r.currency, ''), k.currency), r.cost_micros
  FROM request_records r
  JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id
  CROSS JOIN mtc_reconcile_day_bounds b
 WHERE r.created_at >= b.start_ms AND r.created_at < b.end_ms
   AND r.completed_at IS NOT NULL AND r.status_code IS NOT NULL
ON CONFLICT (request_id) DO UPDATE SET
  tenant_id = excluded.tenant_id,
  key_id = excluded.key_id,
  created_at = excluded.created_at,
  model = excluded.model,
  protocol = excluded.protocol,
  status_class = excluded.status_class,
  error_code = excluded.error_code,
  upstream_account_id = excluded.upstream_account_id,
  model_route_id = excluded.model_route_id,
  duration_ms = excluded.duration_ms,
  input_tokens = excluded.input_tokens,
  output_tokens = excluded.output_tokens,
  cached_input_tokens = excluded.cached_input_tokens,
  cache_write_tokens = excluded.cache_write_tokens,
  service_tier = excluded.service_tier,
  currency = excluded.currency,
  cost_micros = excluded.cost_micros;

-- A request moved out of the selected day must not leave a stale fact behind.
DELETE FROM request_stats_facts f
 USING mtc_reconcile_day_bounds b
 WHERE f.created_at >= b.start_ms AND f.created_at < b.end_ms
   AND NOT EXISTS (
     SELECT 1
       FROM request_records r
      WHERE r.id = f.request_id
        AND r.created_at = f.created_at
        AND r.completed_at IS NOT NULL
        AND r.status_code IS NOT NULL
   );

DELETE FROM request_daily_aggregates a
 USING mtc_reconcile_day_bounds b
 WHERE a.day_bucket = b.day_bucket;

INSERT INTO request_daily_aggregates (
  tenant_id, key_id, day_bucket, model, protocol, status_class, error_code,
  upstream_account_id, model_route_id, service_tier, currency, requests,
  input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
  duration_count, duration_sum_ms, cost_micros
)
SELECT f.tenant_id, f.key_id, f.created_at / 86400000, f.model, f.protocol,
       f.status_class, f.error_code, f.upstream_account_id, f.model_route_id,
       f.service_tier, f.currency, COUNT(*), COALESCE(SUM(f.input_tokens), 0),
       COALESCE(SUM(f.output_tokens), 0), COALESCE(SUM(f.cached_input_tokens), 0),
       COALESCE(SUM(f.cache_write_tokens), 0), COUNT(*),
       COALESCE(SUM(f.duration_ms), 0), COALESCE(SUM(f.cost_micros), 0)
  FROM request_stats_facts f
  CROSS JOIN mtc_reconcile_day_bounds b
 WHERE f.created_at >= b.start_ms AND f.created_at < b.end_ms
 GROUP BY f.tenant_id, f.key_id, f.created_at / 86400000, f.model, f.protocol,
          f.status_class, f.error_code, f.upstream_account_id, f.model_route_id,
          f.service_tier, f.currency;

DELETE FROM generation_daily_aggregates a
 USING mtc_reconcile_day_bounds b
 WHERE a.day_bucket = b.day_bucket;

INSERT INTO generation_daily_aggregates (
  tenant_id, key_id, day_bucket, model, status_class, error_code,
  upstream_account_id, requests, billed_units, cost_micros, currency
)
SELECT f.tenant_id, f.key_id, f.created_at / 86400000, f.model, f.status_class,
       f.error_code, f.upstream_account_id, COUNT(*),
       COALESCE(SUM(f.billed_units), 0), COALESCE(SUM(f.cost_micros), 0), f.currency
  FROM generation_stats_facts f
  CROSS JOIN mtc_reconcile_day_bounds b
 WHERE f.created_at >= b.start_ms AND f.created_at < b.end_ms
 GROUP BY f.tenant_id, f.key_id, f.created_at / 86400000, f.model,
          f.status_class, f.error_code, f.upstream_account_id, f.currency;

DELETE FROM usage_analysis_hourly a
 USING mtc_reconcile_day_bounds b
 WHERE a.hour_bucket >= b.start_ms / 3600000
   AND a.hour_bucket < b.end_ms / 3600000;

INSERT INTO usage_analysis_hourly (
  tenant_id, key_id, hour_bucket, source_kind, model, protocol, status_class,
  error_code, upstream_account_id, model_route_id, service_tier, currency, requests,
  input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units,
  duration_count, duration_sum_ms, duration_bucket_0, duration_bucket_1,
  duration_bucket_2, duration_bucket_3, duration_bucket_4, duration_bucket_5,
  duration_bucket_6, duration_bucket_7, duration_bucket_8, duration_bucket_9,
  duration_bucket_10, duration_bucket_11, cost_micros
)
SELECT f.tenant_id, f.key_id, f.created_at / 3600000, 'request', f.model,
       CASE
         WHEN f.protocol = 'anthropic' OR f.protocol LIKE 'anthropic-%' THEN 'anthropic'
         WHEN f.protocol = 'openai-image' THEN 'openai-image'
         ELSE 'openai'
       END,
       f.status_class, f.error_code, f.upstream_account_id, f.model_route_id,
       f.service_tier, f.currency, COUNT(*),
       COALESCE(SUM(CASE
         WHEN f.input_tokens >= f.cached_input_tokens + f.cache_write_tokens
         THEN f.input_tokens - f.cached_input_tokens - f.cache_write_tokens ELSE 0 END), 0),
       COALESCE(SUM(f.output_tokens), 0), COALESCE(SUM(f.cached_input_tokens), 0),
       COALESCE(SUM(f.cache_write_tokens), 0), 0, COUNT(*),
       COALESCE(SUM(f.duration_ms), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms <= 10 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 10 AND f.duration_ms <= 50 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 50 AND f.duration_ms <= 100 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 100 AND f.duration_ms <= 250 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 250 AND f.duration_ms <= 500 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 500 AND f.duration_ms <= 1000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 1000 AND f.duration_ms <= 2500 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 2500 AND f.duration_ms <= 5000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 5000 AND f.duration_ms <= 10000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 10000 AND f.duration_ms <= 30000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 30000 AND f.duration_ms <= 60000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 60000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(f.cost_micros), 0)
  FROM request_stats_facts f
  CROSS JOIN mtc_reconcile_day_bounds b
 WHERE f.created_at >= b.start_ms AND f.created_at < b.end_ms
 GROUP BY f.tenant_id, f.key_id, f.created_at / 3600000, f.model,
          CASE
            WHEN f.protocol = 'anthropic' OR f.protocol LIKE 'anthropic-%' THEN 'anthropic'
            WHEN f.protocol = 'openai-image' THEN 'openai-image'
            ELSE 'openai'
          END,
          f.status_class, f.error_code, f.upstream_account_id, f.model_route_id,
          f.service_tier, f.currency;

INSERT INTO usage_analysis_hourly (
  tenant_id, key_id, hour_bucket, source_kind, model, protocol, status_class,
  error_code, upstream_account_id, model_route_id, service_tier, currency, requests,
  input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units,
  duration_count, duration_sum_ms, duration_bucket_0, duration_bucket_1,
  duration_bucket_2, duration_bucket_3, duration_bucket_4, duration_bucket_5,
  duration_bucket_6, duration_bucket_7, duration_bucket_8, duration_bucket_9,
  duration_bucket_10, duration_bucket_11, cost_micros
)
SELECT f.tenant_id, f.key_id, f.created_at / 3600000, 'generation', f.model,
       'generation', f.status_class, f.error_code, f.upstream_account_id, '',
       'default', f.currency, COUNT(*), 0, 0, 0, 0,
       COALESCE(SUM(f.billed_units), 0), COUNT(*), COALESCE(SUM(f.duration_ms), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms <= 10 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 10 AND f.duration_ms <= 50 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 50 AND f.duration_ms <= 100 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 100 AND f.duration_ms <= 250 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 250 AND f.duration_ms <= 500 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 500 AND f.duration_ms <= 1000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 1000 AND f.duration_ms <= 2500 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 2500 AND f.duration_ms <= 5000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 5000 AND f.duration_ms <= 10000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 10000 AND f.duration_ms <= 30000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 30000 AND f.duration_ms <= 60000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN f.duration_ms > 60000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(f.cost_micros), 0)
  FROM generation_stats_facts f
  CROSS JOIN mtc_reconcile_day_bounds b
 WHERE f.created_at >= b.start_ms AND f.created_at < b.end_ms
 GROUP BY f.tenant_id, f.key_id, f.created_at / 3600000, f.model,
          f.status_class, f.error_code, f.upstream_account_id, f.currency;

DELETE FROM usage_analysis_daily a
 USING mtc_reconcile_day_bounds b
 WHERE a.day_bucket = b.day_bucket;

-- Daily usage is derived only from the bounded hourly projection.
INSERT INTO usage_analysis_daily (
  tenant_id, key_id, day_bucket, source_kind, model, protocol, status_class,
  error_code, upstream_account_id, model_route_id, service_tier, currency, requests,
  input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units,
  duration_count, duration_sum_ms, duration_bucket_0, duration_bucket_1,
  duration_bucket_2, duration_bucket_3, duration_bucket_4, duration_bucket_5,
  duration_bucket_6, duration_bucket_7, duration_bucket_8, duration_bucket_9,
  duration_bucket_10, duration_bucket_11, cost_micros
)
SELECT h.tenant_id, h.key_id, h.hour_bucket / 24, h.source_kind, h.model, h.protocol,
       h.status_class, h.error_code, h.upstream_account_id, h.model_route_id,
       h.service_tier, h.currency, COALESCE(SUM(h.requests), 0),
       COALESCE(SUM(h.input_tokens), 0), COALESCE(SUM(h.output_tokens), 0),
       COALESCE(SUM(h.cached_input_tokens), 0), COALESCE(SUM(h.cache_write_tokens), 0),
       COALESCE(SUM(h.generation_units), 0), COALESCE(SUM(h.duration_count), 0),
       COALESCE(SUM(h.duration_sum_ms), 0), COALESCE(SUM(h.duration_bucket_0), 0),
       COALESCE(SUM(h.duration_bucket_1), 0), COALESCE(SUM(h.duration_bucket_2), 0),
       COALESCE(SUM(h.duration_bucket_3), 0), COALESCE(SUM(h.duration_bucket_4), 0),
       COALESCE(SUM(h.duration_bucket_5), 0), COALESCE(SUM(h.duration_bucket_6), 0),
       COALESCE(SUM(h.duration_bucket_7), 0), COALESCE(SUM(h.duration_bucket_8), 0),
       COALESCE(SUM(h.duration_bucket_9), 0), COALESCE(SUM(h.duration_bucket_10), 0),
       COALESCE(SUM(h.duration_bucket_11), 0), COALESCE(SUM(h.cost_micros), 0)
  FROM usage_analysis_hourly h
  CROSS JOIN mtc_reconcile_day_bounds b
 WHERE h.hour_bucket >= b.start_ms / 3600000
   AND h.hour_bucket < b.end_ms / 3600000
 GROUP BY h.tenant_id, h.key_id, h.hour_bucket / 24, h.source_kind, h.model,
          h.protocol, h.status_class, h.error_code, h.upstream_account_id,
          h.model_route_id, h.service_tier, h.currency;

COMMIT;

ANALYZE request_stats_facts;
ANALYZE request_daily_aggregates;
ANALYZE generation_stats_facts;
ANALYZE generation_daily_aggregates;
ANALYZE usage_analysis_hourly;
ANALYZE usage_analysis_daily;
