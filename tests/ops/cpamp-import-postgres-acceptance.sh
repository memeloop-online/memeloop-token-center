#!/bin/sh
set -eu

: "${ACCEPTANCE_SCHEMA:?ACCEPTANCE_SCHEMA is required}"
: "${ACCEPTANCE_RUN_ID:?ACCEPTANCE_RUN_ID is required}"
: "${PGHOST:?PGHOST is required}"
: "${PGUSER:?PGUSER is required}"
: "${PGPASSWORD:?PGPASSWORD is required}"
: "${PGDATABASE:?PGDATABASE is required}"

case "$ACCEPTANCE_SCHEMA" in
  mig_[a-f0-9]*) ;;
  *) echo "refusing to run outside an isolated mig_<uuid> schema" >&2; exit 2 ;;
esac
case "$ACCEPTANCE_RUN_ID" in
  *[!a-z0-9-]*|'') echo "ACCEPTANCE_RUN_ID contains unsupported characters" >&2; exit 2 ;;
esac

export PGOPTIONS="-csearch_path=$ACCEPTANCE_SCHEMA"
work_dir="/tmp/$ACCEPTANCE_RUN_ID"
mkdir -p "$work_dir"
source_db="$work_dir/source.sqlite"
unmapped_db="$work_dir/unmapped.sqlite"
same_duplicate_db="$work_dir/same-duplicate.sqlite"
conflicting_duplicate_db="$work_dir/conflicting-duplicate.sqlite"
tenant="cpamp-$ACCEPTANCE_RUN_ID-main"
other_tenant="cpamp-$ACCEPTANCE_RUN_ID-tenant"
unmapped_tenant="cpamp-$ACCEPTANCE_RUN_ID-unmapped"
same_duplicate_tenant="cpamp-$ACCEPTANCE_RUN_ID-same-duplicate"
conflicting_duplicate_tenant="cpamp-$ACCEPTANCE_RUN_ID-conflicting-duplicate"
source="cpamp-acceptance:$ACCEPTANCE_RUN_ID"
other_source="cpamp-acceptance:$ACCEPTANCE_RUN_ID:source"
duplicate_source="cpamp-acceptance:$ACCEPTANCE_RUN_ID:duplicates"

psql_target() {
  psql -X -v ON_ERROR_STOP=1 --no-psqlrc "$@"
}

psql_scalar() {
  psql_target -At "$@"
}

assert_equal() {
  actual=$1
  expected=$2
  label=$3
  if [ "$actual" != "$expected" ]; then
    echo "$label: expected '$expected', got '$actual'" >&2
    exit 1
  fi
}

run_import() {
  import_tenant=$1
  import_source=$2
  import_database=$3
  CPAMP_SQLITE_PATH="$import_database" \
    IMPORT_TENANT_EXTERNAL_ID="$import_tenant" \
    CPAMP_IMPORT_SOURCE="$import_source" \
    CPAMP_OVERLAP_MS=86400000 \
    CPAMP_RESET_IMPORT=false \
    CPAMP_ALLOW_UNMAPPED=false \
    sh /work/migrate-cpamp.sh >/dev/null
}

current_schema=$(psql_target -Atc 'SELECT current_schema()')
assert_equal "$current_schema" "$ACCEPTANCE_SCHEMA" "PostgreSQL search_path isolation"
psql_target -f /work/0001_initial.sql >/dev/null
psql_target -f /work/0002_query_indexes.sql >/dev/null
psql_target -f /work/0004_request_events.sql >/dev/null
psql_target -f /work/0005_generation_jobs.sql >/dev/null
psql_target -f /work/0018_model_price_tiers.sql >/dev/null
psql_target -f /work/0019_session_archive_import.sql >/dev/null
psql_target -c 'CREATE TABLE IF NOT EXISTS request_records_default PARTITION OF request_records DEFAULT' >/dev/null
psql_target -f /work/0021_request_locators.sql >/dev/null
psql_target -f /work/0022_budget_rollups.sql >/dev/null
psql_target -f /work/0023_generation_daily_aggregates.sql >/dev/null
psql_target -f /work/0024_request_stats_rollups.sql >/dev/null
psql_target -f /work/0027_cpamp_source_digests.sql >/dev/null

