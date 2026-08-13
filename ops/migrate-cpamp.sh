#!/bin/sh
set -eu

: "${PGDATABASE:=${DATABASE_URL:-}}"
: "${PGDATABASE:?PGDATABASE or DATABASE_URL is required}"
: "${CPAMP_SQLITE_PATH:=/source/usage.sqlite}"
: "${IMPORT_TENANT_EXTERNAL_ID:=cpa-dogfood-import}"
: "${CPAMP_OVERLAP_MS:=86400000}"
: "${CPAMP_RESET_IMPORT:=false}"
: "${CPAMP_RESET_CONFIRM:=}"

case "$CPAMP_OVERLAP_MS" in *[!0-9]*|'') echo "CPAMP_OVERLAP_MS must be an integer" >&2; exit 2;; esac
case "$CPAMP_RESET_IMPORT" in true|false) ;; *) echo "CPAMP_RESET_IMPORT must be true or false" >&2; exit 2;; esac
[ "$CPAMP_RESET_IMPORT" = false ] || [ "$IMPORT_TENANT_EXTERNAL_ID" = "cpa-dogfood-import" ] || {
  echo "reset is only allowed for the cpa-dogfood-import tenant" >&2
  exit 2
}
[ "$CPAMP_RESET_IMPORT" = false ] || [ "$CPAMP_RESET_CONFIRM" = "DELETE_CPA_DOGFOOD_IMPORT" ] || {
  echo "CPAMP_RESET_CONFIRM=DELETE_CPA_DOGFOOD_IMPORT is required for a reset" >&2
  exit 2
}
[ -r "$CPAMP_SQLITE_PATH" ] || { echo "CPAMP SQLite database is not readable" >&2; exit 2; }

# libpq expands a connection URI supplied through PGDATABASE. The Kubernetes
# Job injects that standard variable directly; DATABASE_URL remains a local
# compatibility fallback. Keeping the URI out of argv prevents credentials
# from appearing in process listings.
export PGDATABASE
unset DATABASE_URL

psql_target() {
  psql -X -v ON_ERROR_STOP=1 --no-psqlrc "$@"
}

psql_target <<'SQL'
CREATE TABLE IF NOT EXISTS cpamp_import_checkpoints (
  source text PRIMARY KEY,
  watermark_ms bigint NOT NULL,
  watermark_hash text NOT NULL,
  imported_events bigint NOT NULL,
  updated_at bigint NOT NULL
);
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
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_identities (
  api_key_hash text PRIMARY KEY, key_id text NOT NULL, account_id text NOT NULL,
  alias text NOT NULL
);
CREATE UNLOGGED TABLE IF NOT EXISTS cpamp_import_new_requests
  (LIKE request_records INCLUDING DEFAULTS);
TRUNCATE cpamp_import_usage, cpamp_import_aliases, cpamp_import_prices,
         cpamp_import_identities, cpamp_import_new_requests;
SQL

if [ "$CPAMP_RESET_IMPORT" = true ]; then
  psql_target -v tenant_external_id="$IMPORT_TENANT_EXTERNAL_ID" <<'SQL'
BEGIN;
DELETE FROM request_events
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM conversation_edges
 WHERE cluster_id IN (
   SELECT id FROM conversation_clusters
    WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id')
 );
DELETE FROM conversation_observations
 WHERE key_id IN (
   SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 );
DELETE FROM conversation_clusters
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM context_nodes
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM semantic_atoms
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM generation_jobs
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM usage_daily_aggregates
 WHERE key_id IN (
   SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 ) OR key_id LIKE 'cpamp-key-%';
DELETE FROM request_records
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM legacy_key_credentials
 WHERE key_id IN (
   SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 );
DELETE FROM key_credentials
 WHERE key_id IN (
   SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 );
DELETE FROM rate_limit_windows
 WHERE key_id IN (
   SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 );
DELETE FROM key_runtime_state
 WHERE key_id IN (
   SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 );
DELETE FROM usage_reservations
 WHERE account_id IN (
   SELECT a.id FROM credit_accounts a JOIN tenants t ON t.id = a.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 );
DELETE FROM ledger_entries
 WHERE account_id IN (
   SELECT a.id FROM credit_accounts a JOIN tenants t ON t.id = a.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 );
DELETE FROM model_routes
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM upstream_credentials
 WHERE account_id IN (
   SELECT u.id FROM upstream_accounts u JOIN tenants t ON t.id = u.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 );
DELETE FROM upstream_accounts
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM key_records
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM credit_accounts
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM principals
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM tenants WHERE external_id = :'tenant_external_id';
DELETE FROM cpamp_import_checkpoints WHERE source = 'cpamp-usage-events-v1';
COMMIT;
SQL
fi

watermark_ms=$(psql_target -Atc "SELECT COALESCE((SELECT watermark_ms FROM cpamp_import_checkpoints WHERE source = 'cpamp-usage-events-v1'), 0)")
case "$watermark_ms" in *[!0-9]*|'') echo "invalid PostgreSQL import watermark" >&2; exit 2;; esac
if [ "$watermark_ms" -gt "$CPAMP_OVERLAP_MS" ]; then
  lower_bound_ms=$((watermark_ms - CPAMP_OVERLAP_MS))
