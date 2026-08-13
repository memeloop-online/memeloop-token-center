#!/bin/sh
set -eu

: "${DATABASE_URL:?DATABASE_URL is required}"
: "${CPAMP_SQLITE_PATH:=/source/usage.sqlite}"
: "${IMPORT_TENANT_EXTERNAL_ID:=cpa-dogfood-import}"

psql_target() {
  psql -X -v ON_ERROR_STOP=1 --no-psqlrc "$DATABASE_URL" "$@"
}

psql_target <<'SQL'
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_usage (
  event_hash text, request_id text, timestamp_ms bigint, provider text,
  model text, endpoint text, api_key_hash text, input_tokens bigint,
  output_tokens bigint, latency_ms bigint, failed bigint,
  fail_status_code bigint, fail_summary text
);
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_aliases (
  api_key_hash text, alias text, updated_at_ms bigint
);
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_prices (
  model text, prompt_per_1m double precision, completion_per_1m double precision,
  source text, updated_at_ms bigint
);
TRUNCATE cpamp_import_usage, cpamp_import_aliases, cpamp_import_prices;
SQL

sqlite3 -header -csv "$CPAMP_SQLITE_PATH" \
  "SELECT event_hash, request_id, timestamp_ms, provider, model, endpoint,
          api_key_hash, input_tokens, output_tokens, latency_ms,
          CASE WHEN failed THEN 1 ELSE 0 END,
          COALESCE(fail_status_code, 0), COALESCE(fail_summary, '')
     FROM usage_events
    WHERE event_hash <> '';" \
  | psql_target -c "\\copy cpamp_import_usage FROM STDIN WITH (FORMAT csv, HEADER true)"

sqlite3 -header -csv "$CPAMP_SQLITE_PATH" \
  "SELECT api_key_hash, alias, updated_at_ms FROM api_key_aliases;" \
  | psql_target -c "\\copy cpamp_import_aliases FROM STDIN WITH (FORMAT csv, HEADER true)"

sqlite3 -header -csv "$CPAMP_SQLITE_PATH" \
  "SELECT model, prompt_per_1m, completion_per_1m, source, updated_at_ms
     FROM model_prices;" \
  | psql_target -c "\\copy cpamp_import_prices FROM STDIN WITH (FORMAT csv, HEADER true)"

psql_target -v tenant_external_id="$IMPORT_TENANT_EXTERNAL_ID" <<'SQL'
BEGIN;

INSERT INTO tenants (id, external_id, created_at)
VALUES ('tenant-cpamp-import', :'tenant_external_id',
        (extract(epoch from clock_timestamp()) * 1000)::bigint)
ON CONFLICT (external_id) DO NOTHING;

