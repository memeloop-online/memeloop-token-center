import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { parseAllDocuments, stringify } from 'yaml';
import { capacityPolicy, restoreReceipt } from './capacity-policy.ts';
import { storageResources, validateStorageResources } from './storage-preflight.ts';

export const boundedClaims = {
  stage: 'mtc-pg-bounded-stage-20261005',
  archive: 'mtc-pg-bounded-archive-20261005',
  scratch: 'mtc-pg-bounded-scratch-20261005',
};
export const boundedJobs = {
  stage: 'mtc-pg-bounded-stage-20261005',
  source: 'mtc-pg-bounded-copy-source-20261005',
  destination: 'mtc-pg-bounded-copy-destination-20261005',
  restore: 'mtc-pg-bounded-restore-20261005',
};
export const policyName = 'mtc-pg-hard-capacity-20261005';
const directory = dirname(fileURLToPath(import.meta.url));
const gib = 1024 ** 3;

function replaceOnce(script: string, before: string, after: string): string {
  assert.equal(script.split(before).length, 2, `Reviewed upstream command changed: ${before}`);
  return script.replace(before, after);
}

export function preparedResources(): any[] {
  const originals = ['mtc-pg-local-stage-20261004.yaml', 'mtc-pg-offhost-copy-20261004.yaml', 'mtc-pg-restore-verification-20261004.yaml']
    .flatMap(name => parseAllDocuments(readFileSync(join(directory, name), 'utf8')).map(document => {
      assert.deepEqual(document.errors, []);
      return document.toJS({ maxAliasCount: 0 });
    }));
  const resources = originals.filter(resource => resource.kind !== 'PersistentVolumeClaim');
  const renamedClaims: Record<string, string> = {
    'mtc-pg-local-stage-20261004': boundedClaims.stage,
    'mtc-pg-logical-backup-20261003': boundedClaims.archive,
    'mtc-pg-restore-scratch-20261004': boundedClaims.scratch,
  };
  const renamedJobs: Record<string, string> = {
    'mtc-pg-local-stage-20261004': boundedJobs.stage,
    'mtc-pg-copy-source-20261004': boundedJobs.source,
    'mtc-pg-copy-destination-20261004': boundedJobs.destination,
    'mtc-pg-restore-verification-20261004': boundedJobs.restore,
  };
  for (const resource of resources.filter(resource => resource.kind === 'Job')) {
    resource.metadata.name = renamedJobs[resource.metadata.name];
    Object.assign(resource.metadata.annotations, {
      'recovery.mtc/storage-gate': 'exact-prebound-longhorn-volumes-require-separate-allocation-approval-no-host-format-or-loop-setup',
      'recovery.mtc/deployment-gate': 'separate-owner-approval-required-do-not-apply-or-unsuspend',
      'recovery.mtc/host-headroom-gate': 'review-backing-block-and-inode-reserves-concurrent-writers-and-no-source-data-device-interference-before-deployment',
      'recovery.mtc/long-term-protection': 'physical-backup-and-continuous-wal-archive-remain-open-not-satisfied-by-logical-receipt',
    });
    delete resource.metadata.annotations['recovery.mtc/free-space-gate'];
    const pod = resource.spec.template.spec;
    for (const volume of pod.volumes) {
      if (volume.persistentVolumeClaim) volume.persistentVolumeClaim.claimName = renamedClaims[volume.persistentVolumeClaim.claimName];
    }
    pod.volumes.push({ name: 'capacity-policy', configMap: { name: policyName } });
    const container = pod.containers[0];
    container.volumeMounts.push({ name: 'capacity-policy', mountPath: '/policy', readOnly: true });
    container.env ??= [];
    container.env = container.env.filter((entry: any) => !['EXPECTED_BACKUP_FS_UUID', 'EXPECTED_SERVER_ADDRESS'].includes(entry.name));
    const environment: Record<string, string> = {
      HARD_CAPACITY_REVIEW_APPROVED: 'false',
      EXPECTED_BACKUP_FS_UUID: '',
      BACKUP_MAX_BYTES: String(28 * gib),
      BACKUP_MIN_BYTES: String(26 * gib),
      BACKUP_RESERVE_BYTES: String(256 * 1024 ** 2),
      CAPACITY_MIN_FREE_INODES: '1024',
    };
    if (resource.metadata.name === boundedJobs.stage) environment.EXPECTED_SERVER_ADDRESS = '';
    if (resource.metadata.name === boundedJobs.restore) Object.assign(environment, {
      EXPECTED_SCRATCH_FS_UUID: '',
      SCRATCH_MAX_BYTES: String(64 * gib),
      SCRATCH_MIN_BYTES: String(60 * gib),
      SCRATCH_RESERVE_BYTES: String(gib),
    });
    container.env.push(...Object.entries(environment).map(([name, value]) => ({ name, value })));
    if (resource.metadata.name === boundedJobs.stage) {
      let script: string = container.command[6];
      script = replaceOnce(script, 'umask 077', 'umask 077\n. /policy/capacity.sh\ncapacity_backup\ntest -n "$EXPECTED_SERVER_ADDRESS"');
      script = replaceOnce(script, 'available_kib=$(timeout 5 df -Pk /backup | awk \'NR == 2 { print $4 }\')\ntest "$available_kib" -ge 67108864',
        'capacity_volume /backup "$EXPECTED_BACKUP_FS_UUID" "$BACKUP_MAX_BYTES" "$BACKUP_MIN_BYTES" 26071793664');
      script = replaceOnce(script, 'available_kib=$(timeout 5 df -Pk /backup | awk \'NR == 2 { print $4 }\')\n    if ! test "$available_kib" -ge 35651584; then',
        'if ! capacity_backup; then');
      script = replaceOnce(script, 'printf \'stage free_kib=%s utc=%s\\n\' "$available_kib" "$(date -u +%FT%TZ)"',
        'printf \'stage bounded-block-and-inode-check-passed utc=%s\\n\' "$(date -u +%FT%TZ)"');
      script = replaceOnce(script, 'touch LOCAL_ARCHIVE_CREATED', String.raw`(
  trap 'marker_status=$?; if test "$marker_status" -ne 0; then rm -f LOCAL_ARCHIVE_CREATED; fi; exit "$marker_status"' EXIT
  capacity_backup
  sync -f .
  touch LOCAL_ARCHIVE_CREATED
  sync -f .
)`);
      script = replaceOnce(script, 'trap \'kill -TERM "$stage_pid" 2>/dev/null || true\' EXIT',
        'trap \'kill -TERM "$stage_pid" 2>/dev/null || true\' EXIT\n  trap \'trap - EXIT; exit 0\' TERM INT');
      container.command[6] = script;
    }
    if (resource.metadata.name === boundedJobs.restore) {
      let script: string = container.command[2];
      script = replaceOnce(script, 'umask 077', String.raw`umask 077
. /policy/capacity.sh
capacity_restore
test ! -e /scratch/RESTORE_SUCCESS.json
test ! -e /scratch/RESTORE_SUCCESS.json.partial
guard_pid=
restore_cleanup() {
  restore_status=$?
  trap - EXIT TERM INT
  if test -n "$guard_pid"; then
    kill "$guard_pid" 2>/dev/null || true
    wait "$guard_pid" 2>/dev/null || true
  fi
  if test -n "$PGDATA"; then
    pg_ctl -D "$PGDATA" -m immediate stop > /dev/null 2>&1 || true
  fi
  if test "$restore_status" -ne 0; then rm -f /scratch/RESTORE_SUCCESS.json; fi
  exit "$restore_status"
}
trap restore_cleanup EXIT
trap 'exit 143' TERM
trap 'exit 130' INT
restore_pid=$$
(
  trap 'kill -TERM "$restore_pid" 2>/dev/null || true' EXIT
  trap 'trap - EXIT; exit 0' TERM INT
  while kill -0 "$restore_pid" 2>/dev/null; do
    capacity_restore || exit 1
    sleep 2
  done
) &
guard_pid=$!`);
      script = replaceOnce(script, 'trap \'pg_ctl -D "$PGDATA" -m immediate stop > /dev/null 2>&1 || true\' EXIT\n', '');
      script = replaceOnce(script, 'test "$table_count" -ge 254', 'test "$table_count" -ge 254\n' + restoreReceipt);
      container.command[2] = script;
    }
    if ([boundedJobs.source, boundedJobs.destination].includes(resource.metadata.name)) {
      container.command = ['/bin/sh', '-ec', '. /policy/capacity.sh; capacity_backup; exec /bin/sleep 43200'];
    }
  }
  resources.push({ apiVersion: 'v1', kind: 'ConfigMap', metadata: { name: policyName, namespace: 'memeloop-token-center' }, data: { 'capacity.sh': capacityPolicy } });
  validateStorageResources();
  resources.push(...structuredClone(storageResources));
  return resources;
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  process.stdout.write(preparedResources().map(resource => stringify(resource)).join('---\n'));
}
