#!/bin/sh
set -eu

: "${PGHOST:?PGHOST is required}"
: "${PGPORT:=5432}"
: "${PGUSER:?PGUSER is required}"
: "${PGDATABASE:?PGDATABASE is required}"
: "${IMPORT_TENANT_EXTERNAL_ID:?IMPORT_TENANT_EXTERNAL_ID is required}"
: "${CPAMP_IMPORT_SOURCE:?CPAMP_IMPORT_SOURCE is required}"
: "${SESSION_ARCHIVE_IMPORT_SOURCE:?SESSION_ARCHIVE_IMPORT_SOURCE is required}"
: "${EXPECTED_CPAMP_EVENTS:?EXPECTED_CPAMP_EVENTS is required}"
: "${EXPECTED_ARCHIVE_RECORDS:?EXPECTED_ARCHIVE_RECORDS is required}"

case "$EXPECTED_CPAMP_EVENTS:$EXPECTED_ARCHIVE_RECORDS" in
  *[!0-9:]*|:*|*::*|*:) echo "expected source counts must be unsigned integers" >&2; exit 2 ;;
esac
[ "$EXPECTED_CPAMP_EVENTS" -gt 0 ] && [ "$EXPECTED_ARCHIVE_RECORDS" -gt 0 ] || {
  echo "expected source counts must be greater than zero" >&2
  exit 2
}

if [ -n "${PGPASSFILE:-}" ] && [ -n "${PGPASSWORD:-}" ]; then
  echo "set exactly one of PGPASSFILE or PGPASSWORD" >&2
  exit 2
elif [ -n "${PGPASSFILE:-}" ]; then
  [ -f "$PGPASSFILE" ] && [ ! -L "$PGPASSFILE" ] && [ -r "$PGPASSFILE" ] || {
    echo "PGPASSFILE must be a readable regular non-symlink file" >&2
    exit 2
  }
  [ "$(stat -c '%a' "$PGPASSFILE")" = 600 ] || {
    echo "PGPASSFILE must have mode 0600" >&2
    exit 2
  }
  export PGPASSFILE
elif [ -n "${PGPASSWORD:-}" ]; then
  export PGPASSWORD
else
  echo "PGPASSFILE or PGPASSWORD is required" >&2
  exit 2
fi
export PGHOST PGPORT PGUSER PGDATABASE

