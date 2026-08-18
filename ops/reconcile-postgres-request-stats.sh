#!/bin/sh
set -eu

usage() {
  cat <<'USAGE'
Usage: reconcile-postgres-request-stats.sh [options]

Read-only inventory is the default. PostgreSQL libpq variables PGHOST, PGUSER,
PGPASSWORD and PGDATABASE are required; PGPORT defaults to 5432.

Options:
  --apply                 Rebuild selected completed UTC days.
  --from YYYY-MM-DD       Include days on/after this date.
  --before YYYY-MM-DD     Exclude days on/after this date (default today UTC).
  --max-days N            Maximum days rebuilt (default 1).
  --prune-before DATE     Inventory or prune stats strictly before a UTC date.
  --confirm-prune         Required with --apply --prune-before.
  --help                  Show this help.

The prune mode refuses to run while request_records still contains a row before
the cutoff. Archive retention must be verified separately before pruning facts.
USAGE
}

ACTION=dry-run
FROM_DATE=
BEFORE_DATE=$(date -u +%F)
MAX_DAYS=1
PRUNE_BEFORE=
CONFIRM_PRUNE=false

while [ "$#" -gt 0 ]; do
  case "$1" in
    --apply) ACTION=apply ;;
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
    --max-days)
      [ "$#" -ge 2 ] || { echo "--max-days requires a value" >&2; exit 2; }
      MAX_DAYS=$2
      shift
      ;;
    --prune-before)
      [ "$#" -ge 2 ] || { echo "--prune-before requires a value" >&2; exit 2; }
      PRUNE_BEFORE=$2
      shift
      ;;
    --confirm-prune) CONFIRM_PRUNE=true ;;
    --help|-h) usage; exit 0 ;;
    *) echo "unknown option: $1" >&2; usage >&2; exit 2 ;;
  esac
  shift
done

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
if [ -n "$FROM_DATE" ]; then validate_date "$FROM_DATE"; fi
if [ -n "$PRUNE_BEFORE" ]; then validate_date "$PRUNE_BEFORE"; fi
case "$MAX_DAYS" in *[!0-9]*|'') echo "--max-days must be an integer" >&2; exit 2 ;; esac
[ "$MAX_DAYS" -ge 1 ] || { echo "--max-days must be at least 1" >&2; exit 2; }
[ -z "$PRUNE_BEFORE" ] || [ -z "$FROM_DATE" ] || {
  echo "--prune-before cannot be combined with --from" >&2
  exit 2
}
[ "$CONFIRM_PRUNE" = false ] || { [ "$ACTION" = apply ] && [ -n "$PRUNE_BEFORE" ]; } || {
  echo "--confirm-prune requires --apply --prune-before" >&2
  exit 2
}
[ "$ACTION" != apply ] || [ -z "$PRUNE_BEFORE" ] || [ "$CONFIRM_PRUNE" = true ] || {
  echo "--apply --prune-before requires --confirm-prune" >&2
  exit 2
}

: "${PGHOST:?PGHOST is required}"
: "${PGPORT:=5432}"
: "${PGUSER:?PGUSER is required}"
: "${PGPASSWORD:?PGPASSWORD is required}"
: "${PGDATABASE:?PGDATABASE is required}"
export PGHOST PGPORT PGUSER PGPASSWORD PGDATABASE

psql_target() {
  psql -X -v ON_ERROR_STOP=1 --no-psqlrc "$@"
}

if [ -n "$PRUNE_BEFORE" ]; then
  psql_target -v cutoff="$PRUNE_BEFORE" <<'SQL'
