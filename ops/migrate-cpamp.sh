#!/bin/sh
set -eu

: "${PGHOST:?PGHOST is required}"
: "${PGPORT:=5432}"
: "${PGUSER:?PGUSER is required}"
: "${PGPASSWORD:?PGPASSWORD is required}"
: "${PGDATABASE:?PGDATABASE is required}"
: "${CPAMP_SQLITE_PATH:=/source/usage.sqlite}"
: "${IMPORT_TENANT_EXTERNAL_ID:=cpa-dogfood-import}"
: "${CPAMP_IMPORT_SOURCE:=cpamp-usage-events-v1}"
: "${CPAMP_OVERLAP_MS:=86400000}"
: "${CPAMP_RESET_IMPORT:=false}"
: "${CPAMP_RESET_CONFIRM:=}"
: "${CPAMP_ALLOW_UNMAPPED:=false}"

case "$CPAMP_OVERLAP_MS" in *[!0-9]*|'') echo "CPAMP_OVERLAP_MS must be an integer" >&2; exit 2;; esac
case "$CPAMP_RESET_IMPORT" in true|false) ;; *) echo "CPAMP_RESET_IMPORT must be true or false" >&2; exit 2;; esac
case "$CPAMP_ALLOW_UNMAPPED" in true|false) ;; *) echo "CPAMP_ALLOW_UNMAPPED must be true or false" >&2; exit 2;; esac
[ "$CPAMP_RESET_IMPORT" = false ] || [ "$IMPORT_TENANT_EXTERNAL_ID" = "cpa-dogfood-import" ] || {
  echo "reset is only allowed for the cpa-dogfood-import tenant" >&2
  exit 2
}
[ "$CPAMP_RESET_IMPORT" = false ] || [ "$CPAMP_RESET_CONFIRM" = "DELETE_CPA_DOGFOOD_IMPORT" ] || {
  echo "CPAMP_RESET_CONFIRM=DELETE_CPA_DOGFOOD_IMPORT is required for a reset" >&2
  exit 2
}
[ -r "$CPAMP_SQLITE_PATH" ] || { echo "CPAMP SQLite database is not readable" >&2; exit 2; }
case "$CPAMP_IMPORT_SOURCE" in *[!A-Za-z0-9._:-]*|'') echo "CPAMP_IMPORT_SOURCE contains unsupported characters" >&2; exit 2;; esac

# Use libpq's individual environment variables so credentials never appear in
# argv and the Debian psql wrapper never interprets a URI as a local database.
export PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE

cleanup_import_lock() {
  if [ -n "${CPAMP_LOCK_PID:-}" ]; then
    kill "$CPAMP_LOCK_PID" 2>/dev/null || true
    wait "$CPAMP_LOCK_PID" 2>/dev/null || true
  fi
  if [ -n "${CPAMP_LOCK_APP:-}" ]; then
    psql_target -v lock_app="$CPAMP_LOCK_APP" -At >/dev/null 2>&1 <<'SQL' || true
SELECT pg_terminate_backend(pid)
  FROM pg_stat_activity
 WHERE application_name = :'lock_app'
   AND usename = current_user
   AND pid <> pg_backend_pid();
SQL
  fi
  if [ -n "${CPAMP_LOCK_STATUS_FILE:-}" ]; then
    rm -f "$CPAMP_LOCK_STATUS_FILE"
  fi
}
trap cleanup_import_lock EXIT HUP INT TERM

# Hold a session-level advisory lock across SQLite extraction and the final transaction.
# This prevents two CronJobs from truncating or mixing the shared staging tables.
psql_target() {
  psql -X -v ON_ERROR_STOP=1 --no-psqlrc "$@"
}

# The staging tables are intentionally shared to keep COPY fast, so imports for
# different tenants and sources must also serialize on one global lock.
lock_key="memeloop-token-center:cpamp:global-staging-v1"
CPAMP_LOCK_STATUS_FILE=$(mktemp /tmp/mtc-cpamp-lock.XXXXXX)
CPAMP_LOCK_APP="mtc-cpamp-lock-${CPAMP_LOCK_STATUS_FILE##*.}"
lock_app=$CPAMP_LOCK_APP
PGAPPNAME="$lock_app" psql -X -v ON_ERROR_STOP=1 --no-psqlrc \
  -v lock_key="$lock_key" -At >/dev/null <<SQL &