sqlite3 "$same_duplicate_db" < /work/initial.sql
sqlite3 "$same_duplicate_db" <<'SQL'
DELETE FROM usage_events;
INSERT INTO usage_events VALUES
  ('fixture-event-same-duplicate', 'legacy-request-same-duplicate', 100000000,
   'openai', 'fixture-model', '/v1/responses',
   'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
   5, 2, 40, 0, NULL, NULL),
  ('fixture-event-same-duplicate', 'legacy-request-same-duplicate', 100000000,
   'openai', 'fixture-model', '/v1/responses',
   'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
   5, 2, 40, 0, NULL, NULL);
SQL
run_import "$same_duplicate_tenant" "$duplicate_source" "$same_duplicate_db"
run_import "$same_duplicate_tenant" "$duplicate_source" "$same_duplicate_db"
same_duplicate_source=$(sqlite3 "$same_duplicate_db" \
  "SELECT count(*) || '|' || count(DISTINCT event_hash) FROM usage_events;")
assert_equal "$same_duplicate_source" "2|1" "same-payload duplicate source fixture"
same_duplicate_state=$(psql_scalar \
  -v tenant="$same_duplicate_tenant" -v source="$duplicate_source" <<'SQL'
SELECT
  (SELECT count(*) FROM request_records r JOIN tenants t ON t.id = r.tenant_id
    WHERE t.external_id = :'tenant') || '|' ||
  (SELECT count(*) FROM import_request_links l JOIN tenants t ON t.id = l.tenant_id
    WHERE t.external_id = :'tenant' AND l.source = :'source') || '|' ||
  (SELECT count(*) FROM request_stats_facts f JOIN tenants t ON t.id = f.tenant_id
    WHERE t.external_id = :'tenant') || '|' ||
  (SELECT imported_events FROM cpamp_import_checkpoints
    WHERE tenant_external_id = :'tenant' AND source = :'source');
SQL
)
assert_equal "$same_duplicate_state" "1|1|1|1" \
  "same event hash and payload deduplicate idempotently"
echo "phase=same-payload-duplicate source_rows=2 distinct_hashes=1 requests=1 links=1 replay=stable"

sqlite3 "$conflicting_duplicate_db" < /work/initial.sql
sqlite3 "$conflicting_duplicate_db" <<'SQL'
DELETE FROM usage_events;
INSERT INTO usage_events VALUES
  ('fixture-event-conflicting-duplicate', 'legacy-request-conflicting-duplicate', 100000000,
   'openai', 'fixture-model', '/v1/responses',
   'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
   5, 2, 40, 0, NULL, NULL),
  ('fixture-event-conflicting-duplicate', 'legacy-request-conflicting-duplicate', 100000000,
   'openai', 'fixture-model', '/v1/responses',
   'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
   6, 2, 40, 0, NULL, NULL);
SQL
if run_import "$conflicting_duplicate_tenant" "$duplicate_source" \
  "$conflicting_duplicate_db" >"$work_dir/conflicting-duplicate.log" 2>&1; then
  echo "conflicting duplicate import unexpectedly succeeded" >&2
  exit 1
fi
grep -q 'CPAMP import stopped: 1 event hashes map to conflicting source rows' \
  "$work_dir/conflicting-duplicate.log"
conflicting_duplicate_state=$(psql_scalar \
  -v tenant="$conflicting_duplicate_tenant" -v source="$duplicate_source" <<'SQL'
SELECT
  (SELECT count(*) FROM tenants WHERE external_id = :'tenant') || '|' ||
  (SELECT count(*) FROM cpamp_import_checkpoints
    WHERE tenant_external_id = :'tenant' AND source = :'source');
SQL
)
assert_equal "$conflicting_duplicate_state" "0|0" \
  "same event hash with different payload fails closed"
echo "phase=conflicting-payload-duplicate exit=nonzero tenant_rows=0 checkpoints=0"

sqlite3 "$source_db" < /work/initial.sql
run_import "$tenant" "$source" "$source_db"
run_import "$tenant" "$source" "$source_db"