else
  lower_bound_ms=0
fi

# A bounded overlap catches late CPAMP queue flushes. request idempotency makes
# every run safe, while avoiding another multi-GB full-table scan at cutover.
sqlite3 -header -csv "$CPAMP_SQLITE_PATH" \
  "SELECT event_hash, request_id, timestamp_ms, provider, model, endpoint,
          api_key_hash, input_tokens, output_tokens, latency_ms,
          CASE WHEN failed THEN 1 ELSE 0 END,
          COALESCE(fail_status_code, 0), COALESCE(fail_summary, '')
     FROM usage_events
    WHERE event_hash <> '' AND timestamp_ms >= $lower_bound_ms
    ;" \
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

WITH digest AS (SELECT md5('tenant:' || :'tenant_external_id') AS value)
INSERT INTO tenants (id, external_id, created_at)
SELECT substr(value,1,8)||'-'||substr(value,9,4)||'-5'||substr(value,14,3)||'-a'||substr(value,18,3)||'-'||substr(value,21,12),
       :'tenant_external_id', (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM digest
ON CONFLICT (external_id) DO NOTHING;

WITH digest AS (SELECT md5('principal:' || :'tenant_external_id' || ':cpamp-import') AS value)
INSERT INTO principals (id, tenant_id, external_id, created_at)
SELECT substr(value,1,8)||'-'||substr(value,9,4)||'-5'||substr(value,14,3)||'-a'||substr(value,18,3)||'-'||substr(value,21,12),
       t.id, 'cpamp-import', (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM digest CROSS JOIN tenants t WHERE t.external_id = :'tenant_external_id'
ON CONFLICT (tenant_id, external_id) DO NOTHING;

INSERT INTO cpamp_import_identities (api_key_hash, key_id, account_id, alias)
SELECT h.api_key_hash,
       substr(k.value,1,8)||'-'||substr(k.value,9,4)||'-5'||substr(k.value,14,3)||'-a'||substr(k.value,18,3)||'-'||substr(k.value,21,12),
       substr(a.value,1,8)||'-'||substr(a.value,9,4)||'-5'||substr(a.value,14,3)||'-a'||substr(a.value,18,3)||'-'||substr(a.value,21,12),
       COALESCE(NULLIF(x.alias, ''), 'CPA ' || substr(h.api_key_hash, 1, 8))
  FROM (SELECT api_key_hash FROM cpamp_import_usage UNION SELECT api_key_hash FROM cpamp_import_aliases) h
  LEFT JOIN cpamp_import_aliases x USING (api_key_hash)
  CROSS JOIN LATERAL (SELECT md5('key:' || :'tenant_external_id' || ':' || h.api_key_hash) AS value) k
  CROSS JOIN LATERAL (SELECT md5('account:' || :'tenant_external_id' || ':' || h.api_key_hash) AS value) a
 WHERE length(h.api_key_hash) = 64;

INSERT INTO credit_accounts
  (id, tenant_id, principal_id, currency, available_micros, reserved_micros, created_at, updated_at)
SELECT i.account_id, t.id, p.id, 'USD', 0, 0,
       (extract(epoch from clock_timestamp()) * 1000)::bigint,
       (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM cpamp_import_identities i
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  JOIN principals p ON p.tenant_id = t.id AND p.external_id = 'cpamp-import'
ON CONFLICT (id) DO NOTHING;

INSERT INTO key_records
  (id, tenant_id, principal_id, account_id, alias, currency, policy_json,
   status, credential_generation, created_at, updated_at)
SELECT i.key_id, t.id, p.id, i.account_id, i.alias, 'USD',
       '{"allowed_models":["*"],"requests_per_minute":60,"tokens_per_minute":1000000,"max_concurrency":4,"daily_budget":null,"weekly_budget":null,"lifetime_budget":null}',
       'active', 0,
       (extract(epoch from clock_timestamp()) * 1000)::bigint,
       (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM cpamp_import_identities i
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  JOIN principals p ON p.tenant_id = t.id AND p.external_id = 'cpamp-import'
ON CONFLICT (id) DO UPDATE SET alias = excluded.alias, updated_at = excluded.updated_at;

INSERT INTO model_prices
  (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at)
SELECT substr(d.value,1,8)||'-'||substr(d.value,9,4)||'-5'||substr(d.value,14,3)||'-a'||substr(d.value,18,3)||'-'||substr(d.value,21,12),
       p.model, 'USD', round(COALESCE(p.prompt_per_1m, 0) * 1000000)::bigint,
       round(COALESCE(p.completion_per_1m, 0) * 1000000)::bigint,
       'cpamp:' || COALESCE(NULLIF(p.source, ''), 'import'), p.updated_at_ms
  FROM cpamp_import_prices p
  CROSS JOIN LATERAL (SELECT md5('price:USD:' || p.model) AS value) d
 WHERE p.model <> ''
ON CONFLICT (model, currency) DO UPDATE SET
  input_micros_per_million = excluded.input_micros_per_million,
  output_micros_per_million = excluded.output_micros_per_million,
  source = excluded.source, updated_at = excluded.updated_at;

INSERT INTO cpamp_import_new_requests
  (id, tenant_id, key_id, created_at, completed_at, protocol, model,
   status_code, duration_ms, input_tokens, output_tokens, cost_micros,
   error_code, request_object, response_object, reservation_id)
SELECT substr(r.value,1,8)||'-'||substr(r.value,9,4)||'-5'||substr(r.value,14,3)||'-a'||substr(r.value,18,3)||'-'||substr(r.value,21,12),
       t.id, i.key_id, u.timestamp_ms,
       u.timestamp_ms + GREATEST(COALESCE(u.latency_ms, 0), 0),
       COALESCE(NULLIF(u.provider, ''), 'openai'), COALESCE(NULLIF(u.model, ''), '-'),
       CASE WHEN u.failed <> 0 THEN NULLIF(u.fail_status_code, 0) ELSE 200 END,
       GREATEST(COALESCE(u.latency_ms, 0), 0), GREATEST(COALESCE(u.input_tokens, 0), 0),
       GREATEST(COALESCE(u.output_tokens, 0), 0),
       round((GREATEST(COALESCE(u.input_tokens, 0), 0) * COALESCE(p.input_micros_per_million, 0)
            + GREATEST(COALESCE(u.output_tokens, 0), 0) * COALESCE(p.output_micros_per_million, 0)) / 1000000.0)::bigint,
       CASE WHEN u.failed <> 0 THEN CASE WHEN u.fail_status_code > 0
            THEN 'http_' || u.fail_status_code::text ELSE 'upstream_error' END END,
       'inline-json:' || jsonb_build_object('source','cpamp','request_id',u.request_id,'endpoint',u.endpoint)::text,
       CASE WHEN u.failed <> 0
            THEN 'inline-json:' || jsonb_build_object('source','cpamp','error',u.fail_summary)::text
            ELSE 'gap://cpamp/' || u.event_hash END,
       'cpamp-import:' || u.event_hash
  FROM cpamp_import_usage u
  JOIN cpamp_import_identities i USING (api_key_hash)
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  LEFT JOIN model_prices p ON p.model = u.model AND p.currency = 'USD'
  CROSS JOIN LATERAL (SELECT md5('request:cpamp:' || u.event_hash) AS value) r
 WHERE NOT EXISTS (
       SELECT 1 FROM request_records existing
        WHERE existing.id = substr(r.value,1,8)||'-'||substr(r.value,9,4)||'-5'||substr(r.value,14,3)||'-a'||substr(r.value,18,3)||'-'||substr(r.value,21,12)
 );

INSERT INTO request_records SELECT * FROM cpamp_import_new_requests;

INSERT INTO usage_daily_aggregates
  (key_id, day_bucket, model, status_class, error_code, requests,
   input_tokens, output_tokens, cost_micros)
SELECT key_id, created_at / 86400000, model,
       CASE WHEN status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END,
       COALESCE(error_code, ''), count(*), sum(input_tokens), sum(output_tokens), sum(cost_micros)
  FROM cpamp_import_new_requests
 GROUP BY key_id, created_at / 86400000, model,
          CASE WHEN status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END,
          COALESCE(error_code, '')
ON CONFLICT (key_id, day_bucket, model, status_class, error_code) DO UPDATE SET
  requests = usage_daily_aggregates.requests + excluded.requests,
  input_tokens = usage_daily_aggregates.input_tokens + excluded.input_tokens,
  output_tokens = usage_daily_aggregates.output_tokens + excluded.output_tokens,
  cost_micros = usage_daily_aggregates.cost_micros + excluded.cost_micros;

INSERT INTO cpamp_import_checkpoints
  (source, watermark_ms, watermark_hash, imported_events, updated_at)
SELECT 'cpamp-usage-events-v1', COALESCE(max(timestamp_ms), 0),
       COALESCE((array_agg(event_hash ORDER BY timestamp_ms DESC, event_hash DESC))[1], ''),
       (SELECT count(*) FROM cpamp_import_new_requests),
       (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM cpamp_import_usage
ON CONFLICT (source) DO UPDATE SET
  watermark_ms = GREATEST(cpamp_import_checkpoints.watermark_ms, excluded.watermark_ms),
  watermark_hash = CASE WHEN excluded.watermark_ms >= cpamp_import_checkpoints.watermark_ms
                        THEN excluded.watermark_hash ELSE cpamp_import_checkpoints.watermark_hash END,
  imported_events = cpamp_import_checkpoints.imported_events + excluded.imported_events,
  updated_at = excluded.updated_at;

COMMIT;
ANALYZE request_records;
ANALYZE usage_daily_aggregates;
SELECT imported_events AS total_imported_events, watermark_ms
  FROM cpamp_import_checkpoints WHERE source = 'cpamp-usage-events-v1';
SQL