SET lock_timeout = '30s';
SELECT pg_advisory_lock(hashtextextended(:'lock_key', 734627102948313));
SELECT pg_sleep(86400);
SQL
CPAMP_LOCK_PID=$!
lock_acquired=false
attempt=0
while [ "$attempt" -lt 30 ]; do
  lock_ready=$(psql_target -v lock_app="$lock_app" -At <<'SQL'
SELECT EXISTS (
  SELECT 1 FROM pg_stat_activity
   WHERE application_name = :'lock_app' AND wait_event = 'PgSleep'
);
SQL
)
  if [ "$lock_ready" = t ]; then
    lock_acquired=true
    break
  fi
  kill -0 "$CPAMP_LOCK_PID" 2>/dev/null || break
  attempt=$((attempt + 1))
  sleep 1
done
[ "$lock_acquired" = true ] || { echo "another CPAMP import is running or the import lock failed" >&2; exit 2; }

psql_target -v tenant_external_id="$IMPORT_TENANT_EXTERNAL_ID" <<'SQL'
CREATE TABLE IF NOT EXISTS cpamp_import_checkpoints (
  tenant_external_id text NOT NULL,
  source text NOT NULL,
  watermark_ms bigint NOT NULL,
  watermark_hash text NOT NULL,
  imported_events bigint NOT NULL,
  updated_at bigint NOT NULL,
  PRIMARY KEY (tenant_external_id, source)
);
ALTER TABLE cpamp_import_checkpoints
  ADD COLUMN IF NOT EXISTS tenant_external_id text;
UPDATE cpamp_import_checkpoints
   SET tenant_external_id = :'tenant_external_id'
 WHERE tenant_external_id IS NULL;
ALTER TABLE cpamp_import_checkpoints
  ALTER COLUMN tenant_external_id SET NOT NULL;
ALTER TABLE cpamp_import_checkpoints
  DROP CONSTRAINT IF EXISTS cpamp_import_checkpoints_pkey;
ALTER TABLE cpamp_import_checkpoints
  ADD CONSTRAINT cpamp_import_checkpoints_pkey PRIMARY KEY (tenant_external_id, source);
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
  psql_target -v tenant_external_id="$IMPORT_TENANT_EXTERNAL_ID" -v import_source="$CPAMP_IMPORT_SOURCE" <<'SQL'
BEGIN;
SELECT set_config('mtc.cpamp_reset_tenant', :'tenant_external_id', true);
SELECT set_config('mtc.cpamp_reset_source', :'import_source', true);
DO $reset_guard$
BEGIN
  IF EXISTS (
    SELECT 1 FROM tenants t
    JOIN upstream_accounts u ON u.tenant_id = t.id
    WHERE t.external_id = current_setting('mtc.cpamp_reset_tenant')
  ) OR EXISTS (
    SELECT 1 FROM tenants t
    JOIN model_routes r ON r.tenant_id = t.id
    WHERE t.external_id = current_setting('mtc.cpamp_reset_tenant')
  ) THEN
    RAISE EXCEPTION 'CPAMP reset refused: tenant has provider accounts or model routes not owned by the usage importer';
  END IF;
  IF EXISTS (
    SELECT 1 FROM cpamp_import_checkpoints
    WHERE tenant_external_id = current_setting('mtc.cpamp_reset_tenant')
      AND source <> current_setting('mtc.cpamp_reset_source')
  ) OR EXISTS (
    SELECT 1 FROM import_request_links l
    JOIN tenants t ON t.id = l.tenant_id
    WHERE t.external_id = current_setting('mtc.cpamp_reset_tenant')
      AND l.source <> current_setting('mtc.cpamp_reset_source')
  ) THEN
    RAISE EXCEPTION 'CPAMP reset refused: tenant contains a different import source';
  END IF;
  IF EXISTS (
    SELECT 1 FROM principals p JOIN tenants t ON t.id = p.tenant_id
    WHERE t.external_id = current_setting('mtc.cpamp_reset_tenant')
      AND p.external_id <> 'cpamp-import'
  ) OR EXISTS (
    SELECT 1 FROM key_records k JOIN tenants t ON t.id = k.tenant_id
    LEFT JOIN principals p ON p.id = k.principal_id AND p.tenant_id = k.tenant_id
    WHERE t.external_id = current_setting('mtc.cpamp_reset_tenant')
      AND COALESCE(p.external_id, '') <> 'cpamp-import'
  ) OR EXISTS (
    SELECT 1 FROM credit_accounts a JOIN tenants t ON t.id = a.tenant_id
    LEFT JOIN principals p ON p.id = a.principal_id AND p.tenant_id = a.tenant_id
    WHERE t.external_id = current_setting('mtc.cpamp_reset_tenant')
      AND COALESCE(p.external_id, '') <> 'cpamp-import'
  ) THEN
    RAISE EXCEPTION 'CPAMP reset refused: tenant contains identities not owned by the usage importer';
  END IF;