tenant_id=$(psql_scalar -v tenant="$tenant" <<'SQL'
SELECT id FROM tenants WHERE external_id = :'tenant';
SQL
)
initial_source=$(sqlite3 "$source_db" 'SELECT count(*) || '"'"'|'"'"' || count(DISTINCT event_hash) || '"'"'|'"'"' || sum(input_tokens) || '"'"'|'"'"' || sum(output_tokens) FROM usage_events;')
assert_equal "$initial_source" "2|2|28|8" "initial SQLite source totals"
initial_target=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT count(*) || '|' || count(DISTINCT id) || '|' || sum(input_tokens) || '|' ||
       sum(output_tokens) || '|' || sum(cost_micros) || '|' ||
       count(*) FILTER (WHERE error_code = 'http_502')
  FROM request_records WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$initial_target" "2|2|28|8|88|1" "initial PostgreSQL request totals"
initial_locators=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT count(*) || '|' || count(*) FILTER (
         WHERE EXISTS (
           SELECT 1 FROM request_records r
            WHERE r.id = l.id AND r.created_at = l.created_at
              AND r.tenant_id = l.tenant_id AND r.key_id = l.key_id
         )
       )
  FROM request_record_locators l WHERE l.tenant_id = :'tenant_id';
SQL
)
assert_equal "$initial_locators" "2|2" "initial request locator coverage"
initial_account_usage_state=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT count(*) || '|' || count(s.account_id)
  FROM credit_accounts a
  LEFT JOIN account_usage_state s ON s.account_id = a.id
 WHERE a.tenant_id = :'tenant_id';
SQL
)
assert_equal "$initial_account_usage_state" "1|1" "imported account usage-state coverage"
initial_aggregate=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT sum(a.requests) || '|' || sum(a.input_tokens) || '|' ||
       sum(a.output_tokens) || '|' || sum(a.cost_micros)
  FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id
 WHERE k.tenant_id = :'tenant_id';
SQL
)
assert_equal "$initial_aggregate" "2|28|8|88" "initial PostgreSQL aggregate totals"
initial_checkpoint=$(psql_scalar -v tenant="$tenant" -v source="$source" <<'SQL'
SELECT watermark_ms || '|' || watermark_hash || '|' || imported_events
  FROM cpamp_import_checkpoints
 WHERE tenant_external_id = :'tenant' AND source = :'source';
SQL
)
assert_equal "$initial_checkpoint" "300000000|fixture-event-initial-b|2" "initial checkpoint"
body_samples=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT count(*) FILTER (
         WHERE request_object LIKE 'gap://cpamp/fixture-event-initial-b/request'
           AND response_object = 'gap://cpamp/fixture-event-initial-b/response'
       ) || '|' || count(*) FILTER (
         WHERE request_object LIKE 'gap://cpamp/fixture-event-initial-a/request'
           AND response_object = 'gap://cpamp/fixture-event-initial-a/response'
       )
  FROM request_records WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$body_samples" "1|1" "request/error body samples"
echo "phase=initial source=2 target=2 distinct_ids=2 errors=1 totals=28/8/88 checkpoint=300000000"

sqlite3 "$source_db" "INSERT INTO usage_events VALUES ('fixture-event-late-overlap', 'legacy-request-late', 299000000, 'openai', 'fixture-model', '/v1/responses', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 19, 7, 90, 0, NULL, NULL);"
run_import "$tenant" "$source" "$source_db"
run_import "$tenant" "$source" "$source_db"
delayed_target=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT count(*) || '|' || count(DISTINCT id) || '|' || sum(input_tokens) || '|' ||
       sum(output_tokens) || '|' || sum(cost_micros)
  FROM request_records WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$delayed_target" "3|3|47|15|154" "late overlap increment"
delayed_checkpoint=$(psql_scalar -v tenant="$tenant" -v source="$source" <<'SQL'
SELECT watermark_ms || '|' || watermark_hash || '|' || imported_events
  FROM cpamp_import_checkpoints
 WHERE tenant_external_id = :'tenant' AND source = :'source';
SQL
)
assert_equal "$delayed_checkpoint" "300000000|fixture-event-initial-b|3" "late overlap checkpoint"
echo "phase=delayed-overlap source=3 target=3 distinct_ids=3 totals=47/15/154 checkpoint=300000000"

