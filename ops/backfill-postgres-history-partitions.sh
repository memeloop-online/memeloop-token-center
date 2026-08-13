#!/bin/sh
set -eu

usage() {
  cat <<'USAGE'
Usage: backfill-postgres-history-partitions.sh [options]

Read-only dry-run is the default. PostgreSQL libpq variables PGHOST, PGUSER,
PGPASSWORD and PGDATABASE are required; PGPORT defaults to 5432.

Options:
  --apply                 Install operational SQL/indexes and move data.
  --indexes-only          With --apply, install/verify indexes but move no rows.
  --table NAME            request_records, request_events, or all (default all).
  --from YYYY-MM-DD       Include completed UTC days on/after this date.
  --before YYYY-MM-DD     Exclude days on/after this date (default today UTC).
  --batch-size N          Copy transaction size, 1..100000 (default 10000).
  --max-days N            Maximum days processed per table (default 1).
  --help                  Show this help.

Examples:
  # Safe inventory only:
  ./ops/backfill-postgres-history-partitions.sh

  # Move one oldest completed request day, in 5,000-row copy transactions:
  ./ops/backfill-postgres-history-partitions.sh --apply \
    --table request_records --batch-size 5000 --max-days 1

Repeat the apply command until the dry-run reports zero default-partition rows.
USAGE
}

ACTION=dry-run
INDEXES_ONLY=false
TABLE_SELECTION=all
FROM_DATE=
BEFORE_DATE=$(date -u +%F)
BATCH_SIZE=10000
MAX_DAYS=1

while [ "$#" -gt 0 ]; do
  case "$1" in
    --apply) ACTION=apply ;;
    --indexes-only) INDEXES_ONLY=true ;;
    --table)
      [ "$#" -ge 2 ] || { echo "--table requires a value" >&2; exit 2; }
      TABLE_SELECTION=$2
      shift
      ;;
    --from)
      [ "$#" -ge 2 ] || { echo "--from requires a value" >&2; exit 2; }
      FROM_DATE=$2
      shift
      ;;
    --before)
      [ "$#" -ge 2 ] || { echo "--before requires a value" >&2; exit 2; }
      BEFORE_DATE=$2
      shift
      ;;
    --batch-size)
      [ "$#" -ge 2 ] || { echo "--batch-size requires a value" >&2; exit 2; }
      BATCH_SIZE=$2
      shift
      ;;
    --max-days)
      [ "$#" -ge 2 ] || { echo "--max-days requires a value" >&2; exit 2; }
      MAX_DAYS=$2
      shift
      ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

case "$TABLE_SELECTION" in
  request_records|request_events|all) ;;
  *) echo "--table must be request_records, request_events, or all" >&2; exit 2 ;;
esac
case "$BATCH_SIZE" in *[!0-9]*|'') echo "--batch-size must be an integer" >&2; exit 2 ;; esac
case "$MAX_DAYS" in *[!0-9]*|'') echo "--max-days must be an integer" >&2; exit 2 ;; esac
[ "$BATCH_SIZE" -ge 1 ] && [ "$BATCH_SIZE" -le 100000 ] || {
  echo "--batch-size must be between 1 and 100000" >&2
  exit 2
}
[ "$MAX_DAYS" -ge 1 ] || { echo "--max-days must be at least 1" >&2; exit 2; }
[ "$INDEXES_ONLY" = false ] || [ "$ACTION" = apply ] || {
  echo "--indexes-only requires --apply" >&2
  exit 2
}

validate_date() {
  value=$1
  case "$value" in
    ????-??-??) ;;
    *) echo "invalid UTC date: $value" >&2; exit 2 ;;
  esac
  normalized=$(date -u -d "$value" +%F 2>/dev/null || true)
  [ "$normalized" = "$value" ] || { echo "invalid UTC date: $value" >&2; exit 2; }
}

validate_date "$BEFORE_DATE"
if [ -n "$FROM_DATE" ]; then
  validate_date "$FROM_DATE"
  from_epoch=$(date -u -d "$FROM_DATE" +%s)
  before_epoch=$(date -u -d "$BEFORE_DATE" +%s)
  [ "$from_epoch" -lt "$before_epoch" ] || {
    echo "--from must be earlier than --before" >&2
    exit 2
  }
