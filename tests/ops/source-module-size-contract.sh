#!/bin/sh
set -eu

repository=$(CDPATH='' cd -- "$(dirname -- "$0")/../.." && pwd)

line_count() {
  wc -l < "$repository/$1" | tr -d ' '
}

assert_range() {
  path=$1
  minimum=$2
  maximum=$3
  lines=$(line_count "$path")
  if [ "$lines" -lt "$minimum" ] || [ "$lines" -gt "$maximum" ]; then
    echo "$path has $lines lines; expected $minimum..$maximum" >&2
    exit 1
  fi
}

# These ceilings are deliberately close to the post-refactor sizes. They keep
# the old hotspots from silently absorbing their extracted responsibilities,
# while the lower bounds make deleting or bypassing the focused modules fail.
assert_range src/api/proxy.rs 1 1700
assert_range src/api/proxy/streaming.rs 400 550
assert_range src/db/generation/jobs.rs 1 1550
assert_range src/db/generation/jobs/finish.rs 300 450

grep -Fq 'mod streaming;' "$repository/src/api/proxy.rs"
grep -Fq 'streaming::stream_response' "$repository/src/api/proxy.rs"
grep -Fq 'pub(super) async fn stream_response' \
  "$repository/src/api/proxy/streaming.rs"
! grep -Fq 'tokio::spawn(async move' "$repository/src/api/proxy.rs"

proxy_start=$(grep -n '^pub(super) async fn proxy(' "$repository/src/api/proxy.rs" | cut -d: -f1)
proxy_end=$(grep -n '^fn record_delivered_chunk(' "$repository/src/api/proxy.rs" | cut -d: -f1)
proxy_span=$((proxy_end - proxy_start))
if [ "$proxy_span" -gt 550 ]; then
  echo "proxy entrypoint spans $proxy_span lines; expected at most 550" >&2
  exit 1
fi

grep -Fq 'mod finish;' "$repository/src/db/generation/jobs.rs"
! grep -Fq 'pub async fn finish_generation_job' \
  "$repository/src/db/generation/jobs.rs"
grep -Fq 'pub async fn finish_generation_job' \
  "$repository/src/db/generation/jobs/finish.rs"

echo 'Source module size contract OK'
