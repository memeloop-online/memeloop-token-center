export const capacityPolicy = String.raw`capacity_volume() {
  capacity_path=$1
  capacity_uuid=$2
  capacity_max=$3
  capacity_min=$4
  capacity_reserve=$5
  test "$HARD_CAPACITY_REVIEW_APPROVED" = true || return 1
  case "$capacity_uuid" in ''|*[!a-zA-Z0-9-]*) return 1 ;; esac
  capacity_type=$(timeout 5 findmnt -rn -o FSTYPE -T "$capacity_path") || return 1
  case "$capacity_type" in ext4|xfs) ;; *) return 1 ;; esac
  capacity_target=$(timeout 5 findmnt -rn -o TARGET -T "$capacity_path") || return 1
  capacity_root=$(timeout 5 findmnt -rn -o FSROOT -T "$capacity_path") || return 1
  capacity_actual_uuid=$(timeout 5 findmnt -rn -o UUID -T "$capacity_path") || return 1
  test "$capacity_target" = "$capacity_path" || return 1
  test "$capacity_root" = / || return 1
  test "$capacity_actual_uuid" = "$capacity_uuid" || return 1
  capacity_device=$(timeout 5 findmnt -rn -o SOURCE -T "$capacity_path") || return 1
  case "$capacity_device" in /dev/*) ;; *) return 1 ;; esac
  case "$capacity_device" in *'['*|*']'*) return 1 ;; esac
  capacity_stats=$(timeout 5 stat -f -c '%S %b %a %c %d' "$capacity_path") || return 1
  set -- $capacity_stats
  test "$#" -eq 5 || return 1
  for capacity_number in "$@" "$capacity_max" "$capacity_min" "$capacity_reserve" "$CAPACITY_MIN_FREE_INODES"; do
    case "$capacity_number" in ''|*[!0-9]*) return 1 ;; esac
  done
  test "$1" -gt 0 && test "$2" -gt 0 && test "$4" -gt 0 || return 1
  capacity_bytes=$(($1 * $2))
  test "$capacity_bytes" -le "$capacity_max" && test "$capacity_bytes" -ge "$capacity_min" || return 1
  test "$(($1 * $3))" -ge "$capacity_reserve" || return 1
  test "$5" -ge "$CAPACITY_MIN_FREE_INODES" || return 1
}
capacity_backup() {
  capacity_volume /backup "$EXPECTED_BACKUP_FS_UUID" "$BACKUP_MAX_BYTES" "$BACKUP_MIN_BYTES" "$BACKUP_RESERVE_BYTES"
}
capacity_scratch() {
  test "$EXPECTED_SCRATCH_FS_UUID" != "$EXPECTED_BACKUP_FS_UUID" || return 1
  capacity_volume /scratch "$EXPECTED_SCRATCH_FS_UUID" "$SCRATCH_MAX_BYTES" "$SCRATCH_MIN_BYTES" "$SCRATCH_RESERVE_BYTES"
}
capacity_restore() {
  capacity_backup && capacity_scratch
}
`;

export const restoreReceipt = String.raw`
pg_ctl -D "$PGDATA" -m fast -w stop > /scratch/stop.log
capacity_restore
archive_bytes=$(stat -c %s "$archive")
case "$archive_bytes" in ''|*[!0-9]*) exit 1 ;; esac
test "$archive_bytes" -gt 0 && test "$archive_bytes" -le 25769803776
printf '{"schema":1,"state":"isolated-logical-restore-verified","archive_sha256":"%s","archive_bytes":%s,"public_relations":%s,"backup_fs_uuid":"%s","scratch_fs_uuid":"%s","verified_at":"%s","original_ownership_acl_verified":false,"application_acceptance_verified":false,"physical_wal_protection_verified":false}\n' \
  "$EXPECTED_SOURCE_SHA256" "$archive_bytes" "$table_count" "$EXPECTED_BACKUP_FS_UUID" "$EXPECTED_SCRATCH_FS_UUID" "$(date -u +%FT%TZ)" > /scratch/RESTORE_SUCCESS.json.partial
sync -f /scratch
mv /scratch/RESTORE_SUCCESS.json.partial /scratch/RESTORE_SUCCESS.json
sync -f /scratch
`;
