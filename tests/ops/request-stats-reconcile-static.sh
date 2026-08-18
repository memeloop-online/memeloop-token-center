#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
driver=$repository/ops/reconcile-postgres-request-stats.sh

sh -n "$driver"
"$driver" --help >/dev/null

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
assert_contains 'ON CONFLICT (request_id) DO UPDATE SET'
assert_contains 'DELETE FROM request_daily_aggregates'
assert_contains 'DRY RUN: no request statistics were changed'

echo 'request statistics reconciliation static safety checks passed'
