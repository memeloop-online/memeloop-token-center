ALTER TABLE session_usage_totals
    ADD COLUMN cached_input_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE session_usage_totals
    ADD COLUMN cache_write_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE session_usage_totals
    ADD COLUMN generation_units BIGINT NOT NULL DEFAULT 0;

ALTER TABLE session_usage_hourly
    ADD COLUMN cached_input_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE session_usage_hourly
    ADD COLUMN cache_write_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE session_usage_hourly
    ADD COLUMN generation_units BIGINT NOT NULL DEFAULT 0;

ALTER TABLE session_usage_daily
    ADD COLUMN cached_input_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE session_usage_daily
    ADD COLUMN cache_write_tokens BIGINT NOT NULL DEFAULT 0;
ALTER TABLE session_usage_daily
    ADD COLUMN generation_units BIGINT NOT NULL DEFAULT 0;

-- Rebuild the derived projections transactionally. Request input is normalized
-- to uncached input here, matching the other usage-analysis dimensions.
DELETE FROM session_usage_totals;
DELETE FROM session_usage_hourly;
DELETE FROM session_usage_daily;

INSERT INTO session_usage_totals (
    tenant_id, key_id, session_id, currency, last_activity_at, requests,
    errors, input_tokens, output_tokens, cached_input_tokens,
    cache_write_tokens, generation_units, duration_count, duration_sum_ms,
    cost_micros
)
SELECT tenant_id, key_id, session_id, currency, MAX(created_at), COUNT(*),
       SUM(CASE WHEN status_class = 'failure' THEN 1 ELSE 0 END),
       SUM(CASE
               WHEN input_tokens >= cached_input_tokens + cache_write_tokens
               THEN input_tokens - cached_input_tokens - cache_write_tokens
               ELSE 0
           END),
       SUM(output_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens), 0,
       COUNT(*), SUM(duration_ms), SUM(cost_micros)
  FROM request_stats_facts
 GROUP BY tenant_id, key_id, session_id, currency;

INSERT INTO session_usage_totals (
    tenant_id, key_id, session_id, currency, last_activity_at, requests,
    errors, input_tokens, output_tokens, cached_input_tokens,
    cache_write_tokens, generation_units, duration_count, duration_sum_ms,
    cost_micros
)
SELECT tenant_id, key_id, 'unlinked:' || key_id, currency, MAX(created_at),
       COUNT(*), SUM(CASE WHEN status_class = 'failure' THEN 1 ELSE 0 END),
       0, 0, 0, 0, SUM(billed_units), COUNT(*), SUM(duration_ms),
       SUM(cost_micros)
  FROM generation_stats_facts
 GROUP BY tenant_id, key_id, currency
ON CONFLICT (tenant_id, key_id, session_id, currency) DO UPDATE SET
    last_activity_at = CASE
        WHEN session_usage_totals.last_activity_at < excluded.last_activity_at
        THEN excluded.last_activity_at ELSE session_usage_totals.last_activity_at END,
    requests = session_usage_totals.requests + excluded.requests,
    errors = session_usage_totals.errors + excluded.errors,
    generation_units = session_usage_totals.generation_units + excluded.generation_units,
    duration_count = session_usage_totals.duration_count + excluded.duration_count,
    duration_sum_ms = session_usage_totals.duration_sum_ms + excluded.duration_sum_ms,
    cost_micros = session_usage_totals.cost_micros + excluded.cost_micros;

INSERT INTO session_usage_hourly (
    tenant_id, key_id, session_id, hour_bucket, model, protocol, status_class,
    error_code, upstream_account_id, model_route_id, currency, requests,
    input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
    generation_units, duration_count, duration_sum_ms, cost_micros
)
SELECT tenant_id, key_id, session_id, created_at / 3600000, model,
       CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
            THEN 'anthropic'
            WHEN protocol = 'openai-image' THEN 'openai-image'
            ELSE 'openai' END,
       status_class, error_code, upstream_account_id, model_route_id, currency,
       COUNT(*),
       SUM(CASE
               WHEN input_tokens >= cached_input_tokens + cache_write_tokens
               THEN input_tokens - cached_input_tokens - cache_write_tokens
               ELSE 0
           END),
       SUM(output_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens), 0,
       COUNT(*), SUM(duration_ms), SUM(cost_micros)
  FROM request_stats_facts
 GROUP BY tenant_id, key_id, session_id, created_at / 3600000, model,
          CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
               THEN 'anthropic'
               WHEN protocol = 'openai-image' THEN 'openai-image'
               ELSE 'openai' END,
          status_class, error_code, upstream_account_id, model_route_id, currency;

INSERT INTO session_usage_hourly (
    tenant_id, key_id, session_id, hour_bucket, model, protocol, status_class,
    error_code, upstream_account_id, model_route_id, currency, requests,
    input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
    generation_units, duration_count, duration_sum_ms, cost_micros
)
SELECT tenant_id, key_id, 'unlinked:' || key_id, created_at / 3600000,
       model, 'generation', status_class, error_code, upstream_account_id,
       model_route_id, currency, COUNT(*), 0, 0, 0, 0, SUM(billed_units),
       COUNT(*), SUM(duration_ms), SUM(cost_micros)
  FROM generation_stats_facts
 GROUP BY tenant_id, key_id, created_at / 3600000, model, status_class,
          error_code, upstream_account_id, model_route_id, currency;

INSERT INTO session_usage_daily (
    tenant_id, key_id, session_id, day_bucket, model, protocol, status_class,
    error_code, upstream_account_id, model_route_id, currency, requests,
    input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
    generation_units, duration_count, duration_sum_ms, cost_micros
)
SELECT tenant_id, key_id, session_id, created_at / 86400000, model,
       CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
            THEN 'anthropic'
            WHEN protocol = 'openai-image' THEN 'openai-image'
            ELSE 'openai' END,
       status_class, error_code, upstream_account_id, model_route_id, currency,
       COUNT(*),
       SUM(CASE
               WHEN input_tokens >= cached_input_tokens + cache_write_tokens
               THEN input_tokens - cached_input_tokens - cache_write_tokens
               ELSE 0
           END),
       SUM(output_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens), 0,
       COUNT(*), SUM(duration_ms), SUM(cost_micros)
  FROM request_stats_facts
 GROUP BY tenant_id, key_id, session_id, created_at / 86400000, model,
          CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
               THEN 'anthropic'
               WHEN protocol = 'openai-image' THEN 'openai-image'
               ELSE 'openai' END,
          status_class, error_code, upstream_account_id, model_route_id, currency;

INSERT INTO session_usage_daily (
    tenant_id, key_id, session_id, day_bucket, model, protocol, status_class,
    error_code, upstream_account_id, model_route_id, currency, requests,
    input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
    generation_units, duration_count, duration_sum_ms, cost_micros
)
SELECT tenant_id, key_id, 'unlinked:' || key_id, created_at / 86400000,
       model, 'generation', status_class, error_code, upstream_account_id,
       model_route_id, currency, COUNT(*), 0, 0, 0, 0, SUM(billed_units),
       COUNT(*), SUM(duration_ms), SUM(cost_micros)
  FROM generation_stats_facts
 GROUP BY tenant_id, key_id, created_at / 86400000, model, status_class,
          error_code, upstream_account_id, model_route_id, currency;