counts=$(psql -X -v ON_ERROR_STOP=1 --no-psqlrc -qAt \
  -v tenant="$IMPORT_TENANT_EXTERNAL_ID" \
  -v cpamp_source="$CPAMP_IMPORT_SOURCE" \
  -v archive_source="$SESSION_ARCHIVE_IMPORT_SOURCE" <<'SQL'
BEGIN TRANSACTION READ ONLY;
WITH selected_tenant AS (
  SELECT id FROM tenants WHERE external_id = :'tenant'
), migration_request_ids AS (
  SELECT target_request_id AS request_id
    FROM session_archive_correlations
   WHERE tenant_id = (SELECT id FROM selected_tenant)
     AND source = :'archive_source'
     AND disposition = 'exact'
  UNION ALL
  SELECT archive_request_id
    FROM session_archive_unlinked_requests
   WHERE tenant_id = (SELECT id FROM selected_tenant)
     AND source = :'archive_source'
), migration_observations AS (
  SELECT o.id, o.cluster_id
    FROM conversation_observations o
    JOIN migration_request_ids m ON m.request_id = o.request_id
), measured AS (
  SELECT
    COALESCE((SELECT imported_events FROM cpamp_import_checkpoints
      WHERE tenant_external_id = :'tenant' AND source = :'cpamp_source'), 0) AS cpamp_checkpoint,
    (SELECT count(*) FROM import_request_links l
      WHERE l.tenant_id = (SELECT id FROM selected_tenant)
        AND l.source = :'cpamp_source') AS cpamp_links,
    COALESCE((SELECT imported_records FROM session_archive_import_checkpoints
      WHERE tenant_id = (SELECT id FROM selected_tenant)
        AND source = :'archive_source'), 0) AS archive_checkpoint,
    COALESCE((SELECT watermark_ms FROM session_archive_import_checkpoints
      WHERE tenant_id = (SELECT id FROM selected_tenant)
        AND source = :'archive_source'), 0) AS archive_watermark,
    (SELECT count(*) FROM session_archive_correlations c
      WHERE c.tenant_id = (SELECT id FROM selected_tenant)
        AND c.source = :'archive_source') AS archive_correlated,
    (SELECT count(*) FROM session_archive_correlations c
      WHERE c.tenant_id = (SELECT id FROM selected_tenant)
        AND c.source = :'archive_source' AND c.disposition = 'exact') AS archive_exact,
    (SELECT count(*) FROM session_archive_correlations c
      WHERE c.tenant_id = (SELECT id FROM selected_tenant)
        AND c.source = :'archive_source' AND c.disposition = 'unlinked') AS archive_unlinked,
    (SELECT count(*) FROM session_archive_import_records r
      WHERE r.tenant_id = (SELECT id FROM selected_tenant)
        AND r.source = :'archive_source') AS exact_provenance,
    (SELECT count(*) FROM session_archive_unlinked_requests u
      WHERE u.tenant_id = (SELECT id FROM selected_tenant)
        AND u.source = :'archive_source') AS unlinked_projection,
    (SELECT count(*) FROM session_archive_quarantine_records q
      LEFT JOIN session_archive_quarantine_resolutions z ON z.quarantine_id = q.id
      WHERE q.tenant_id = (SELECT id FROM selected_tenant)
        AND q.source = :'archive_source' AND z.id IS NULL) AS unresolved_quarantine,
    (SELECT count(*) FROM session_archive_import_records a
      JOIN request_records r ON r.id = a.target_request_id
      WHERE a.tenant_id = (SELECT id FROM selected_tenant)
        AND a.source = :'archive_source'
        AND (r.request_object LIKE 'gap://%' OR r.response_object LIKE 'gap://%'))
      AS exact_gap_locators,
    (SELECT count(*) FROM session_archive_unlinked_requests u
      WHERE u.tenant_id = (SELECT id FROM selected_tenant)
        AND u.source = :'archive_source'
        AND (u.request_object LIKE 'gap://%' OR u.response_object LIKE 'gap://%'))
      AS unlinked_gap_locators,
    (SELECT count(DISTINCT cluster_id) FROM migration_observations)
      AS correlated_clusters,
    (SELECT count(*) FROM migration_observations) AS correlated_observations,
    (SELECT count(*) FROM conversation_edges e
      WHERE e.to_observation_id IN (SELECT id FROM migration_observations)
        AND (e.from_observation_id IS NULL OR e.from_observation_id IN (
          SELECT id FROM migration_observations
        ))) AS conversation_edges
)
SELECT cpamp_checkpoint || '|' || cpamp_links || '|' ||
       archive_checkpoint || '|' || archive_watermark || '|' ||
       archive_correlated || '|' || archive_exact || '|' || archive_unlinked || '|' ||
       exact_provenance || '|' || unlinked_projection || '|' ||
       unresolved_quarantine || '|' || exact_gap_locators || '|' ||
       unlinked_gap_locators || '|' || correlated_clusters || '|' ||
       correlated_observations || '|' || conversation_edges
FROM measured;
COMMIT;
SQL
)

IFS='|' read -r cpamp_checkpoint cpamp_links archive_checkpoint archive_watermark \
  archive_correlated archive_exact archive_unlinked exact_provenance \
  unlinked_projection unresolved_quarantine exact_gap_locators \
  unlinked_gap_locators correlated_clusters \
  correlated_observations conversation_edges <<EOF
$counts
EOF

[ "$cpamp_checkpoint" = "$EXPECTED_CPAMP_EVENTS" ] &&
[ "$cpamp_links" = "$EXPECTED_CPAMP_EVENTS" ] || {
  echo "migration audit failed: CPAMP source, checkpoint, and request links disagree" >&2
  exit 1
}
[ "$archive_checkpoint" = "$EXPECTED_ARCHIVE_RECORDS" ] &&
[ "$archive_correlated" = "$EXPECTED_ARCHIVE_RECORDS" ] &&
[ "$exact_provenance" = "$archive_exact" ] &&
[ "$unlinked_projection" = "$archive_unlinked" ] &&
[ "$unresolved_quarantine" = 0 ] &&
[ "$exact_gap_locators" = 0 ] &&
[ "$unlinked_gap_locators" = 0 ] || {
  echo "migration audit failed: archive source, checkpoint, correlation, projection, or quarantine counts disagree" >&2
  exit 1
}

printf '{"archive_checkpoint":%s,"archive_correlated":%s,"archive_exact":%s,"archive_unlinked":%s,"archive_watermark_ms":%s,"conversation_clusters":%s,"conversation_edges":%s,"conversation_observations":%s,"cpamp_checkpoint":%s,"cpamp_links":%s,"gap_locators":0,"unresolved_quarantine":0}\n' \
  "$archive_checkpoint" "$archive_correlated" "$archive_exact" \
  "$archive_unlinked" "$archive_watermark" "$correlated_clusters" \
  "$conversation_edges" "$correlated_observations" "$cpamp_checkpoint" "$cpamp_links"