sqlite3 "$source_db" "INSERT INTO usage_events VALUES ('fixture-event-new-watermark', 'legacy-request-new', 400000000, 'anthropic', 'fixture-model', '/v1/messages', 'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', 23, 11, 210, 0, NULL, NULL);"
run_import "$tenant" "$source" "$source_db"
final_ids=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT string_agg(id, ',' ORDER BY id)
  FROM request_records WHERE tenant_id = :'tenant_id';
SQL
)
analysis_before_replay=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT
  (SELECT count(*) || ':' || COALESCE(sum(input_tokens), 0) || ':' ||
          COALESCE(sum(output_tokens), 0) || ':' || COALESCE(sum(cost_micros), 0)
     FROM request_stats_facts WHERE tenant_id = :'tenant_id') || '|' ||
  (SELECT count(*) || ':' || COALESCE(sum(requests), 0) || ':' ||
          COALESCE(sum(input_tokens), 0) || ':' || COALESCE(sum(output_tokens), 0) || ':' ||
          COALESCE(sum(cost_micros), 0)
     FROM request_daily_aggregates WHERE tenant_id = :'tenant_id') || '|' ||
  (SELECT count(*) || ':' || COALESCE(sum(requests), 0) || ':' ||
          COALESCE(sum(input_tokens), 0) || ':' || COALESCE(sum(output_tokens), 0) || ':' ||
          COALESCE(sum(cost_micros), 0)
     FROM usage_analysis_hourly WHERE tenant_id = :'tenant_id') || '|' ||
  (SELECT count(*) || ':' || COALESCE(sum(requests), 0) || ':' ||
          COALESCE(sum(input_tokens), 0) || ':' || COALESCE(sum(output_tokens), 0) || ':' ||
          COALESCE(sum(cost_micros), 0)
     FROM usage_analysis_daily WHERE tenant_id = :'tenant_id');
SQL
)
run_import "$tenant" "$source" "$source_db"
replayed_ids=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT string_agg(id, ',' ORDER BY id)
  FROM request_records WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$replayed_ids" "$final_ids" "deterministic request IDs after replay"
analysis_after_replay=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT
  (SELECT count(*) || ':' || COALESCE(sum(input_tokens), 0) || ':' ||
          COALESCE(sum(output_tokens), 0) || ':' || COALESCE(sum(cost_micros), 0)
     FROM request_stats_facts WHERE tenant_id = :'tenant_id') || '|' ||
  (SELECT count(*) || ':' || COALESCE(sum(requests), 0) || ':' ||
          COALESCE(sum(input_tokens), 0) || ':' || COALESCE(sum(output_tokens), 0) || ':' ||
          COALESCE(sum(cost_micros), 0)
     FROM request_daily_aggregates WHERE tenant_id = :'tenant_id') || '|' ||
  (SELECT count(*) || ':' || COALESCE(sum(requests), 0) || ':' ||
          COALESCE(sum(input_tokens), 0) || ':' || COALESCE(sum(output_tokens), 0) || ':' ||
          COALESCE(sum(cost_micros), 0)
     FROM usage_analysis_hourly WHERE tenant_id = :'tenant_id') || '|' ||
  (SELECT count(*) || ':' || COALESCE(sum(requests), 0) || ':' ||
          COALESCE(sum(input_tokens), 0) || ':' || COALESCE(sum(output_tokens), 0) || ':' ||
          COALESCE(sum(cost_micros), 0)
     FROM usage_analysis_daily WHERE tenant_id = :'tenant_id');
SQL
)
assert_equal "$analysis_after_replay" "$analysis_before_replay" "analytics remain stable after replay"
final_source=$(sqlite3 "$source_db" 'SELECT count(*) || '"'"'|'"'"' || count(DISTINCT event_hash) || '"'"'|'"'"' || sum(input_tokens) || '"'"'|'"'"' || sum(output_tokens) FROM usage_events;')
assert_equal "$final_source" "4|4|70|26" "final SQLite source totals"
final_target=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT count(*) || '|' || count(DISTINCT id) || '|' ||
       count(DISTINCT reservation_id) || '|' || sum(input_tokens) || '|' ||
       sum(output_tokens) || '|' || sum(cost_micros)
  FROM request_records WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$final_target" "4|4|4|70|26|244" "final PostgreSQL request totals"