END
$reset_guard$;
DELETE FROM request_event_locators
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
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
DELETE FROM usage_analysis_hourly
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM usage_analysis_daily
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM request_daily_aggregates
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM request_stats_facts
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM usage_daily_aggregates
 WHERE key_id IN (
   SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id
    WHERE t.external_id = :'tenant_external_id'
 ) OR key_id LIKE 'cpamp-key-%';
DELETE FROM request_record_locators
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
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
DELETE FROM key_records
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM credit_accounts
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM principals
 WHERE tenant_id IN (SELECT id FROM tenants WHERE external_id = :'tenant_external_id');
DELETE FROM tenants WHERE external_id = :'tenant_external_id';
DELETE FROM cpamp_import_checkpoints
 WHERE tenant_external_id = :'tenant_external_id' AND source = :'import_source';
COMMIT;
SQL
fi

watermark_ms=$(psql_target -v tenant_external_id="$IMPORT_TENANT_EXTERNAL_ID" -v import_source="$CPAMP_IMPORT_SOURCE" -At <<'SQL'
SELECT COALESCE((
  SELECT watermark_ms FROM cpamp_import_checkpoints
   WHERE tenant_external_id = :'tenant_external_id' AND source = :'import_source'
), 0);
SQL
)
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
          lower(api_key_hash), input_tokens, output_tokens, latency_ms,
          CASE WHEN failed THEN 1 ELSE 0 END,
          COALESCE(fail_status_code, 0), COALESCE(fail_summary, '')
     FROM usage_events
    WHERE event_hash <> '' AND timestamp_ms >= $lower_bound_ms
    ;" \
  | psql_target -c "\\copy cpamp_import_usage FROM STDIN WITH (FORMAT csv, HEADER true)"

sqlite3 -header -csv "$CPAMP_SQLITE_PATH" \
  "SELECT lower(api_key_hash), alias, updated_at_ms FROM api_key_aliases;" \
  | psql_target -c "\\copy cpamp_import_aliases FROM STDIN WITH (FORMAT csv, HEADER true)"

sqlite3 -header -csv "$CPAMP_SQLITE_PATH" \
  "SELECT model, prompt_per_1m, completion_per_1m, source, updated_at_ms
     FROM model_prices;" \
  | psql_target -c "\\copy cpamp_import_prices FROM STDIN WITH (FORMAT csv, HEADER true)"

staged_count=$(psql_target -Atc "SELECT count(*) FROM cpamp_import_usage")
unmapped_count=$(psql_target -Atc "SELECT count(*) FROM cpamp_import_usage WHERE COALESCE(api_key_hash, '') !~ '^[0-9a-f]{64}$'")
invalid_alias_count=$(psql_target -Atc "SELECT count(*) FROM cpamp_import_aliases WHERE COALESCE(api_key_hash, '') !~ '^[0-9a-f]{64}$'")
duplicate_count=$(psql_target -Atc "SELECT COALESCE(sum(events - 1), 0) FROM (SELECT count(*) AS events FROM cpamp_import_usage GROUP BY event_hash HAVING count(*) > 1) duplicates")
conflicting_duplicate_count=$(psql_target -At <<'SQL'
WITH duplicate_event_hashes AS MATERIALIZED (
  SELECT event_hash
    FROM cpamp_import_usage
   GROUP BY event_hash
  HAVING count(*) > 1
),
-- Keep both CTEs materialized so PostgreSQL cannot move sha256 evaluation
-- ahead of the cheap event-hash candidate reduction.
digested_duplicates AS MATERIALIZED (
  SELECT u.event_hash,
         encode(sha256(convert_to(jsonb_build_array(
           u.request_id, u.timestamp_ms, u.provider, u.model, u.endpoint, u.api_key_hash,
           u.input_tokens, u.output_tokens, u.latency_ms, u.failed,
           u.fail_status_code, u.fail_summary
         )::text, 'UTF8')), 'hex') AS source_digest
    FROM cpamp_import_usage u
    JOIN duplicate_event_hashes d ON d.event_hash = u.event_hash
)
SELECT count(*)
  FROM (
    SELECT event_hash
      FROM digested_duplicates
     GROUP BY event_hash
    HAVING count(DISTINCT source_digest) > 1
  ) conflicts;
SQL
)
case "$staged_count:$unmapped_count:$invalid_alias_count:$duplicate_count:$conflicting_duplicate_count" in
  *[!0-9:]*|:*|*::*|*:)
    echo "invalid CPAMP staging validation result" >&2
    exit 2
    ;;
esac
if [ "$unmapped_count" -gt 0 ] && [ "$CPAMP_ALLOW_UNMAPPED" != true ]; then
  echo "CPAMP import stopped: $unmapped_count staged events have no supported key identity; set CPAMP_ALLOW_UNMAPPED=true only after accepting that data loss" >&2
  exit 2