INSERT INTO principals (id, tenant_id, external_id, created_at)
SELECT 'principal-cpamp-import', id, 'cpamp-import',
       (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM tenants WHERE external_id = :'tenant_external_id'
ON CONFLICT (tenant_id, external_id) DO NOTHING;

WITH hashes AS (
  SELECT DISTINCT api_key_hash FROM cpamp_import_usage
   WHERE length(api_key_hash) = 64
), identities AS (
  SELECT h.api_key_hash,
         'cpamp-account-' || substr(h.api_key_hash, 1, 24) AS account_id,
         'cpamp-key-' || substr(h.api_key_hash, 1, 24) AS key_id,
         COALESCE(NULLIF(a.alias, ''), 'CPA ' || substr(h.api_key_hash, 1, 8)) AS alias
    FROM hashes h LEFT JOIN cpamp_import_aliases a USING (api_key_hash)
)
INSERT INTO credit_accounts
  (id, tenant_id, principal_id, currency, available_micros, reserved_micros,
   created_at, updated_at)
SELECT i.account_id, t.id, p.id, 'USD', 0, 0,
       (extract(epoch from clock_timestamp()) * 1000)::bigint,
       (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM identities i
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  JOIN principals p ON p.tenant_id = t.id AND p.external_id = 'cpamp-import'
ON CONFLICT (id) DO NOTHING;

WITH hashes AS (
  SELECT DISTINCT api_key_hash FROM cpamp_import_usage
   WHERE length(api_key_hash) = 64
), identities AS (
  SELECT h.api_key_hash,
         'cpamp-account-' || substr(h.api_key_hash, 1, 24) AS account_id,
         'cpamp-key-' || substr(h.api_key_hash, 1, 24) AS key_id,
         COALESCE(NULLIF(a.alias, ''), 'CPA ' || substr(h.api_key_hash, 1, 8)) AS alias
    FROM hashes h LEFT JOIN cpamp_import_aliases a USING (api_key_hash)
)
INSERT INTO key_records
  (id, tenant_id, principal_id, account_id, alias, currency, policy_json,
   status, credential_generation, created_at, updated_at)
SELECT i.key_id, t.id, p.id, i.account_id, i.alias, 'USD',
       '{"allowed_models":["*"],"requests_per_minute":60,"tokens_per_minute":1000000,"max_concurrency":4}',
       'active', 0,
       (extract(epoch from clock_timestamp()) * 1000)::bigint,
       (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM identities i
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  JOIN principals p ON p.tenant_id = t.id AND p.external_id = 'cpamp-import'
ON CONFLICT (id) DO UPDATE SET alias = excluded.alias, updated_at = excluded.updated_at;

INSERT INTO model_prices
  (id, model, currency, input_micros_per_million,
   output_micros_per_million, source, updated_at)
SELECT 'cpamp-price-' || substr(md5(model), 1, 24), model, 'USD',
       round(COALESCE(prompt_per_1m, 0) * 1000000)::bigint,
       round(COALESCE(completion_per_1m, 0) * 1000000)::bigint,
       'cpamp:' || COALESCE(NULLIF(source, ''), 'import'), updated_at_ms
  FROM cpamp_import_prices WHERE model <> ''
ON CONFLICT (model, currency) DO UPDATE SET
  input_micros_per_million = excluded.input_micros_per_million,
  output_micros_per_million = excluded.output_micros_per_million,
  source = excluded.source, updated_at = excluded.updated_at;

INSERT INTO request_records
  (id, tenant_id, key_id, created_at, completed_at, protocol, model,
   status_code, duration_ms, input_tokens, output_tokens, cost_micros,
   error_code, request_object, response_object, reservation_id)
SELECT 'cpamp:' || u.event_hash, t.id,
       'cpamp-key-' || substr(u.api_key_hash, 1, 24), u.timestamp_ms,
       u.timestamp_ms + GREATEST(COALESCE(u.latency_ms, 0), 0),
       COALESCE(NULLIF(u.provider, ''), 'openai'), COALESCE(NULLIF(u.model, ''), '-'),
       CASE WHEN u.failed <> 0 THEN NULLIF(u.fail_status_code, 0) ELSE 200 END,
       GREATEST(COALESCE(u.latency_ms, 0), 0),
       GREATEST(COALESCE(u.input_tokens, 0), 0),
       GREATEST(COALESCE(u.output_tokens, 0), 0),
       round((GREATEST(COALESCE(u.input_tokens, 0), 0) * COALESCE(p.input_micros_per_million, 0)
            + GREATEST(COALESCE(u.output_tokens, 0), 0) * COALESCE(p.output_micros_per_million, 0)) / 1000000.0)::bigint,
       CASE WHEN u.failed <> 0 THEN COALESCE(NULLIF(u.fail_summary, ''), 'upstream_error') END,
       jsonb_build_object('source', 'cpamp', 'request_id', u.request_id,
                          'endpoint', u.endpoint)::text,
       CASE WHEN u.failed <> 0 THEN jsonb_build_object('error', u.fail_summary)::text END,
       'cpamp-import:' || u.event_hash
  FROM cpamp_import_usage u
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  LEFT JOIN model_prices p ON p.model = u.model AND p.currency = 'USD'
 WHERE length(u.api_key_hash) = 64
   AND NOT EXISTS (SELECT 1 FROM request_records r WHERE r.id = 'cpamp:' || u.event_hash);

INSERT INTO usage_daily_aggregates
  (key_id, day_bucket, model, status_class, error_code, requests,
   input_tokens, output_tokens, cost_micros)
SELECT key_id, created_at / 86400000, model,
       CASE WHEN status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END,
       COALESCE(error_code, ''), count(*), sum(input_tokens), sum(output_tokens), sum(cost_micros)
  FROM request_records
 WHERE id LIKE 'cpamp:%'
 GROUP BY key_id, created_at / 86400000, model,
          CASE WHEN status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END,
          COALESCE(error_code, '')
ON CONFLICT (key_id, day_bucket, model, status_class, error_code) DO UPDATE SET
  requests = excluded.requests, input_tokens = excluded.input_tokens,
  output_tokens = excluded.output_tokens, cost_micros = excluded.cost_micros;

COMMIT;
ANALYZE request_records;
ANALYZE usage_daily_aggregates;
DROP TABLE cpamp_import_usage, cpamp_import_aliases, cpamp_import_prices;
SQL
