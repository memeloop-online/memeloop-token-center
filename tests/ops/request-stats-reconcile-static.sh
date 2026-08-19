#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
driver=$repository/ops/reconcile-postgres-request-stats.sh
day_sql=$repository/ops/postgres/reconcile-observability-day.sql

sh -n "$driver"
"$driver" --help >/dev/null
[ -f "$day_sql" ] || { echo "missing observability day rebuild SQL" >&2; exit 1; }

assert_contains() {
  pattern=$1
  grep -F -- "$pattern" "$driver" >/dev/null || {
    echo "missing request-stats safety marker: $pattern" >&2
    exit 1
  }
}

assert_contains 'ACTION=dry-run'
assert_contains '--confirm-prune'
assert_contains 'pg_advisory_xact_lock'
assert_contains 'mtc_request_stats_prune_guard'
assert_contains 'DELETE FROM request_daily_aggregates'
assert_contains 'DELETE FROM generation_daily_aggregates'
assert_contains 'DELETE FROM usage_analysis_hourly'
assert_contains 'DELETE FROM usage_analysis_daily'
assert_contains 'DELETE FROM generation_stats_facts'
assert_contains 'DRY RUN: no request statistics were changed'

assert_sql_contains() {
  pattern=$1
  grep -F -- "$pattern" "$day_sql" >/dev/null || {
    echo "missing observability rebuild marker: $pattern" >&2
    exit 1
  }
}

assert_sql_contains 'SET TRANSACTION ISOLATION LEVEL SERIALIZABLE'
assert_sql_contains 'IN SHARE ROW EXCLUSIVE MODE'
assert_sql_contains 'ON CONFLICT (request_id) DO UPDATE SET'
assert_sql_contains 'cached_input_tokens, cache_write_tokens'
assert_sql_contains 'service_tier, currency'
assert_sql_contains "'request'"
assert_sql_contains "'generation'"
assert_sql_contains 'INSERT INTO usage_analysis_hourly'
assert_sql_contains 'INSERT INTO usage_analysis_daily'
assert_sql_contains 'FROM usage_analysis_hourly h'

echo 'request statistics reconciliation static safety checks passed'