fi
if [ "$invalid_alias_count" -gt 0 ]; then
  echo "CPAMP import stopped: $invalid_alias_count staged aliases have a non-hex key identity" >&2
  exit 2
fi
if [ "$conflicting_duplicate_count" -gt 0 ]; then
  echo "CPAMP import stopped: $conflicting_duplicate_count event hashes map to conflicting source rows" >&2
  exit 2
fi
echo "CPAMP staged=$staged_count unmapped=$unmapped_count duplicate_rows_deduplicated=$duplicate_count conflicting_event_hashes=0" >&2

psql_target -v tenant_external_id="$IMPORT_TENANT_EXTERNAL_ID" -v import_source="$CPAMP_IMPORT_SOURCE" <<'SQL'
BEGIN;

CREATE TEMP TABLE cpamp_import_canonical ON COMMIT DROP AS
SELECT DISTINCT ON (event_hash) u.*,
       encode(sha256(convert_to(jsonb_build_array(
         request_id, timestamp_ms, provider, model, endpoint, api_key_hash,
         input_tokens, output_tokens, latency_ms, failed,
         fail_status_code, fail_summary
       )::text, 'UTF8')), 'hex') AS source_digest
  FROM cpamp_import_usage u
 ORDER BY event_hash, timestamp_ms DESC, request_id DESC;

-- Once an event hash has durable provenance, the source payload is immutable. Abort the
-- whole transaction before accepting a changed row under an already-imported identity.
CREATE TEMP TABLE cpamp_import_source_conflicts (
  event_hash text,
  invalid boolean NOT NULL CHECK (invalid = false)
) ON COMMIT DROP;
INSERT INTO cpamp_import_source_conflicts (event_hash, invalid)
SELECT u.event_hash, true
  FROM cpamp_import_canonical u
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  JOIN import_request_links l
    ON l.tenant_id = t.id
   AND l.source = :'import_source'
   AND l.external_event_hash = u.event_hash
 WHERE l.source_digest <> '' AND l.source_digest <> u.source_digest;

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
  source = excluded.source, updated_at = excluded.updated_at
WHERE model_prices.source LIKE 'cpamp:%';

-- Since schema v18 settlement snapshots prefer model_price_tiers. Keep the CPAMP-owned
-- default tier in lockstep with its compatibility row, but never overwrite a manual or
-- catalog-owned base/tier. Missing cache dimensions conservatively reuse input pricing.
INSERT INTO model_price_tiers
  (id, model, currency, service_tier, input_micros_per_million,
   cached_input_micros_per_million, cache_write_micros_per_million,
   output_micros_per_million, cache_price_estimated, source, updated_at)
SELECT substr(d.value,1,8)||'-'||substr(d.value,9,4)||'-5'||substr(d.value,14,3)||'-a'||substr(d.value,18,3)||'-'||substr(d.value,21,12),
       b.model, b.currency, 'default', b.input_micros_per_million,
       b.input_micros_per_million, b.input_micros_per_million,
       b.output_micros_per_million, 1, b.source, b.updated_at
  FROM model_prices b
  JOIN cpamp_import_prices p ON p.model = b.model
  CROSS JOIN LATERAL (SELECT md5('price-tier:USD:default:' || b.model) AS value) d
 WHERE b.currency = 'USD' AND b.source LIKE 'cpamp:%'
ON CONFLICT (model, currency, service_tier) DO UPDATE SET
  input_micros_per_million = excluded.input_micros_per_million,
  cached_input_micros_per_million = excluded.cached_input_micros_per_million,
  cache_write_micros_per_million = excluded.cache_write_micros_per_million,
  output_micros_per_million = excluded.output_micros_per_million,
  cache_price_estimated = excluded.cache_price_estimated,
  source = excluded.source,
  updated_at = excluded.updated_at
WHERE model_price_tiers.source LIKE 'cpamp:%';

INSERT INTO cpamp_import_new_requests
  (id, tenant_id, key_id, created_at, completed_at, protocol, model,
   status_code, duration_ms, input_tokens, output_tokens, cost_micros,
   error_code, request_object, response_object, reservation_id,
   cached_input_tokens, cache_write_tokens, service_tier, currency)
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
       'gap://cpamp/' || u.event_hash || '/request',
       'gap://cpamp/' || u.event_hash || '/response',
       'cpamp-import:' || u.event_hash,
       0, 0, 'default', 'USD'
  FROM cpamp_import_canonical u
  JOIN cpamp_import_identities i USING (api_key_hash)
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  LEFT JOIN model_prices p ON p.model = u.model AND p.currency = 'USD'
  CROSS JOIN LATERAL (SELECT md5('request:cpamp:' ||
    CASE WHEN :'tenant_external_id' = 'cpa-dogfood-import' AND :'import_source' = 'cpamp-usage-events-v1'
         THEN '' ELSE :'tenant_external_id' || ':' || :'import_source' || ':' END || u.event_hash) AS value) r
