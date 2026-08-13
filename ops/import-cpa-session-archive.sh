#!/bin/sh
set -eu

: "${CPA_SESSION_ARCHIVE_INPUT:?CPA_SESSION_ARCHIVE_INPUT is required}"
: "${IMPORT_TENANT_EXTERNAL_ID:=cpa-dogfood-import}"
: "${CPAMP_IMPORT_SOURCE:=cpamp-usage-events-v1}"
: "${SESSION_ARCHIVE_IMPORT_SOURCE:=cpa-session-archive-v2}"
: "${SESSION_ARCHIVE_OVERLAP_MS:=86400000}"
: "${SESSION_ARCHIVE_TIME_TOLERANCE_MS:=300000}"
: "${SESSION_ARCHIVE_MAX_LINE_BYTES:=134217728}"
: "${SESSION_ARCHIVE_ALLOW_UNMAPPED:=false}"
: "${SESSION_ARCHIVE_APPLY:=false}"
: "${MTC_SESSION_ARCHIVE_IMPORT_BIN:=import-cpa-session-archive}"

[ -f "$CPA_SESSION_ARCHIVE_INPUT" ] && [ -r "$CPA_SESSION_ARCHIVE_INPUT" ] || {
  echo "CPA_SESSION_ARCHIVE_INPUT must be a readable regular file" >&2
  exit 2
}
case "$SESSION_ARCHIVE_APPLY" in true|false) ;; *) echo "SESSION_ARCHIVE_APPLY must be true or false" >&2; exit 2;; esac
case "$SESSION_ARCHIVE_ALLOW_UNMAPPED" in true|false) ;; *) echo "SESSION_ARCHIVE_ALLOW_UNMAPPED must be true or false" >&2; exit 2;; esac
for value in "$SESSION_ARCHIVE_OVERLAP_MS" "$SESSION_ARCHIVE_TIME_TOLERANCE_MS" "$SESSION_ARCHIVE_MAX_LINE_BYTES"; do
  case "$value" in *[!0-9]*|'') echo "archive import numeric settings must be unsigned integers" >&2; exit 2;; esac
done
command -v "$MTC_SESSION_ARCHIVE_IMPORT_BIN" >/dev/null 2>&1 || {
  echo "session archive importer binary is unavailable" >&2
  exit 2
}

set -- \
  --input "$CPA_SESSION_ARCHIVE_INPUT" \
  --tenant-external-id "$IMPORT_TENANT_EXTERNAL_ID" \
  --cpamp-source "$CPAMP_IMPORT_SOURCE" \
  --archive-source "$SESSION_ARCHIVE_IMPORT_SOURCE" \
  --overlap-ms "$SESSION_ARCHIVE_OVERLAP_MS" \
  --time-tolerance-ms "$SESSION_ARCHIVE_TIME_TOLERANCE_MS" \
  --max-line-bytes "$SESSION_ARCHIVE_MAX_LINE_BYTES" \
  --allow-unmapped "$SESSION_ARCHIVE_ALLOW_UNMAPPED"

if [ "$SESSION_ARCHIVE_APPLY" = true ]; then
  set -- "$@" --apply
fi

# The binary prints counts/checkpoints only; it never logs payloads, credentials or object bytes.
exec "$MTC_SESSION_ARCHIVE_IMPORT_BIN" "$@"