final_locators=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT count(*) || '|' || count(*) FILTER (
         WHERE EXISTS (
           SELECT 1 FROM request_records r
            WHERE r.id = l.id AND r.created_at = l.created_at
              AND r.tenant_id = l.tenant_id AND r.key_id = l.key_id
         )
       )
  FROM request_record_locators l WHERE l.tenant_id = :'tenant_id';
SQL
)
assert_equal "$final_locators" "4|4" "final request locator coverage"
final_aggregate=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT sum(a.requests) || '|' || sum(a.input_tokens) || '|' ||
       sum(a.output_tokens) || '|' || sum(a.cost_micros)
  FROM usage_daily_aggregates a JOIN key_records k ON k.id = a.key_id
 WHERE k.tenant_id = :'tenant_id';
SQL
)
assert_equal "$final_aggregate" "4|70|26|244" "final PostgreSQL aggregate totals"
final_facts=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT count(*) || '|' || count(DISTINCT request_id) || '|' ||
       sum(input_tokens) || '|' || sum(output_tokens) || '|' || sum(cost_micros) || '|' ||
       sum(duration_ms) || '|' || count(*) FILTER (WHERE status_class = 'success') || '|' ||
       count(*) FILTER (WHERE status_class = 'failure' AND error_code = 'http_502') || '|' ||
       count(*) FILTER (WHERE protocol = 'openai') || '|' ||
       count(*) FILTER (WHERE protocol = 'anthropic') || '|' ||
       count(*) FILTER (WHERE service_tier = 'default' AND currency = 'USD') || '|' ||
       count(*) FILTER (WHERE upstream_account_id = '' AND model_route_id = '')
  FROM request_stats_facts WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$final_facts" "4|4|70|26|244|600|3|1|3|1|4|4" "v24 request fact dimensions"
final_request_daily=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT sum(requests) || '|' || sum(input_tokens) || '|' || sum(output_tokens) || '|' ||
       sum(cached_input_tokens) || '|' || sum(cache_write_tokens) || '|' ||
       sum(duration_count) || '|' || sum(duration_sum_ms) || '|' || sum(cost_micros) || '|' ||
       sum(requests) FILTER (WHERE protocol = 'openai') || '|' ||
       sum(requests) FILTER (WHERE protocol = 'anthropic') || '|' ||
       sum(requests) FILTER (WHERE status_class = 'failure' AND error_code = 'http_502') || '|' ||
       count(DISTINCT currency) || '|' || min(currency)
  FROM request_daily_aggregates WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$final_request_daily" "4|70|26|0|0|4|600|244|3|1|1|1|USD" "v24 request daily dimensions"
final_hourly_analysis=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT sum(requests) || '|' || sum(input_tokens) || '|' || sum(output_tokens) || '|' ||
       sum(cached_input_tokens) || '|' || sum(cache_write_tokens) || '|' ||
       sum(generation_units) || '|' || sum(duration_count) || '|' ||
       sum(duration_sum_ms) || '|' || sum(cost_micros) || '|' ||
       sum(duration_bucket_2) || '|' || sum(duration_bucket_3) || '|' ||
       sum(duration_bucket_0 + duration_bucket_1 + duration_bucket_2 + duration_bucket_3 +
           duration_bucket_4 + duration_bucket_5 + duration_bucket_6 + duration_bucket_7 +
           duration_bucket_8 + duration_bucket_9 + duration_bucket_10 + duration_bucket_11) || '|' ||
       sum(requests) FILTER (WHERE protocol = 'openai') || '|' ||
       sum(requests) FILTER (WHERE protocol = 'anthropic') || '|' ||
       sum(requests) FILTER (WHERE status_class = 'failure' AND error_code = 'http_502') || '|' ||
       count(*) FILTER (WHERE source_kind <> 'request' OR service_tier <> 'default' OR currency <> 'USD')
  FROM usage_analysis_hourly WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$final_hourly_analysis" "4|70|26|0|0|0|4|600|244|1|3|4|3|1|1|0" "v24 hourly analysis dimensions"