;

CREATE TEMP TABLE cpamp_import_claimed_requests (
  id text PRIMARY KEY
) ON COMMIT DROP;
WITH claimed AS (
  INSERT INTO request_record_locators (id, created_at, tenant_id, key_id)
  SELECT id, created_at, tenant_id, key_id
    FROM cpamp_import_new_requests
  ON CONFLICT (id) DO NOTHING
  RETURNING id
)
INSERT INTO cpamp_import_claimed_requests (id)
SELECT id FROM claimed;

-- A replay is accepted only when the stable routing coordinates are identical
-- and the pointed-to leaf row exists.  The CHECK makes every other collision
-- abort the surrounding transaction instead of silently selecting one row.
CREATE TEMP TABLE cpamp_import_locator_conflicts (
  id text,
  invalid boolean NOT NULL CHECK (invalid = false)
) ON COMMIT DROP;
INSERT INTO cpamp_import_locator_conflicts (id, invalid)
SELECT n.id, true
  FROM cpamp_import_new_requests n
  JOIN request_record_locators l ON l.id = n.id
 WHERE l.created_at <> n.created_at
    OR l.tenant_id <> n.tenant_id
    OR l.key_id <> n.key_id
    OR (
      NOT EXISTS (SELECT 1 FROM cpamp_import_claimed_requests c WHERE c.id = n.id)
      AND NOT EXISTS (
        SELECT 1 FROM request_records r
         WHERE r.id = l.id AND r.created_at = l.created_at
           AND r.tenant_id = l.tenant_id AND r.key_id = l.key_id
      )
    );

-- Only the transaction that won the locator claim may create the partitioned
-- leaf and contribute aggregates.  Exact-coordinate replays remain no-ops.
DELETE FROM cpamp_import_new_requests n
 WHERE NOT EXISTS (SELECT 1 FROM cpamp_import_claimed_requests c WHERE c.id = n.id);

INSERT INTO request_records SELECT * FROM cpamp_import_new_requests;

INSERT INTO request_stats_facts
  (request_id, tenant_id, key_id, created_at, model, protocol, status_class,
   error_code, upstream_account_id, model_route_id, duration_ms,
   input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
   service_tier, currency, cost_micros)
SELECT id, tenant_id, key_id, created_at, model, protocol,
       CASE WHEN status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END,
       COALESCE(error_code, ''), COALESCE(upstream_account_id, ''),
       COALESCE(model_route_id, ''), COALESCE(duration_ms, 0),
       input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
       service_tier, currency, cost_micros
  FROM cpamp_import_new_requests
 WHERE completed_at IS NOT NULL AND status_code IS NOT NULL
ON CONFLICT (request_id) DO NOTHING;

-- All aggregate deltas are derived from the facts whose locator was claimed by this
-- transaction. Exact replays therefore cannot increment any legacy or v24 analysis table.
CREATE TEMP TABLE cpamp_import_new_request_facts ON COMMIT DROP AS
SELECT f.*
  FROM request_stats_facts f
  JOIN cpamp_import_claimed_requests c ON c.id = f.request_id;

INSERT INTO request_daily_aggregates
  (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code,
   upstream_account_id, model_route_id, service_tier, currency, requests,
   input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
   duration_count, duration_sum_ms, cost_micros)
SELECT tenant_id, key_id, created_at / 86400000, model, protocol,
       status_class, error_code, upstream_account_id, model_route_id,
       service_tier, currency, COUNT(*), COALESCE(SUM(input_tokens), 0),
       COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cached_input_tokens), 0),
       COALESCE(SUM(cache_write_tokens), 0), COUNT(*),
       COALESCE(SUM(duration_ms), 0), COALESCE(SUM(cost_micros), 0)
  FROM cpamp_import_new_request_facts
 GROUP BY tenant_id, key_id, created_at / 86400000, model, protocol,
          status_class, error_code, upstream_account_id, model_route_id,
          service_tier, currency