WITH boundary AS (
  SELECT (extract(epoch FROM (:'cutoff'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AS millis
)
SELECT :'cutoff' AS prune_before_utc,
       (SELECT count(*) FROM request_records, boundary WHERE created_at < boundary.millis) AS retained_raw_rows,
       (SELECT count(*) FROM request_stats_facts, boundary WHERE created_at < boundary.millis) AS fact_rows,
       (SELECT COALESCE(sum(requests), 0) FROM request_daily_aggregates
         WHERE day_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint) AS aggregate_requests;
SQL
  if [ "$ACTION" != apply ]; then
    echo "DRY RUN: no request statistics were pruned."
    exit 0
  fi
  psql_target -v cutoff="$PRUNE_BEFORE" <<'SQL'
BEGIN;
SELECT pg_advisory_xact_lock(hashtextextended('memeloop-token-center:request-stats', 734627102948314));
LOCK TABLE request_records IN SHARE MODE;
CREATE TEMP TABLE mtc_request_stats_prune_guard (
  invalid boolean NOT NULL CHECK (invalid = false)
) ON COMMIT DROP;
INSERT INTO mtc_request_stats_prune_guard (invalid)
SELECT true
 WHERE EXISTS (
   SELECT 1 FROM request_records
    WHERE created_at < (extract(epoch FROM (:'cutoff'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
 );
DELETE FROM request_daily_aggregates
 WHERE day_bucket < (:'cutoff'::date - DATE '1970-01-01')::bigint;
DELETE FROM request_stats_facts
 WHERE created_at < (extract(epoch FROM (:'cutoff'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint;
COMMIT;
SQL
  echo "Pruned request statistics strictly before $PRUNE_BEFORE after verifying raw history was absent."
  exit 0
fi

if [ -n "$FROM_DATE" ]; then
  from_predicate="created_at >= (extract(epoch FROM (:'from_date'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AND"
else
  from_predicate=""
fi

days=$(psql_target -At -v from_date="$FROM_DATE" -v before_date="$BEFORE_DATE" <<SQL
SELECT to_char(to_timestamp(created_at / 1000.0) AT TIME ZONE 'UTC', 'YYYY-MM-DD')
  FROM request_records
 WHERE $from_predicate
       created_at < (extract(epoch FROM (:'before_date'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
   AND completed_at IS NOT NULL AND status_code IS NOT NULL
 GROUP BY 1
 ORDER BY 1
 LIMIT $MAX_DAYS;
SQL
)

if [ -z "$days" ]; then
  echo "No completed UTC request days matched the selected range."
  exit 0
fi

for day in $days; do
  echo "Request statistics UTC day $day:"
  psql_target -v day="$day" <<'SQL'
WITH bounds AS (
  SELECT (extract(epoch FROM (:'day'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AS start_ms,
         (extract(epoch FROM ((:'day'::date + 1)::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint AS end_ms
)
SELECT (SELECT count(*) FROM request_records, bounds
         WHERE created_at >= start_ms AND created_at < end_ms
           AND completed_at IS NOT NULL AND status_code IS NOT NULL) AS terminal_raw_rows,
       (SELECT count(*) FROM request_stats_facts, bounds
         WHERE created_at >= start_ms AND created_at < end_ms) AS fact_rows,
       (SELECT COALESCE(sum(requests), 0) FROM request_daily_aggregates
         WHERE day_bucket = (:'day'::date - DATE '1970-01-01')::bigint) AS aggregate_requests;
SQL
  if [ "$ACTION" != apply ]; then continue; fi
  psql_target -v day="$day" <<'SQL'
BEGIN;
SELECT pg_advisory_xact_lock(hashtextextended('memeloop-token-center:request-stats', 734627102948314));
INSERT INTO request_stats_facts
  (request_id, tenant_id, key_id, created_at, model, protocol, status_class,
   error_code, upstream_account_id, model_route_id, duration_ms,
   input_tokens, output_tokens, cost_micros)
SELECT id, tenant_id, key_id, created_at, model, protocol,
       CASE WHEN status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END,
       COALESCE(error_code, ''), COALESCE(upstream_account_id, ''),
       COALESCE(model_route_id, ''), COALESCE(duration_ms, 0),
       input_tokens, output_tokens, cost_micros
  FROM request_records
 WHERE created_at >= (extract(epoch FROM (:'day'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
   AND created_at < (extract(epoch FROM ((:'day'::date + 1)::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
   AND completed_at IS NOT NULL AND status_code IS NOT NULL
ON CONFLICT (request_id) DO UPDATE SET
  tenant_id = excluded.tenant_id, key_id = excluded.key_id,
  created_at = excluded.created_at, model = excluded.model, protocol = excluded.protocol,
  status_class = excluded.status_class, error_code = excluded.error_code,
  upstream_account_id = excluded.upstream_account_id,
  model_route_id = excluded.model_route_id, duration_ms = excluded.duration_ms,
  input_tokens = excluded.input_tokens, output_tokens = excluded.output_tokens,
  cost_micros = excluded.cost_micros;
DELETE FROM request_daily_aggregates
 WHERE day_bucket = (:'day'::date - DATE '1970-01-01')::bigint;
INSERT INTO request_daily_aggregates
  (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code,
   upstream_account_id, model_route_id, requests, input_tokens,
   output_tokens, cost_micros)
SELECT tenant_id, key_id, created_at / 86400000, model, protocol, status_class,
       error_code, upstream_account_id, model_route_id, COUNT(*),
       SUM(input_tokens), SUM(output_tokens), SUM(cost_micros)
  FROM request_stats_facts
 WHERE created_at >= (extract(epoch FROM (:'day'::date::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
   AND created_at < (extract(epoch FROM ((:'day'::date + 1)::timestamp AT TIME ZONE 'UTC')) * 1000)::bigint
 GROUP BY tenant_id, key_id, created_at / 86400000, model, protocol,
          status_class, error_code, upstream_account_id, model_route_id;
COMMIT;
ANALYZE request_stats_facts;
ANALYZE request_daily_aggregates;
SQL
done

if [ "$ACTION" = dry-run ]; then
  echo "DRY RUN: no request statistics were changed. Re-run with --apply to rebuild at most $MAX_DAYS day(s)."
fi