final_daily_analysis=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT sum(requests) || '|' || sum(input_tokens) || '|' || sum(output_tokens) || '|' ||
       sum(cached_input_tokens) || '|' || sum(cache_write_tokens) || '|' ||
       sum(generation_units) || '|' || sum(duration_count) || '|' ||
       sum(duration_sum_ms) || '|' || sum(cost_micros) || '|' ||
       sum(duration_bucket_2) || '|' || sum(duration_bucket_3) || '|' ||
       sum(duration_bucket_0 + duration_bucket_1 + duration_bucket_2 + duration_bucket_3 +
           duration_bucket_4 + duration_bucket_5 + duration_bucket_6 + duration_bucket_7 +
           duration_bucket_8 + duration_bucket_9 + duration_bucket_10 + duration_bucket_11) || '|' ||
       sum(requests) FILTER (WHERE protocol = 'openai') || '|' ||
       sum(requests) FILTER (WHERE protocol = 'anthropic') || '|' ||
       sum(requests) FILTER (WHERE status_class = 'failure' AND error_code = 'http_502') || '|' ||
       count(*) FILTER (WHERE source_kind <> 'request' OR service_tier <> 'default' OR currency <> 'USD')
  FROM usage_analysis_daily WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$final_daily_analysis" "4|70|26|0|0|0|4|600|244|1|3|4|3|1|1|0" "v24 daily analysis dimensions"
echo "phase=v24-analytics facts=4 request_daily=4 hourly=4 daily=4 currency=USD protocols=3-openai/1-anthropic errors=http_502:1 durations=600ms buckets=1/3 replay=stable"
final_checkpoint=$(psql_scalar -v tenant="$tenant" -v source="$source" <<'SQL'
SELECT watermark_ms || '|' || watermark_hash || '|' || imported_events
  FROM cpamp_import_checkpoints
 WHERE tenant_external_id = :'tenant' AND source = :'source';
SQL
)
assert_equal "$final_checkpoint" "400000000|fixture-event-new-watermark|4" "final checkpoint"
echo "phase=new-watermark source=4 target=4 distinct_ids=4 totals=70/26/244 checkpoint=400000000 replay=stable"

run_import "$other_tenant" "$source" "$source_db"
run_import "$tenant" "$other_source" "$source_db"
isolated_checkpoints=$(psql_scalar -v tenant="$tenant" -v other_tenant="$other_tenant" -v source="$source" -v other_source="$other_source" <<'SQL'
SELECT count(*) || '|' || count(DISTINCT tenant_external_id || ':' || source)
  FROM cpamp_import_checkpoints
 WHERE (tenant_external_id = :'tenant' AND source IN (:'source', :'other_source'))
    OR (tenant_external_id = :'other_tenant' AND source = :'source');
SQL
)
assert_equal "$isolated_checkpoints" "3|3" "tenant and source checkpoint isolation"
scoped_request_ids=$(psql_scalar -v tenant="$tenant" -v other_tenant="$other_tenant" <<'SQL'
SELECT count(*) || '|' || count(DISTINCT r.id) || '|' ||
       (SELECT count(*) FROM request_stats_facts f WHERE f.tenant_id IN (
          SELECT id FROM tenants WHERE external_id IN (:'tenant', :'other_tenant')
        )) || '|' ||
       (SELECT sum(requests) FROM usage_analysis_hourly h WHERE h.tenant_id IN (
          SELECT id FROM tenants WHERE external_id IN (:'tenant', :'other_tenant')
        )) || '|' ||
       (SELECT sum(requests) FROM usage_analysis_daily d WHERE d.tenant_id IN (
          SELECT id FROM tenants WHERE external_id IN (:'tenant', :'other_tenant')
        ))
  FROM request_records r JOIN tenants t ON t.id = r.tenant_id
 WHERE t.external_id IN (:'tenant', :'other_tenant');
SQL
)
assert_equal "$scoped_request_ids" "12|12|12|12|12" "tenant/source-scoped deterministic IDs and analytics"
echo "phase=checkpoint-isolation checkpoint_pairs=3/3 scoped_request_ids=12/12 scoped_analytics=12/12/12"

