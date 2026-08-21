-- A session id is either a proven conversation cluster UUID or the explicit
-- unlinked:<stable-key-id> bucket. Missing evidence is never guessed.
CREATE TABLE session_usage_totals (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    currency TEXT NOT NULL,
    last_activity_at BIGINT NOT NULL,
    requests BIGINT NOT NULL,
    errors BIGINT NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    duration_count BIGINT NOT NULL,
    duration_sum_ms BIGINT NOT NULL,
    cost_micros BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, key_id, session_id, currency)
);

CREATE TABLE session_usage_hourly (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    hour_bucket BIGINT NOT NULL,
    model TEXT NOT NULL,
    protocol TEXT NOT NULL,
    status_class TEXT NOT NULL,
    error_code TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    currency TEXT NOT NULL,
    requests BIGINT NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    duration_count BIGINT NOT NULL,
    duration_sum_ms BIGINT NOT NULL,
    cost_micros BIGINT NOT NULL,
    PRIMARY KEY (
        tenant_id, key_id, session_id, hour_bucket, model, protocol,
        status_class, error_code, upstream_account_id, model_route_id, currency
    )
);

CREATE TABLE session_usage_daily (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    day_bucket BIGINT NOT NULL,
    model TEXT NOT NULL,
    protocol TEXT NOT NULL,
    status_class TEXT NOT NULL,
    error_code TEXT NOT NULL,
    upstream_account_id TEXT NOT NULL,
    model_route_id TEXT NOT NULL,
    currency TEXT NOT NULL,
    requests BIGINT NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    duration_count BIGINT NOT NULL,
    duration_sum_ms BIGINT NOT NULL,
    cost_micros BIGINT NOT NULL,
    PRIMARY KEY (
        tenant_id, key_id, session_id, day_bucket, model, protocol,
        status_class, error_code, upstream_account_id, model_route_id, currency
    )
);

INSERT INTO session_usage_totals (
    tenant_id, key_id, session_id, currency, last_activity_at, requests,
    errors, input_tokens, output_tokens, duration_count, duration_sum_ms,
    cost_micros
)
SELECT fact.tenant_id, fact.key_id,
       COALESCE(record.conversation_cluster_id, 'unlinked:' || fact.key_id),
       fact.currency, MAX(fact.created_at), COUNT(*),
       SUM(CASE WHEN fact.status_class = 'failure' THEN 1 ELSE 0 END),
       SUM(fact.input_tokens), SUM(fact.output_tokens), COUNT(*),
       SUM(fact.duration_ms), SUM(fact.cost_micros)
  FROM request_stats_facts fact
  JOIN request_records record
    ON record.id = fact.request_id AND record.created_at = fact.created_at
 GROUP BY fact.tenant_id, fact.key_id,
          COALESCE(record.conversation_cluster_id, 'unlinked:' || fact.key_id),
          fact.currency;

INSERT INTO session_usage_hourly (
    tenant_id, key_id, session_id, hour_bucket, model, protocol, status_class,
    error_code, upstream_account_id, model_route_id, currency, requests,
    input_tokens, output_tokens, duration_count, duration_sum_ms, cost_micros
)
SELECT fact.tenant_id, fact.key_id,
       COALESCE(record.conversation_cluster_id, 'unlinked:' || fact.key_id),
       fact.created_at / 3600000, fact.model,
       CASE WHEN fact.protocol = 'anthropic' OR fact.protocol LIKE 'anthropic-%'
            THEN 'anthropic'
            WHEN fact.protocol = 'openai-image' THEN 'openai-image'
            ELSE 'openai' END,
       fact.status_class,
       fact.error_code, fact.upstream_account_id, fact.model_route_id,
       fact.currency, COUNT(*), SUM(fact.input_tokens), SUM(fact.output_tokens),
       COUNT(*), SUM(fact.duration_ms), SUM(fact.cost_micros)
  FROM request_stats_facts fact
  JOIN request_records record
    ON record.id = fact.request_id AND record.created_at = fact.created_at
 GROUP BY fact.tenant_id, fact.key_id,
          COALESCE(record.conversation_cluster_id, 'unlinked:' || fact.key_id),
          fact.created_at / 3600000, fact.model,
          CASE WHEN fact.protocol = 'anthropic' OR fact.protocol LIKE 'anthropic-%'
               THEN 'anthropic'
               WHEN fact.protocol = 'openai-image' THEN 'openai-image'
               ELSE 'openai' END,
          fact.status_class, fact.error_code, fact.upstream_account_id,
          fact.model_route_id, fact.currency;

INSERT INTO session_usage_daily (
    tenant_id, key_id, session_id, day_bucket, model, protocol, status_class,
    error_code, upstream_account_id, model_route_id, currency, requests,
    input_tokens, output_tokens, duration_count, duration_sum_ms, cost_micros
)
SELECT fact.tenant_id, fact.key_id,
       COALESCE(record.conversation_cluster_id, 'unlinked:' || fact.key_id),
       fact.created_at / 86400000, fact.model,
       CASE WHEN fact.protocol = 'anthropic' OR fact.protocol LIKE 'anthropic-%'
            THEN 'anthropic'
            WHEN fact.protocol = 'openai-image' THEN 'openai-image'
            ELSE 'openai' END,
       fact.status_class,
       fact.error_code, fact.upstream_account_id, fact.model_route_id,
       fact.currency, COUNT(*), SUM(fact.input_tokens), SUM(fact.output_tokens),
       COUNT(*), SUM(fact.duration_ms), SUM(fact.cost_micros)
  FROM request_stats_facts fact
  JOIN request_records record
    ON record.id = fact.request_id AND record.created_at = fact.created_at
 GROUP BY fact.tenant_id, fact.key_id,
          COALESCE(record.conversation_cluster_id, 'unlinked:' || fact.key_id),
          fact.created_at / 86400000, fact.model,
          CASE WHEN fact.protocol = 'anthropic' OR fact.protocol LIKE 'anthropic-%'
               THEN 'anthropic'
               WHEN fact.protocol = 'openai-image' THEN 'openai-image'
               ELSE 'openai' END,
          fact.status_class, fact.error_code, fact.upstream_account_id,
          fact.model_route_id, fact.currency;

CREATE INDEX session_usage_totals_tenant_activity_idx
    ON session_usage_totals (tenant_id, last_activity_at DESC, session_id, key_id);
CREATE INDEX session_usage_totals_key_activity_idx
    ON session_usage_totals (key_id, last_activity_at DESC, session_id);
CREATE INDEX session_usage_hourly_tenant_time_idx
    ON session_usage_hourly (tenant_id, hour_bucket);
CREATE INDEX session_usage_hourly_key_time_idx
    ON session_usage_hourly (key_id, hour_bucket);
CREATE INDEX session_usage_daily_tenant_time_idx
    ON session_usage_daily (tenant_id, day_bucket);
CREATE INDEX session_usage_daily_key_time_idx
    ON session_usage_daily (key_id, day_bucket);
