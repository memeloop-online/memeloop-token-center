import assert from 'node:assert/strict';
import { restoreReceipt } from './capacity-policy.ts';

export function superviseRestore(original: string): string {
  const stopTrap = 'trap \'pg_ctl -D "$PGDATA" -m immediate stop > /dev/null 2>&1 || true\' EXIT\n';
  assert.equal(original.split(stopTrap).length, 2);
  assert.equal(original.split('test "$table_count" -ge 254').length, 2);
  const child = original.replace(stopTrap, '') + String.raw`
timeout -k 1 15 pg_ctl -D "$PGDATA" -m fast -w stop > /scratch/stop.log
printf '%s\n' "$table_count" > /tmp/restore-public-relations
`;
  const stop = 'pg_ctl -D "$PGDATA" -m fast -w stop > /scratch/stop.log\n';
  assert.ok(restoreReceipt.startsWith('\n' + stop));
  const receipt = restoreReceipt.replace(stop, '');
  const quoted = "'" + child.replaceAll("'", "'\\''") + "'";
  return String.raw`umask 077
test "$PARENT_REVIEW_APPROVED" = true
. /policy/capacity.sh
test ! -e /scratch/RESTORE_SUCCESS.json
test ! -e /scratch/RESTORE_SUCCESS.json.partial
test ! -e /scratch/pgdata
test ! -e /scratch/socket
test ! -e /tmp/restore-public-relations
restore_wait_started=$(date +%s)
if test "$(printenv BACKUP_UUID_ATTESTATION || true)" = external-csi-lease || test "$(printenv SCRATCH_UUID_ATTESTATION || true)" = external-csi-lease; then
  test "$BACKUP_UUID_ATTESTATION" = external-csi-lease
  test "$SCRATCH_UUID_ATTESTATION" = external-csi-lease
  until test -f /tmp/backup-volume.lease && test -f /tmp/scratch-volume.lease; do
    test "$(($(date +%s) - restore_wait_started))" -lt 120
    sleep 1
  done
fi
capacity_restore
export PGDATA=/scratch/pgdata PGHOST=/scratch/socket PGUSER=postgres PGDATABASE=postgres
restore_child_pid=
restore_cleanup() {
  restore_status=$?
  trap - EXIT TERM INT
  if test -n "$restore_child_pid"; then
    kill -TERM -- "-$restore_child_pid" 2>/dev/null || true
    kill -TERM "$restore_child_pid" 2>/dev/null || true
    sleep 2
    kill -KILL -- "-$restore_child_pid" 2>/dev/null || true
    kill -KILL "$restore_child_pid" 2>/dev/null || true
    wait "$restore_child_pid" 2>/dev/null || true
  fi
  if test "$restore_status" -ne 0; then
    timeout -k 1 8 pg_ctl -D "$PGDATA" -m immediate -w stop > /dev/null 2>&1 || true
    rm -f /scratch/RESTORE_SUCCESS.json
  fi
  exit "$restore_status"
}
trap restore_cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
setsid /bin/sh -ec ` + quoted + String.raw` &
restore_child_pid=$!
while kill -0 "$restore_child_pid" 2>/dev/null; do
  if ! capacity_restore; then
    printf 'restore_abort=volume-identity-or-capacity utc=%s\n' "$(date -u +%FT%TZ)" >&2
    exit 1
  fi
  sleep 2
done
wait "$restore_child_pid"
restore_child_pid=
capacity_restore
table_count=$(cat /tmp/restore-public-relations)
case "$table_count" in ''|*[!0-9]*) exit 1 ;; esac
test "$table_count" -ge 254
archive=/backup/mtc-pg-logical-20261004/memeloop_token_center.dump
` + receipt + String.raw`
capacity_restore
cat /scratch/RESTORE_SUCCESS.json
`;
}