fi

: "${PGHOST:?PGHOST is required}"
: "${PGPORT:=5432}"
: "${PGUSER:?PGUSER is required}"
: "${PGPASSWORD:?PGPASSWORD is required}"
: "${PGDATABASE:?PGDATABASE is required}"
export PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE

SCRIPT_DIRECTORY=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
POSTGRES_DIRECTORY=$SCRIPT_DIRECTORY/postgres

psql_target() {
  psql -X --no-psqlrc -v ON_ERROR_STOP=1 "$@"
}

tables() {
  case "$TABLE_SELECTION" in
    all) printf '%s\n' request_records request_events ;;
    *) printf '%s\n' "$TABLE_SELECTION" ;;
  esac
}

time_column() {
  case "$1" in
    request_records) printf '%s\n' created_at ;;
    request_events) printf '%s\n' event_at ;;
  esac
}

identity_column() {
  case "$1" in
    request_records) printf '%s\n' id ;;
    request_events) printf '%s\n' event_id ;;
  esac
}

date_predicate() {
  column=$1
  if [ -n "$FROM_DATE" ]; then
    printf '%s' "$column >= ((:'from_date'::date::timestamp AT TIME ZONE 'UTC')::timestamptz) AND "
  fi
  printf '%s' "$column < ((:'before_date'::date::timestamp AT TIME ZONE 'UTC')::timestamptz)"
}