ON CONFLICT (tenant_id, key_id, day_bucket, model, protocol, status_class,
             error_code, upstream_account_id, model_route_id, service_tier,
             currency) DO UPDATE SET
  requests = request_daily_aggregates.requests + excluded.requests,
  input_tokens = request_daily_aggregates.input_tokens + excluded.input_tokens,
  output_tokens = request_daily_aggregates.output_tokens + excluded.output_tokens,
  cached_input_tokens = request_daily_aggregates.cached_input_tokens + excluded.cached_input_tokens,
  cache_write_tokens = request_daily_aggregates.cache_write_tokens + excluded.cache_write_tokens,
  duration_count = request_daily_aggregates.duration_count + excluded.duration_count,
  duration_sum_ms = request_daily_aggregates.duration_sum_ms + excluded.duration_sum_ms,
  cost_micros = request_daily_aggregates.cost_micros + excluded.cost_micros;

INSERT INTO usage_analysis_hourly
  (tenant_id, key_id, hour_bucket, source_kind, model, protocol, status_class,
   error_code, upstream_account_id, model_route_id, service_tier, currency,
   requests, input_tokens, output_tokens, cached_input_tokens,
   cache_write_tokens, generation_units, duration_count, duration_sum_ms,
   duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3,
   duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7,
   duration_bucket_8, duration_bucket_9, duration_bucket_10,
   duration_bucket_11, cost_micros)