sqlite3 "$unmapped_db" < /work/initial.sql
sqlite3 "$unmapped_db" "DELETE FROM usage_events; INSERT INTO usage_events VALUES ('fixture-event-unmapped', 'legacy-unmapped', 500000000, 'openai', 'fixture-model', '/v1/responses', 'invalid-hash', 1, 1, 10, 0, NULL, NULL); DELETE FROM api_key_aliases;"
if run_import "$unmapped_tenant" "$source" "$unmapped_db" >"$work_dir/unmapped.log" 2>&1; then
  echo "unmapped import unexpectedly succeeded" >&2
  exit 1
fi
grep -q 'staged events have no supported key identity' "$work_dir/unmapped.log"
unmapped_state=$(psql_scalar -v tenant="$unmapped_tenant" <<'SQL'
SELECT (SELECT count(*) FROM tenants WHERE external_id = :'tenant') || '|' ||
       (SELECT count(*) FROM cpamp_import_checkpoints WHERE tenant_external_id = :'tenant');
SQL
)
assert_equal "$unmapped_state" "0|0" "unmapped fail-closed target state"
echo "phase=unmapped-fail-closed exit=nonzero tenant_rows=0 checkpoints=0"

reset_tenant="cpa-dogfood-import"
reset_source="cpamp-reset-guard:$ACCEPTANCE_RUN_ID"
run_import "$reset_tenant" "$reset_source" "$source_db"
psql_target -v tenant="$reset_tenant" <<'SQL'
WITH identity AS (SELECT md5('cpamp-reset-provider-guard') AS value)
INSERT INTO upstream_accounts
  (id, tenant_id, name, driver, auth_kind, config_json, status,
   credential_generation, created_at, updated_at)
SELECT substr(value,1,8)||'-'||substr(value,9,4)||'-5'||substr(value,14,3)||'-a'||substr(value,18,3)||'-'||substr(value,21,12),
       t.id, 'operator-owned-provider', 'http-json', 'none',
       '{"base_url":"https://api.example.test"}', 'active', 1, 1, 1
  FROM identity CROSS JOIN tenants t WHERE t.external_id = :'tenant';
SQL
if CPAMP_SQLITE_PATH="$source_db" \
  IMPORT_TENANT_EXTERNAL_ID="$reset_tenant" \
  CPAMP_IMPORT_SOURCE="$reset_source" \
  CPAMP_RESET_IMPORT=true \
  CPAMP_RESET_CONFIRM=DELETE_CPA_DOGFOOD_IMPORT \
  CPAMP_ALLOW_UNMAPPED=false \
  sh /work/migrate-cpamp.sh >"$work_dir/reset-guard.log" 2>&1; then
  echo "reset with an operator-owned provider unexpectedly succeeded" >&2
  exit 1
fi
grep -q 'tenant has provider accounts or model routes not owned by the usage importer' "$work_dir/reset-guard.log"
reset_guard_state=$(psql_scalar -v tenant="$reset_tenant" <<'SQL'
SELECT (SELECT count(*) FROM upstream_accounts u JOIN tenants t ON t.id = u.tenant_id WHERE t.external_id = :'tenant') || '|' ||
       (SELECT count(*) FROM request_records r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = :'tenant') || '|' ||
       (SELECT sum(requests) FROM usage_analysis_hourly h JOIN tenants t ON t.id = h.tenant_id WHERE t.external_id = :'tenant') || '|' ||
       (SELECT sum(requests) FROM usage_analysis_daily d JOIN tenants t ON t.id = d.tenant_id WHERE t.external_id = :'tenant');
SQL
)
assert_equal "$reset_guard_state" "1|4|4|4" "reset guard preserves provider and imported usage"
echo "phase=reset-provider-fail-closed upstreams=1 requests=4 analytics=4/4"

echo "CPAMP PostgreSQL acceptance: PASS schema=$ACCEPTANCE_SCHEMA"
