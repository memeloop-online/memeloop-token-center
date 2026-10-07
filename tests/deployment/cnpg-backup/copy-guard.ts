import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';
import { archiveIdentity, attestBackupVolume, stageIdentity, stageIdentityForPod, type InventoryReader } from './volume-identity.ts';

export const copyIdentities = { source: stageIdentity, destination: archiveIdentity };
export const copyImage = 'ghcr.io/cloudnative-pg/postgresql@sha256:b1deeed2aa998b2f381e39c5cadb9ec06127708c8bd62965743af19abf21628f';
export type CopySide = keyof typeof copyIdentities;
export const copySides: CopySide[] = ['source', 'destination'];
export const copyContainerCommand = String.raw`umask 077
. /policy/capacity.sh
copy_wait_start=$(date +%s)
until test -f /tmp/backup-volume.lease; do
  test "$(($(date +%s) - copy_wait_start))" -lt 120
  sleep 1
done
while :; do
  capacity_backup || exit 1
  sleep 2
done`;

export function validateCopyPod(pod: any, side: CopySide, expectedUID?: string): void {
  const expected = side === 'source' ? stageIdentityForPod(pod, 'copy') : archiveIdentity;
  assert.equal(pod.metadata.namespace, expected.namespace);
  assert.ok(!pod.metadata.deletionTimestamp);
  assert.match(pod.metadata.uid, /^[a-zA-Z0-9-]{1,64}$/);
  if (expectedUID !== undefined) assert.equal(pod.metadata.uid, expectedUID, 'Copy pod replaced');
  const job = pod.metadata.labels['job-name'];
  assert.match(job, new RegExp(`^mtc-pg-bounded-copy-${side}-[0-9]{8}(?:-[a-z0-9]+)*$`));
  assert.ok(pod.metadata.name.startsWith(`${job}-`));
  assert.equal(pod.metadata.ownerReferences.filter((owner: any) => owner.kind === 'Job' && owner.name === job && owner.controller === true).length, 1);
  assert.equal(pod.metadata.labels['recovery.mtc/operation'], 'pg-logical-backup-20261004');
  assert.equal(pod.metadata.labels['recovery.mtc/role'], 'copy');
  assert.equal(pod.spec.nodeName, expected.node);
  assert.equal(pod.status.phase, 'Running');
  assert.equal(pod.spec.automountServiceAccountToken, false);
  assert.ok(!pod.spec.hostPID && !pod.spec.hostNetwork);
  assert.equal(pod.spec.securityContext.runAsUser, 26);
  assert.equal(pod.spec.containers.length, 1);
  assert.equal(pod.spec.initContainers?.length ?? 0, 0);
  assert.equal(pod.spec.ephemeralContainers?.length ?? 0, 0);
  assert.ok(pod.spec.volumes.every((volume: any) => !volume.hostPath && !volume.secret));
  const claims = pod.spec.volumes.filter((volume: any) => volume.persistentVolumeClaim);
  assert.equal(claims.length, 1);
  assert.equal(claims[0].name, 'backup');
  assert.equal(claims[0].persistentVolumeClaim.claimName, expected.name);
  assert.equal(claims[0].persistentVolumeClaim.readOnly ?? false, side === 'source');
  const container = pod.spec.containers[0];
  assert.equal(container.name, 'copy');
  assert.equal(container.image, copyImage);
  assert.equal(container.imagePullPolicy, 'IfNotPresent');
  assert.ok(!container.envFrom?.length);
  assert.ok(container.env.every((entry: any) => !entry.valueFrom || entry.name === 'POD_UID' && entry.valueFrom.fieldRef?.fieldPath === 'metadata.uid'));
  assert.equal(container.securityContext.allowPrivilegeEscalation, false);
  assert.equal(container.securityContext.readOnlyRootFilesystem, true);
  assert.deepEqual(container.securityContext.capabilities.drop, ['ALL']);
  assert.equal(container.volumeMounts.find((mount: any) => mount.name === 'backup').readOnly ?? false, side === 'source');
  assert.deepEqual(container.command, ['/bin/sh', '-ec', copyContainerCommand]);
  const status = pod.status.containerStatuses.find((entry: any) => entry.name === 'copy');
  assert.ok(status.ready && status.state.running);
  assert.equal(status.restartCount, 0);
  assert.match(status.containerID, /^containerd:\/\/[a-f0-9]{64}$/);
}

export function renewCopyLease(read: InventoryReader, side: CopySide, podName: string, uid: string, clock: () => number = Date.now): void {
  const expected = copyIdentities[side];
  const getPod = () => JSON.parse(read(['-n', expected.namespace, 'get', 'pod', podName, '-o', 'json']));
  const before = getPod();
  validateCopyPod(before, side, uid);
  const observedIdentity = side === 'source' ? stageIdentityForPod(before, 'copy') : expected;
  const lease = attestBackupVolume(read, before, observedIdentity, 'copy', clock);
  const after = getPod();
  validateCopyPod(after, side, uid);
  assert.deepEqual(after.spec, before.spec, 'Copy pod spec changed across CSI observation');
  assert.deepEqual(after.status.containerStatuses, before.status.containerStatuses, 'Copy container changed across CSI observation');
  const epoch = Number(lease.split(' ')[0]);
  const age = Math.floor(clock() / 1000) - epoch;
  assert.ok(age >= 0 && age <= expected.leaseSeconds, 'Copy CSI observation expired before publication');
  read(['-n', expected.namespace, 'exec', podName, '-c', 'copy', '--', '/bin/sh', '-ec',
    `umask 077; test "$POD_UID" = '${uid}'; printf '%s\\n' '${lease}' > /tmp/backup-volume.lease.partial; mv /tmp/backup-volume.lease.partial /tmp/backup-volume.lease`]);
}

export async function watchCopyLease(renew: () => void, ready: () => void, clock: () => number = () => performance.now(), pause: (milliseconds: number) => Promise<void> = delay): Promise<never> {
  const started = clock();
  let announced = false;
  while (clock() - started < 43_200_000) {
    const cycleStarted = clock();
    renew();
    if (!announced) { ready(); announced = true; }
    await pause(Math.max(0, 15_000 - (clock() - cycleStarted)));
  }
  throw new Error('Copy guard deadline');
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  assert.equal(process.env.PARENT_REVIEW_APPROVED, 'true');
  assert.equal(process.argv.length, 5, 'copy-guard.ts SIDE POD UID');
  const [side, podName, uid] = process.argv.slice(2) as [CopySide, string, string];
  assert.ok(copySides.includes(side));
  assert.ok(process.connected, 'Copy guard requires its owning controller IPC connection');
  process.once('disconnect', () => { process.exitCode = 1; process.exit(); });
  const read: InventoryReader = args => execFileSync('kubectl', ['--request-timeout=8s', ...args], { encoding: 'utf8', timeout: 10_000, killSignal: 'SIGKILL', maxBuffer: 8 * 1024 ** 2, stdio: ['ignore', 'pipe', 'pipe'] });
  try {
    await watchCopyLease(() => renewCopyLease(read, side, podName, uid), () => { console.log('COPY_LEASES_READY'); });
  } catch {
    console.error(JSON.stringify({ event: 'copy-guard-stopped', side, at: new Date().toISOString(), leaseSeconds: 45 }));
    process.exitCode = 1;
  }
}
