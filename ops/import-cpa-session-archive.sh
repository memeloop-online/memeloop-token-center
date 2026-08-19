#!/bin/sh
set -eu

: "${CPA_SESSION_ARCHIVE_INPUT:?CPA_SESSION_ARCHIVE_INPUT is required}"
: "${IMPORT_TENANT_EXTERNAL_ID:=cpa-dogfood-import}"
: "${CPAMP_IMPORT_SOURCE:=cpamp-usage-events-v1}"
: "${SESSION_ARCHIVE_IMPORT_SOURCE:=cpa-session-archive-v2}"
: "${SESSION_ARCHIVE_OVERLAP_MS:=86400000}"
: "${SESSION_ARCHIVE_TIME_TOLERANCE_MS:=300000}"
: "${SESSION_ARCHIVE_MAX_LINE_BYTES:=16777216}"
: "${SESSION_ARCHIVE_PLAN_DIRECTORY:=/tmp}"
: "${SESSION_ARCHIVE_MAX_PLAN_BYTES:=1073741824}"
: "${SESSION_ARCHIVE_ALLOW_UNMAPPED:=false}"
: "${SESSION_ARCHIVE_APPLY:=false}"
: "${MTC_SESSION_ARCHIVE_IMPORT_BIN:=import-cpa-session-archive}"

[ -f "$CPA_SESSION_ARCHIVE_INPUT" ] && [ -r "$CPA_SESSION_ARCHIVE_INPUT" ] || {
  echo "CPA_SESSION_ARCHIVE_INPUT must be a readable regular file" >&2
  exit 2
}
case "$SESSION_ARCHIVE_APPLY" in true|false) ;; *) echo "SESSION_ARCHIVE_APPLY must be true or false" >&2; exit 2;; esac
case "$SESSION_ARCHIVE_ALLOW_UNMAPPED" in true|false) ;; *) echo "SESSION_ARCHIVE_ALLOW_UNMAPPED must be true or false" >&2; exit 2;; esac
for value in "$SESSION_ARCHIVE_OVERLAP_MS" "$SESSION_ARCHIVE_TIME_TOLERANCE_MS" "$SESSION_ARCHIVE_MAX_LINE_BYTES" "$SESSION_ARCHIVE_MAX_PLAN_BYTES"; do
  case "$value" in *[!0-9]*|'') echo "archive import numeric settings must be unsigned integers" >&2; exit 2;; esac
done
[ "$SESSION_ARCHIVE_MAX_LINE_BYTES" -le 16777216 ] || {
  echo "SESSION_ARCHIVE_MAX_LINE_BYTES must not exceed the 16 MiB importer hard limit" >&2
  exit 2
}
[ -d "$SESSION_ARCHIVE_PLAN_DIRECTORY" ] && [ -w "$SESSION_ARCHIVE_PLAN_DIRECTORY" ] || {
  echo "SESSION_ARCHIVE_PLAN_DIRECTORY must be a writable directory" >&2
  exit 2
}
command -v "$MTC_SESSION_ARCHIVE_IMPORT_BIN" >/dev/null 2>&1 || {
  echo "session archive importer binary is unavailable" >&2
  exit 2
}

set -- \
  --input "$CPA_SESSION_ARCHIVE_INPUT" \
  --plan-directory "$SESSION_ARCHIVE_PLAN_DIRECTORY" \
  --tenant-external-id "$IMPORT_TENANT_EXTERNAL_ID" \
  --cpamp-source "$CPAMP_IMPORT_SOURCE" \
  --archive-source "$SESSION_ARCHIVE_IMPORT_SOURCE" \
  --overlap-ms "$SESSION_ARCHIVE_OVERLAP_MS" \
  --time-tolerance-ms "$SESSION_ARCHIVE_TIME_TOLERANCE_MS" \
  --max-line-bytes "$SESSION_ARCHIVE_MAX_LINE_BYTES" \
  --max-plan-bytes "$SESSION_ARCHIVE_MAX_PLAN_BYTES" \
  --allow-unmapped "$SESSION_ARCHIVE_ALLOW_UNMAPPED"

if [ "$SESSION_ARCHIVE_APPLY" = true ]; then
  set -- "$@" --apply
fi

# The binary prints counts and the source inode/size/mtime/BLAKE3 seal only; it never
# logs payloads, credentials or object bytes.
exec "$MTC_SESSION_ARCHIVE_IMPORT_BIN" "$@"
