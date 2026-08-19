\set ON_ERROR_STOP on

-- Deterministic, synthetic, secret-free scale fixture for postgres_explain.py.
-- Run only against a disposable database after all application migrations.
SET synchronous_commit = off;

INSERT INTO tenants (id, external_id, created_at)
VALUES ('10000000-0000-0000-0000-000000000001', 'scale-fixture', 0);

INSERT INTO principals (id, tenant_id, external_id, created_at)
VALUES (
  '10000000-0000-0000-0000-000000000002',
  '10000000-0000-0000-0000-000000000001',
  'scale-principal',
  0
);

INSERT INTO credit_accounts (
  id, tenant_id, principal_id, currency, available_micros, reserved_micros,
  created_at, updated_at
)
VALUES (
  '10000000-0000-0000-0000-000000000003',
  '10000000-0000-0000-0000-000000000001',
  '10000000-0000-0000-0000-000000000002',
  'USD', 1000000000000, 0, 0, 0
);

INSERT INTO key_records (
  id, tenant_id, principal_id, account_id, alias, currency, policy_json,
  status, credential_generation, created_at, updated_at
)
VALUES (
  '10000000-0000-0000-0000-000000000004',
  '10000000-0000-0000-0000-000000000001',
  '10000000-0000-0000-0000-000000000002',
  '10000000-0000-0000-0000-000000000003',
  'scale-key', 'USD', '{}', 'active', 1, 0, 0
);

INSERT INTO upstream_accounts (
  id, tenant_id, name, driver, auth_kind, config_json, status,
  credential_generation, created_at, updated_at
)
VALUES (
  '10000000-0000-0000-0000-000000000005',
  '10000000-0000-0000-0000-000000000001',
  'scale-upstream', 'openai', 'api_key', '{}', 'active', 1, 0, 0
);

WITH bounds AS (
  SELECT (extract(epoch FROM date_trunc('day', now() AT TIME ZONE 'UTC')) * 1000)::bigint AS day_start
)
INSERT INTO request_records (
  id, tenant_id, key_id, created_at, completed_at, protocol, model,
  status_code, duration_ms, input_tokens, output_tokens, cost_micros,
  error_code, request_object, response_object, reservation_id,
  upstream_account_id, model_route_id, cached_input_tokens,
  cache_write_tokens, service_tier, currency
)
SELECT '20000000-0000-0000-0000-' || lpad(n::text, 12, '0'),
       '10000000-0000-0000-0000-000000000001',
       '10000000-0000-0000-0000-000000000004',
       day_start + (n % 86400) * 1000,
       day_start + (n % 86400) * 1000 + 50,
       CASE WHEN n % 5 = 0 THEN 'anthropic' ELSE 'openai' END,
       format('scale-model-%s', n % 20),
       CASE WHEN n % 10 = 0 THEN 502 ELSE 200 END,
       50 + n % 5000,
       100 + n % 200,
       25 + n % 100,
       1000 + n % 500,
       CASE WHEN n % 10 = 0 THEN 'upstream_timeout' END,
       'objects/scale-request',
       'objects/scale-response',
       '30000000-0000-0000-0000-' || lpad(n::text, 12, '0'),
       '10000000-0000-0000-0000-000000000005',
       CASE WHEN n % 3 = 0
         THEN '10000000-0000-0000-0000-000000000006'
       END,
       10, 5, 'default', 'USD'
  FROM generate_series(1, 100000) AS fixture(n)
 CROSS JOIN bounds;

INSERT INTO request_events (
  event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol,
  model, status_code, duration_ms, input_tokens, output_tokens, cost_micros,
  error_code
)
SELECT '40000000-0000-0000-0000-' ||
       lpad(row_number() OVER (ORDER BY created_at, id)::text, 12, '0'),
       tenant_id, key_id, id, completed_at, 'completed', protocol, model,
       status_code, duration_ms, input_tokens, output_tokens, cost_micros,
       error_code
  FROM request_records
 WHERE tenant_id = '10000000-0000-0000-0000-000000000001';

INSERT INTO request_stats_facts (
  request_id, tenant_id, key_id, created_at, model, protocol, status_class,
  error_code, upstream_account_id, model_route_id, duration_ms, input_tokens,
  output_tokens, cached_input_tokens, cache_write_tokens, service_tier,
  currency, cost_micros
)
SELECT id, tenant_id, key_id, created_at, model, protocol,
       CASE WHEN status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END,
       COALESCE(error_code, ''), COALESCE(upstream_account_id, ''),
       COALESCE(model_route_id, ''), duration_ms, input_tokens, output_tokens,
       cached_input_tokens, cache_write_tokens, service_tier, currency,
       cost_micros
  FROM request_records
 WHERE tenant_id = '10000000-0000-0000-0000-000000000001';

