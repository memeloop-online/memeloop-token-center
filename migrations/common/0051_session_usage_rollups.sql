-- A session id is either a proven conversation cluster UUID or the explicit
-- unlinked:<stable-key-id> bucket. Missing evidence is never guessed.
ALTER TABLE request_stats_facts
    ADD COLUMN session_id TEXT NOT NULL DEFAULT '';

UPDATE request_stats_facts
   SET session_id = COALESCE((
       SELECT record.conversation_cluster_id
         FROM request_records record
        WHERE record.id = request_stats_facts.request_id
          AND record.created_at = request_stats_facts.created_at
   ), 'unlinked:' || key_id);

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
SELECT fact.tenant_id, fact.key_id, fact.session_id,
       fact.currency, MAX(fact.created_at), COUNT(*),
       SUM(CASE WHEN fact.status_class = 'failure' THEN 1 ELSE 0 END),
       SUM(fact.input_tokens), SUM(fact.output_tokens), COUNT(*),
       SUM(fact.duration_ms), SUM(fact.cost_micros)
  FROM request_stats_facts fact
 GROUP BY fact.tenant_id, fact.key_id, fact.session_id, fact.currency;

INSERT INTO session_usage_hourly (
    tenant_id, key_id, session_id, hour_bucket, model, protocol, status_class,
    error_code, upstream_account_id, model_route_id, currency, requests,
    input_tokens, output_tokens, duration_count, duration_sum_ms, cost_micros
)
SELECT fact.tenant_id, fact.key_id, fact.session_id,
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
 GROUP BY fact.tenant_id, fact.key_id, fact.session_id,
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
SELECT fact.tenant_id, fact.key_id, fact.session_id,
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
 GROUP BY fact.tenant_id, fact.key_id, fact.session_id,
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
CREATE INDEX session_usage_hourly_session_model_idx
    ON session_usage_hourly (tenant_id, key_id, session_id, model);
CREATE INDEX session_usage_daily_tenant_time_idx
    ON session_usage_daily (tenant_id, day_bucket);
CREATE INDEX session_usage_daily_key_time_idx
    ON session_usage_daily (key_id, day_bucket);

CREATE INDEX request_stats_facts_session_currency_time_idx
    ON request_stats_facts
       (tenant_id, key_id, session_id, currency, created_at DESC, request_id DESC);

-- Archive-only records are diagnostic, never billable. This compact projection
-- makes them discoverable without changing any authoritative usage dimension.
CREATE TABLE session_archive_totals (
    tenant_id TEXT NOT NULL,
    key_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    last_activity_at BIGINT NOT NULL,
    requests BIGINT NOT NULL,
    errors BIGINT NOT NULL,
    input_tokens BIGINT NOT NULL,
    output_tokens BIGINT NOT NULL,
    duration_count BIGINT NOT NULL,
    duration_sum_ms BIGINT NOT NULL,
    PRIMARY KEY (tenant_id, key_id, session_id)
);

INSERT INTO session_archive_totals (
    tenant_id, key_id, session_id, last_activity_at, requests, errors,
    input_tokens, output_tokens, duration_count, duration_sum_ms
)
SELECT tenant_id, key_id,
       COALESCE(conversation_cluster_id, 'unlinked:' || key_id),
       MAX(source_started_at), COUNT(*),
       SUM(CASE WHEN status_code IS NOT NULL
                     AND (status_code < 200 OR status_code >= 400)
                THEN 1 ELSE 0 END),
       SUM(input_tokens), SUM(output_tokens),
       SUM(CASE WHEN duration_ms IS NULL THEN 0 ELSE 1 END),
       SUM(COALESCE(duration_ms, 0))
  FROM session_archive_unlinked_requests
 GROUP BY tenant_id, key_id,
          COALESCE(conversation_cluster_id, 'unlinked:' || key_id);

CREATE INDEX session_archive_totals_tenant_activity_idx
    ON session_archive_totals
       (tenant_id, last_activity_at DESC, session_id DESC, key_id);
CREATE INDEX session_archive_totals_key_activity_idx
    ON session_archive_totals
       (key_id, last_activity_at DESC, session_id DESC);

CREATE INDEX request_records_active_session_cursor_idx
    ON request_records
       (tenant_id, key_id, conversation_cluster_id, created_at DESC, id DESC)
    WHERE status_code IS NULL;
