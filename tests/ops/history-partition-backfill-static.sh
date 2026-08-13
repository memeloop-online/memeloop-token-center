#!/bin/sh
set -eu

repository=$(CDPATH= cd -- "$(dirname -- "$0")/../.." && pwd)
driver=$repository/ops/backfill-postgres-history-partitions.sh
procedure=$repository/ops/postgres/history-partition-backfill.sql
indexes=$repository/ops/postgres/history-partition-indexes.sql

sh -n "$driver"
"$driver" --help >/dev/null

assert_contains() {
  file=$1
  pattern=$2
  grep -F -- "$pattern" "$file" >/dev/null || {
    echo "missing required safety marker in $file: $pattern" >&2
    exit 1
  }
}

assert_contains "$driver" 'ACTION=dry-run'
assert_contains "$driver" '--apply'
assert_contains "$driver" 'pg_try_advisory_lock'
assert_contains "$driver" 'CREATE INDEX CONCURRENTLY'
assert_contains "$procedure" 'COMMIT;'
assert_contains "$procedure" 'LOCK TABLE public.%I IN SHARE ROW EXCLUSIVE MODE'
assert_contains "$procedure" 'ON CONFLICT (%4$I) DO NOTHING'
assert_contains "$procedure" 'EXCEPT SELECT * FROM public.%2$I'
assert_contains "$procedure" 'ATTACH PARTITION'
assert_contains "$indexes" 'ON ONLY public.request_records (created_at DESC, id DESC)'
assert_contains "$indexes" 'ON ONLY public.request_events (event_at ASC, event_id ASC)'

if grep -F 'migrations/sqlite' "$driver" "$procedure" "$indexes" >/dev/null; then
  echo 'PostgreSQL backfill must not reference SQLite migrations' >&2
  exit 1
fi

echo 'history partition backfill static safety checks passed'