INSERT INTO usage_daily_aggregates (
  key_id, day_bucket, model, status_class, error_code, requests,
  input_tokens, output_tokens, cost_micros
)
SELECT key_id, created_at / 86400000, model, status_class, error_code,
       count(*), sum(input_tokens), sum(output_tokens), sum(cost_micros)
  FROM request_stats_facts
 WHERE tenant_id = '10000000-0000-0000-0000-000000000001'
 GROUP BY key_id, created_at / 86400000, model, status_class, error_code;

INSERT INTO request_daily_aggregates (
  tenant_id, key_id, day_bucket, model, protocol, status_class, error_code,
  upstream_account_id, model_route_id, service_tier, currency, requests,
  input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
  duration_count, duration_sum_ms, cost_micros
)
SELECT tenant_id, key_id, created_at / 86400000, model, protocol,
       status_class, error_code, upstream_account_id, model_route_id,
       service_tier, currency, count(*), sum(input_tokens), sum(output_tokens),
       sum(cached_input_tokens), sum(cache_write_tokens), count(*),
       sum(duration_ms), sum(cost_micros)
  FROM request_stats_facts
 WHERE tenant_id = '10000000-0000-0000-0000-000000000001'
 GROUP BY tenant_id, key_id, created_at / 86400000, model, protocol,
          status_class, error_code, upstream_account_id, model_route_id,
          service_tier, currency;

INSERT INTO usage_analysis_hourly (
  tenant_id, key_id, hour_bucket, source_kind, model, protocol, status_class,
  error_code, upstream_account_id, model_route_id, service_tier, currency,
  requests, input_tokens, output_tokens, cached_input_tokens,
  cache_write_tokens, generation_units, duration_count, duration_sum_ms,
  duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3,
  duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7,
  duration_bucket_8, duration_bucket_9, duration_bucket_10,
  duration_bucket_11, cost_micros
)
SELECT tenant_id, key_id, created_at / 3600000, 'request', model,
       CASE WHEN protocol = 'anthropic' THEN 'anthropic' ELSE 'openai' END,
       status_class, error_code, upstream_account_id, model_route_id,
       service_tier, currency, count(*),
       sum(input_tokens - cached_input_tokens - cache_write_tokens),
       sum(output_tokens), sum(cached_input_tokens), sum(cache_write_tokens),
       0, count(*), sum(duration_ms),
       sum(CASE WHEN duration_ms <= 10 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 10 AND duration_ms <= 50 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 50 AND duration_ms <= 100 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 100 AND duration_ms <= 250 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 250 AND duration_ms <= 500 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 500 AND duration_ms <= 1000 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 1000 AND duration_ms <= 2500 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 2500 AND duration_ms <= 5000 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 5000 AND duration_ms <= 10000 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 10000 AND duration_ms <= 30000 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 30000 AND duration_ms <= 60000 THEN 1 ELSE 0 END),
       sum(CASE WHEN duration_ms > 60000 THEN 1 ELSE 0 END),
       sum(cost_micros)
  FROM request_stats_facts
 WHERE tenant_id = '10000000-0000-0000-0000-000000000001'
 GROUP BY tenant_id, key_id, created_at / 3600000, model, protocol,
          status_class, error_code, upstream_account_id, model_route_id,
          service_tier, currency;

INSERT INTO usage_analysis_daily (
  tenant_id, key_id, day_bucket, source_kind, model, protocol, status_class,
  error_code, upstream_account_id, model_route_id, service_tier, currency,
  requests, input_tokens, output_tokens, cached_input_tokens,
  cache_write_tokens, generation_units, duration_count, duration_sum_ms,
  duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3,
  duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7,
  duration_bucket_8, duration_bucket_9, duration_bucket_10,
  duration_bucket_11, cost_micros
)
SELECT tenant_id, key_id, hour_bucket / 24, source_kind, model, protocol,
       status_class, error_code, upstream_account_id, model_route_id,
       service_tier, currency, sum(requests), sum(input_tokens),
       sum(output_tokens), sum(cached_input_tokens), sum(cache_write_tokens),
       sum(generation_units), sum(duration_count), sum(duration_sum_ms),
       sum(duration_bucket_0), sum(duration_bucket_1), sum(duration_bucket_2),
       sum(duration_bucket_3), sum(duration_bucket_4), sum(duration_bucket_5),
       sum(duration_bucket_6), sum(duration_bucket_7), sum(duration_bucket_8),
       sum(duration_bucket_9), sum(duration_bucket_10),
       sum(duration_bucket_11), sum(cost_micros)
  FROM usage_analysis_hourly
 WHERE tenant_id = '10000000-0000-0000-0000-000000000001'
 GROUP BY tenant_id, key_id, hour_bucket / 24, source_kind, model, protocol,
          status_class, error_code, upstream_account_id, model_route_id,
          service_tier, currency;

ANALYZE request_records;
ANALYZE request_events;
ANALYZE request_stats_facts;
ANALYZE request_daily_aggregates;
ANALYZE usage_daily_aggregates;
ANALYZE usage_analysis_hourly;
ANALYZE usage_analysis_daily;
