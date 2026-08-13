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
tenant="cpamp-$ACCEPTANCE_RUN_ID-main"
other_tenant="cpamp-$ACCEPTANCE_RUN_ID-tenant"
unmapped_tenant="cpamp-$ACCEPTANCE_RUN_ID-unmapped"
source="cpamp-acceptance:$ACCEPTANCE_RUN_ID"
other_source="cpamp-acceptance:$ACCEPTANCE_RUN_ID:source"

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
    /work/migrate-cpamp.sh >/dev/null
}

current_schema=$(psql_target -Atc 'SELECT current_schema()')
assert_equal "$current_schema" "$ACCEPTANCE_SCHEMA" "PostgreSQL search_path isolation"
psql_target -f /work/0001_initial.sql >/dev/null
psql_target -f /work/0002_query_indexes.sql >/dev/null
psql_target -f /work/0019_session_archive_import.sql >/dev/null
psql_target -c 'CREATE TABLE IF NOT EXISTS request_records_default PARTITION OF request_records DEFAULT' >/dev/null
psql_target -f /work/0021_request_locators.sql >/dev/null

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
           AND response_object LIKE '%fixture upstream failure%'
       ) || '|' || count(*) FILTER (
         WHERE request_object LIKE 'gap://cpamp/fixture-event-initial-a/request'
           AND response_object LIKE 'gap://cpamp/%'
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
run_import "$tenant" "$source" "$source_db"
replayed_ids=$(psql_scalar -v tenant_id="$tenant_id" <<'SQL'
SELECT string_agg(id, ',' ORDER BY id)
  FROM request_records WHERE tenant_id = :'tenant_id';
SQL
)
assert_equal "$replayed_ids" "$final_ids" "deterministic request IDs after replay"
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
SELECT count(*) || '|' || count(DISTINCT r.id)
  FROM request_records r JOIN tenants t ON t.id = r.tenant_id
 WHERE t.external_id IN (:'tenant', :'other_tenant');
SQL
)
assert_equal "$scoped_request_ids" "12|12" "tenant/source-scoped deterministic IDs"
echo "phase=checkpoint-isolation checkpoint_pairs=3/3 scoped_request_ids=12/12"

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

echo "CPAMP PostgreSQL acceptance: PASS schema=$ACCEPTANCE_SCHEMA"