SELECT tenant_id, key_id, created_at / 3600000, 'request', model,
       CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
            THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
            ELSE 'openai' END,
       status_class, error_code, upstream_account_id, model_route_id,
       service_tier, currency, COUNT(*),
       COALESCE(SUM(CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                         THEN input_tokens - cached_input_tokens - cache_write_tokens
                         ELSE 0 END), 0),
       COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cached_input_tokens), 0),
       COALESCE(SUM(cache_write_tokens), 0), 0, COUNT(*),
       COALESCE(SUM(duration_ms), 0),
       COALESCE(SUM(CASE WHEN duration_ms <= 10 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 10 AND duration_ms <= 50 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 50 AND duration_ms <= 100 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 100 AND duration_ms <= 250 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 250 AND duration_ms <= 500 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 500 AND duration_ms <= 1000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 1000 AND duration_ms <= 2500 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 2500 AND duration_ms <= 5000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 5000 AND duration_ms <= 10000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 10000 AND duration_ms <= 30000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 30000 AND duration_ms <= 60000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 60000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(cost_micros), 0)
  FROM cpamp_import_new_request_facts
 GROUP BY tenant_id, key_id, created_at / 3600000, model,
          CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
               THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
               ELSE 'openai' END,
          status_class, error_code, upstream_account_id, model_route_id,
          service_tier, currency
ON CONFLICT (tenant_id, key_id, hour_bucket, source_kind, model, protocol,
             status_class, error_code, upstream_account_id, model_route_id,
             service_tier, currency) DO UPDATE SET
  requests = usage_analysis_hourly.requests + excluded.requests,
  input_tokens = usage_analysis_hourly.input_tokens + excluded.input_tokens,
  output_tokens = usage_analysis_hourly.output_tokens + excluded.output_tokens,
  cached_input_tokens = usage_analysis_hourly.cached_input_tokens + excluded.cached_input_tokens,
  cache_write_tokens = usage_analysis_hourly.cache_write_tokens + excluded.cache_write_tokens,
  generation_units = usage_analysis_hourly.generation_units + excluded.generation_units,
  duration_count = usage_analysis_hourly.duration_count + excluded.duration_count,
  duration_sum_ms = usage_analysis_hourly.duration_sum_ms + excluded.duration_sum_ms,
  duration_bucket_0 = usage_analysis_hourly.duration_bucket_0 + excluded.duration_bucket_0,
  duration_bucket_1 = usage_analysis_hourly.duration_bucket_1 + excluded.duration_bucket_1,
  duration_bucket_2 = usage_analysis_hourly.duration_bucket_2 + excluded.duration_bucket_2,
  duration_bucket_3 = usage_analysis_hourly.duration_bucket_3 + excluded.duration_bucket_3,
  duration_bucket_4 = usage_analysis_hourly.duration_bucket_4 + excluded.duration_bucket_4,
  duration_bucket_5 = usage_analysis_hourly.duration_bucket_5 + excluded.duration_bucket_5,
  duration_bucket_6 = usage_analysis_hourly.duration_bucket_6 + excluded.duration_bucket_6,
  duration_bucket_7 = usage_analysis_hourly.duration_bucket_7 + excluded.duration_bucket_7,
  duration_bucket_8 = usage_analysis_hourly.duration_bucket_8 + excluded.duration_bucket_8,
  duration_bucket_9 = usage_analysis_hourly.duration_bucket_9 + excluded.duration_bucket_9,
  duration_bucket_10 = usage_analysis_hourly.duration_bucket_10 + excluded.duration_bucket_10,
  duration_bucket_11 = usage_analysis_hourly.duration_bucket_11 + excluded.duration_bucket_11,
  cost_micros = usage_analysis_hourly.cost_micros + excluded.cost_micros;

INSERT INTO usage_analysis_daily
  (tenant_id, key_id, day_bucket, source_kind, model, protocol, status_class,
   error_code, upstream_account_id, model_route_id, service_tier, currency,
   requests, input_tokens, output_tokens, cached_input_tokens,
   cache_write_tokens, generation_units, duration_count, duration_sum_ms,
   duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3,
   duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7,
   duration_bucket_8, duration_bucket_9, duration_bucket_10,
   duration_bucket_11, cost_micros)
SELECT tenant_id, key_id, created_at / 86400000, 'request', model,
       CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
            THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
            ELSE 'openai' END,
       status_class, error_code, upstream_account_id, model_route_id,
       service_tier, currency, COUNT(*),
       COALESCE(SUM(CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                         THEN input_tokens - cached_input_tokens - cache_write_tokens
                         ELSE 0 END), 0),
       COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cached_input_tokens), 0),
       COALESCE(SUM(cache_write_tokens), 0), 0, COUNT(*),
       COALESCE(SUM(duration_ms), 0),
       COALESCE(SUM(CASE WHEN duration_ms <= 10 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 10 AND duration_ms <= 50 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 50 AND duration_ms <= 100 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 100 AND duration_ms <= 250 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 250 AND duration_ms <= 500 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 500 AND duration_ms <= 1000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 1000 AND duration_ms <= 2500 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 2500 AND duration_ms <= 5000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 5000 AND duration_ms <= 10000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 10000 AND duration_ms <= 30000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 30000 AND duration_ms <= 60000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(CASE WHEN duration_ms > 60000 THEN 1 ELSE 0 END), 0),
       COALESCE(SUM(cost_micros), 0)
  FROM cpamp_import_new_request_facts
 GROUP BY tenant_id, key_id, created_at / 86400000, model,
          CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
               THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
               ELSE 'openai' END,
          status_class, error_code, upstream_account_id, model_route_id,
          service_tier, currency
ON CONFLICT (tenant_id, key_id, day_bucket, source_kind, model, protocol,
             status_class, error_code, upstream_account_id, model_route_id,
             service_tier, currency) DO UPDATE SET
  requests = usage_analysis_daily.requests + excluded.requests,
  input_tokens = usage_analysis_daily.input_tokens + excluded.input_tokens,
  output_tokens = usage_analysis_daily.output_tokens + excluded.output_tokens,
  cached_input_tokens = usage_analysis_daily.cached_input_tokens + excluded.cached_input_tokens,
  cache_write_tokens = usage_analysis_daily.cache_write_tokens + excluded.cache_write_tokens,
  generation_units = usage_analysis_daily.generation_units + excluded.generation_units,
  duration_count = usage_analysis_daily.duration_count + excluded.duration_count,
  duration_sum_ms = usage_analysis_daily.duration_sum_ms + excluded.duration_sum_ms,
  duration_bucket_0 = usage_analysis_daily.duration_bucket_0 + excluded.duration_bucket_0,
  duration_bucket_1 = usage_analysis_daily.duration_bucket_1 + excluded.duration_bucket_1,
  duration_bucket_2 = usage_analysis_daily.duration_bucket_2 + excluded.duration_bucket_2,
  duration_bucket_3 = usage_analysis_daily.duration_bucket_3 + excluded.duration_bucket_3,
  duration_bucket_4 = usage_analysis_daily.duration_bucket_4 + excluded.duration_bucket_4,
  duration_bucket_5 = usage_analysis_daily.duration_bucket_5 + excluded.duration_bucket_5,
  duration_bucket_6 = usage_analysis_daily.duration_bucket_6 + excluded.duration_bucket_6,
  duration_bucket_7 = usage_analysis_daily.duration_bucket_7 + excluded.duration_bucket_7,
  duration_bucket_8 = usage_analysis_daily.duration_bucket_8 + excluded.duration_bucket_8,
  duration_bucket_9 = usage_analysis_daily.duration_bucket_9 + excluded.duration_bucket_9,
  duration_bucket_10 = usage_analysis_daily.duration_bucket_10 + excluded.duration_bucket_10,
  duration_bucket_11 = usage_analysis_daily.duration_bucket_11 + excluded.duration_bucket_11,
  cost_micros = usage_analysis_daily.cost_micros + excluded.cost_micros;

-- Preserve the source request identity independently from the deterministic target UUID.
-- cpa-session-archive uses the same CPA request id; keeping the CPAMP event hash, timestamp,
-- model and key hash here lets its body importer fail closed instead of guessing by time.
INSERT INTO import_request_links
  (tenant_id, source, external_event_hash, external_request_id, source_key_hash,
   target_request_id, source_created_at, source_model, source_digest, created_at)
SELECT t.id, :'import_source', u.event_hash, u.request_id, u.api_key_hash,
       substr(r.value,1,8)||'-'||substr(r.value,9,4)||'-5'||substr(r.value,14,3)||'-a'||substr(r.value,18,3)||'-'||substr(r.value,21,12),
       u.timestamp_ms, COALESCE(NULLIF(u.model, ''), '-'), u.source_digest,
       (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM cpamp_import_canonical u
  JOIN tenants t ON t.external_id = :'tenant_external_id'
  CROSS JOIN LATERAL (SELECT md5('request:cpamp:' ||
    CASE WHEN :'tenant_external_id' = 'cpa-dogfood-import' AND :'import_source' = 'cpamp-usage-events-v1'
         THEN '' ELSE :'tenant_external_id' || ':' || :'import_source' || ':' END || u.event_hash) AS value) r
 WHERE COALESCE(u.request_id, '') <> '' AND length(COALESCE(u.api_key_hash, '')) = 64
ON CONFLICT (tenant_id, source, external_event_hash) DO UPDATE SET
  external_request_id = excluded.external_request_id,
  source_key_hash = excluded.source_key_hash,
  target_request_id = excluded.target_request_id,
  source_created_at = excluded.source_created_at,
  source_model = excluded.source_model,
  source_digest = CASE WHEN import_request_links.source_digest = ''
                       THEN excluded.source_digest ELSE import_request_links.source_digest END;

-- Earlier CPAMP imports stored tiny synthetic inline-json markers instead of bodies. Normalize
-- only an exact marker reconstructed from the currently staged source row and its durable link.
-- A real inline body, CAS locator, or already archived object can never satisfy this predicate.
UPDATE request_records r
   SET request_object = 'gap://cpamp/' || l.external_event_hash || '/request'
  FROM import_request_links l
  JOIN request_record_locators rl
    ON rl.id = l.target_request_id AND rl.tenant_id = l.tenant_id
  JOIN cpamp_import_canonical u
    ON u.event_hash = l.external_event_hash
   AND u.request_id = l.external_request_id
 WHERE l.tenant_id = r.tenant_id
   AND l.target_request_id = r.id
   AND rl.created_at = r.created_at
   AND l.source = :'import_source'
   AND r.request_object = 'inline-json:' ||
       jsonb_build_object('source','cpamp','request_id',u.request_id,'endpoint',u.endpoint)::text;

UPDATE request_records r
   SET response_object = 'gap://cpamp/' || l.external_event_hash || '/response'
  FROM import_request_links l
  JOIN request_record_locators rl
    ON rl.id = l.target_request_id AND rl.tenant_id = l.tenant_id
  JOIN cpamp_import_canonical u
    ON u.event_hash = l.external_event_hash
   AND u.request_id = l.external_request_id
 WHERE l.tenant_id = r.tenant_id
   AND l.target_request_id = r.id
   AND rl.created_at = r.created_at
   AND l.source = :'import_source'
   AND u.failed <> 0
   AND r.response_object = 'inline-json:' ||
       jsonb_build_object('source','cpamp','error',u.fail_summary)::text;

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
  (tenant_external_id, source, watermark_ms, watermark_hash, imported_events, updated_at)
SELECT :'tenant_external_id', :'import_source', COALESCE(max(timestamp_ms), 0),
       COALESCE((array_agg(event_hash ORDER BY timestamp_ms DESC, event_hash DESC))[1], ''),
       (SELECT count(*) FROM cpamp_import_new_requests),
       (extract(epoch from clock_timestamp()) * 1000)::bigint
  FROM cpamp_import_usage
ON CONFLICT (tenant_external_id, source) DO UPDATE SET
  watermark_ms = GREATEST(cpamp_import_checkpoints.watermark_ms, excluded.watermark_ms),
  watermark_hash = CASE WHEN excluded.watermark_ms >= cpamp_import_checkpoints.watermark_ms
                        THEN excluded.watermark_hash ELSE cpamp_import_checkpoints.watermark_hash END,
  imported_events = cpamp_import_checkpoints.imported_events + excluded.imported_events,
  updated_at = excluded.updated_at;

COMMIT;
ANALYZE request_records;
ANALYZE usage_daily_aggregates;
ANALYZE request_stats_facts;
ANALYZE request_daily_aggregates;
ANALYZE usage_analysis_hourly;
ANALYZE usage_analysis_daily;
SELECT imported_events AS total_imported_events, watermark_ms
  FROM cpamp_import_checkpoints
 WHERE tenant_external_id = :'tenant_external_id' AND source = :'import_source';
SQL