plan_table() {
  table_name=$1
  column=$(time_column "$table_name")
  default_table=${table_name}_default
  predicate=$(date_predicate "to_timestamp($column / 1000.0)")

  echo "Plan for public.$default_table (completed UTC days only):"
  psql_target -v from_date="$FROM_DATE" -v before_date="$BEFORE_DATE" <<SQL
SELECT to_char(to_timestamp($column / 1000.0) AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS utc_day,
       count(*) AS rows,
       min($column) AS minimum_millis,
       max($column) AS maximum_millis
  FROM public.$default_table
 WHERE $predicate
 GROUP BY 1
 ORDER BY 1
 LIMIT $MAX_DAYS;

SELECT count(*) AS selected_rows,
       pg_size_pretty(pg_total_relation_size('public.$default_table')) AS current_default_size
  FROM public.$default_table
 WHERE $predicate;
SQL

  psql_target -v table_name="$table_name" <<'SQL'
SELECT table_name, day_start, status, source_rows, staged_rows, moved_rows,
       batch_size, updated_at, completed_at
  FROM public.mtc_history_partition_backfill_state
 WHERE to_regclass('public.mtc_history_partition_backfill_state') IS NOT NULL
   AND table_name = :'table_name'
 ORDER BY day_start;
SQL
}

state_table_exists() {
  [ "$(psql_target -Atc "SELECT to_regclass('public.mtc_history_partition_backfill_state') IS NOT NULL")" = t ]
}

dry_run() {
  echo "DRY RUN: no schema or data changes will be made."
  for table_name in $(tables); do
    if ! state_table_exists; then
      echo "Operational state table is not installed yet; showing source inventory only."
      column=$(time_column "$table_name")
      default_table=${table_name}_default
      predicate=$(date_predicate "to_timestamp($column / 1000.0)")
      echo "Plan for public.$default_table (completed UTC days only):"
      psql_target -v from_date="$FROM_DATE" -v before_date="$BEFORE_DATE" <<SQL
SELECT to_char(to_timestamp($column / 1000.0) AT TIME ZONE 'UTC', 'YYYY-MM-DD') AS utc_day,
       count(*) AS rows,
       min($column) AS minimum_millis,
       max($column) AS maximum_millis
  FROM public.$default_table
 WHERE $predicate
 GROUP BY 1
 ORDER BY 1
 LIMIT $MAX_DAYS;
SELECT count(*) AS selected_rows,
       pg_size_pretty(pg_total_relation_size('public.$default_table')) AS current_default_size
  FROM public.$default_table
 WHERE $predicate;
SQL
    else
      plan_table "$table_name"
    fi
  done

  psql_target <<'SQL'
SELECT index_name, installed, valid
  FROM (
    VALUES
      ('request_records_recent_idx'),
      ('request_records_tenant_time_idx'),
      ('request_records_key_time_idx'),
      ('request_events_global_cursor_idx'),
      ('request_events_tenant_cursor_idx')
  ) required(index_name)
  CROSS JOIN LATERAL (
    SELECT to_regclass('public.' || required.index_name) IS NOT NULL AS installed,
           COALESCE((
             SELECT indisvalid FROM pg_index
              WHERE indexrelid = to_regclass('public.' || required.index_name)
           ), false) AS valid
  ) status
 ORDER BY index_name;
SQL
  echo "Re-run with --apply to install indexes and process at most $MAX_DAYS day(s) per selected table."
}

index_leaf_name() {
  leaf=$1
  kind=$2
  psql_target -At -v leaf="$leaf" -v kind="$kind" <<'SQL'
SELECT format('mtc_%s_%s_%s', left(:'leaf', 28), :'kind', substr(md5(:'leaf'), 1, 8));
SQL
}

index_is_attached() {
  parent_index=$1
  leaf=$2
  psql_target -At -v parent_index="$parent_index" -v leaf="$leaf" <<'SQL'
SELECT EXISTS (
    SELECT 1
      FROM pg_inherits inheritance
      JOIN pg_index child_index ON child_index.indexrelid = inheritance.inhrelid
     WHERE inheritance.inhparent = to_regclass('public.' || :'parent_index')
       AND child_index.indrelid = to_regclass('public.' || :'leaf')
);
SQL
}

ensure_leaf_index() {
  parent_table=$1
  parent_index=$2
  kind=$3
  definition=$4

  leaves=$(psql_target -At -v parent_table="$parent_table" <<'SQL'
SELECT child.relname
  FROM pg_inherits inheritance
  JOIN pg_class parent ON parent.oid = inheritance.inhparent
  JOIN pg_namespace parent_namespace ON parent_namespace.oid = parent.relnamespace
  JOIN pg_class child ON child.oid = inheritance.inhrelid
  JOIN pg_namespace child_namespace ON child_namespace.oid = child.relnamespace
 WHERE parent_namespace.nspname = 'public'
   AND child_namespace.nspname = 'public'
   AND parent.relname = :'parent_table'
 ORDER BY child.relname;
SQL
)

  for leaf in $leaves; do
    if [ "$(index_is_attached "$parent_index" "$leaf")" = t ]; then
      continue
    fi
    leaf_index=$(index_leaf_name "$leaf" "$kind")
    index_status=$(psql_target -At -v leaf_index="$leaf_index" <<'SQL'
SELECT CASE
         WHEN candidate.indexrelid IS NULL THEN 'missing'
         WHEN indisvalid AND indisready THEN 'valid'
         ELSE 'invalid'
       END
  FROM (SELECT to_regclass('public.' || :'leaf_index') AS indexrelid) candidate
  LEFT JOIN pg_index ON pg_index.indexrelid = candidate.indexrelid;
SQL
)
    if [ "$index_status" = invalid ]; then
      echo "Dropping invalid interrupted index public.$leaf_index"
      psql_target -v leaf_index="$leaf_index" <<'SQL'
DROP INDEX CONCURRENTLY IF EXISTS public.:"leaf_index";
SQL
      index_status=missing
    fi
    if [ "$index_status" = missing ]; then
      echo "Building public.$leaf_index concurrently on public.$leaf"
      psql_target -v leaf="$leaf" -v leaf_index="$leaf_index" <<SQL
CREATE INDEX CONCURRENTLY :"leaf_index" ON public.:"leaf" ($definition);
SQL
    fi
    echo "Attaching public.$leaf_index to public.$parent_index"
    psql_target -v parent_index="$parent_index" -v leaf_index="$leaf_index" <<'SQL'
ALTER INDEX public.:"parent_index" ATTACH PARTITION public.:"leaf_index";
SQL
  done

  parent_valid=$(psql_target -At -v parent_index="$parent_index" <<'SQL'
SELECT indisvalid AND indisready
  FROM pg_index
 WHERE indexrelid = to_regclass('public.' || :'parent_index');
SQL
)
  [ "$parent_valid" = t ] || {
    echo "partitioned index public.$parent_index remains invalid; a partition may have appeared concurrently, rerun --apply --indexes-only" >&2
    exit 1
  }
}

install_indexes() {
  echo "Installing partitioned-index metadata. Leaf builds use CREATE INDEX CONCURRENTLY."
  psql_target -f "$POSTGRES_DIRECTORY/history-partition-indexes.sql"
  ensure_leaf_index request_records request_records_recent_idx recent \
    'created_at DESC, id DESC'
  ensure_leaf_index request_records request_records_tenant_time_idx tenant_time \
    'tenant_id, created_at DESC, id DESC'
  ensure_leaf_index request_records request_records_key_time_idx key_time \
    'key_id, created_at DESC, id DESC'
  ensure_leaf_index request_events request_events_global_cursor_idx global_cursor \
    'event_at ASC, event_id ASC'
  ensure_leaf_index request_events request_events_tenant_cursor_idx tenant_cursor \
    'tenant_id, event_at ASC, event_id ASC'
}

selected_days() {
  table_name=$1
  column=$(time_column "$table_name")
  default_table=${table_name}_default
  predicate=$(date_predicate "to_timestamp($column / 1000.0)")
  psql_target -At -v from_date="$FROM_DATE" -v before_date="$BEFORE_DATE" <<SQL
SELECT to_char(to_timestamp($column / 1000.0) AT TIME ZONE 'UTC', 'YYYY-MM-DD')
  FROM public.$default_table
 WHERE $predicate
 GROUP BY 1
 ORDER BY 1
 LIMIT $MAX_DAYS;
SQL
}

backfill_day() {
  table_name=$1
  day=$2
  echo "Backfilling $table_name UTC day $day in committed batches of $BATCH_SIZE"
  psql_target -v table_name="$table_name" -v day="$day" -v batch_size="$BATCH_SIZE" <<'SQL'
SELECT pg_try_advisory_lock(
           hashtext('memeloop-token-center-history-partition'),
           hashtext(:'table_name')
       ) AS acquired \gset
\if :acquired
CALL public.mtc_backfill_history_partition(
    :'table_name', :'day'::date, :batch_size
);
SELECT pg_advisory_unlock(
           hashtext('memeloop-token-center-history-partition'),
           hashtext(:'table_name')
       );
\else
\echo 'another history backfill holds the PostgreSQL advisory lock'
\quit 75
\endif
SQL

  suffix=$(printf '%s' "$day" | tr -d '-')
  target=${table_name}_${suffix}
  psql_target -v target="$target" <<'SQL'
ANALYZE public.:"target";
SQL
  psql_target -v table_name="$table_name" -v day="$day" <<'SQL'
SELECT table_name, day_start, status, source_rows, staged_rows, moved_rows,
       source_rows = staged_rows AND staged_rows = moved_rows AS counts_match,
       completed_at
  FROM public.mtc_history_partition_backfill_state
 WHERE table_name = :'table_name' AND day_start = :'day'::date;
SQL
}

if [ "$ACTION" = dry-run ]; then
  dry_run
  exit 0
fi

echo "APPLY mode selected. Each daily cutover is atomic and independently restartable."
psql_target -f "$POSTGRES_DIRECTORY/history-partition-backfill.sql"
install_indexes

if [ "$INDEXES_ONLY" = true ]; then
  echo "Index installation and verification complete; no rows were moved."
  exit 0
fi

for table_name in $(tables); do
  days=$(selected_days "$table_name")
  if [ -z "$days" ]; then
    echo "No selected completed UTC days remain in public.${table_name}_default."
    continue
  fi
  for day in $days; do
    backfill_day "$table_name" "$day"
  done
done

echo "Apply run complete. Run again without --apply to inspect remaining default-partition rows."
