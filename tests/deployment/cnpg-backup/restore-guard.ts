import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { copyImage, watchCopyLease } from './copy-guard.ts';
import { archiveIdentity, attestBackupVolume, type InventoryReader, type stageIdentity } from './volume-identity.ts';

export type RestoreSide = 'backup' | 'scratch';
export type RestorePlan = { commandSHA256: string; archiveSHA256: string; scratch: typeof stageIdentity };
export const restoreSides: RestoreSide[] = ['backup', 'scratch'];
export const restoreClaim = 'mtc-pg-bounded-scratch-20261005';

export function validateRestorePlan(plan: RestorePlan): void {
  assert.match(plan.commandSHA256, /^[a-f0-9]{64}$/);
  assert.match(plan.archiveSHA256, /^[a-f0-9]{64}$/);
  const scratch = plan.scratch;
  assert.equal(scratch.name, restoreClaim);
  assert.equal(scratch.namespace, archiveIdentity.namespace);
  assert.equal(scratch.node, archiveIdentity.node);
  assert.equal(scratch.capacityGiB, 64);
  assert.equal(scratch.leaseSeconds, 45);
  assert.equal(scratch.device, `/dev/longhorn/${restoreClaim}`);
  assert.equal(scratch.diskUUID, archiveIdentity.diskUUID);
  assert.equal(scratch.diskPath, archiveIdentity.diskPath);
  for (const key of ['claimUID', 'persistentUID', 'longhornUID', 'filesystemUUID'] as const) {
    assert.match(scratch[key], /^[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}$/);
    assert.notEqual(scratch[key], archiveIdentity[key]);
  }
}

export function validateRestorePod(pod: any, plan: RestorePlan, uid?: string, terminal = false): void {
  validateRestorePlan(plan);
  assert.equal(pod.metadata.namespace, archiveIdentity.namespace);
  assert.ok(!pod.metadata.deletionTimestamp);
  assert.match(pod.metadata.uid, /^[a-zA-Z0-9-]{1,64}$/);
  if (uid !== undefined) assert.equal(pod.metadata.uid, uid);
  const job = pod.metadata.labels['job-name'];
  assert.match(job, /^mtc-pg-bounded-restore-[0-9]{8}(?:-[a-z0-9]+)*$/);
  assert.ok(pod.metadata.name.startsWith(`${job}-`));
  assert.equal(pod.metadata.ownerReferences.filter((owner: any) => owner.kind === 'Job' && owner.name === job && owner.controller === true).length, 1);
  assert.equal(pod.metadata.labels['recovery.mtc/operation'], 'pg-logical-backup-20261004');
  assert.equal(pod.metadata.labels['recovery.mtc/role'], 'verify');
  assert.equal(pod.spec.nodeName, archiveIdentity.node);
  assert.ok(!pod.spec.hostPID && !pod.spec.hostNetwork);
  assert.equal(pod.spec.automountServiceAccountToken, false);
  assert.equal(pod.spec.securityContext.runAsUser, 26);
  assert.equal(pod.spec.securityContext.runAsNonRoot, true);
  assert.equal(pod.spec.securityContext.seccompProfile.type, 'RuntimeDefault');
  assert.equal(pod.spec.containers.length, 1);
  assert.equal(pod.spec.initContainers?.length ?? 0, 0);
  assert.equal(pod.spec.ephemeralContainers?.length ?? 0, 0);
  assert.ok(pod.spec.volumes.every((volume: any) => !volume.hostPath && !volume.secret));
  assert.deepEqual(pod.spec.volumes.map((volume: any) => volume.name).sort(), ['backup', 'capacity-policy', 'scratch', 'tmp']);
  assert.deepEqual(pod.spec.volumes.find((volume: any) => volume.name === 'tmp').emptyDir, { medium: 'Memory', sizeLimit: '64Mi' });
  const claims = pod.spec.volumes.filter((volume: any) => volume.persistentVolumeClaim);
  assert.equal(claims.length, 2);
  const container = pod.spec.containers[0];
  assert.equal(container.name, 'verify');
  assert.equal(container.image, copyImage);
  assert.equal(container.imagePullPolicy, 'IfNotPresent');
  assert.equal(container.securityContext.allowPrivilegeEscalation, false);
  assert.equal(container.securityContext.readOnlyRootFilesystem, true);
  assert.deepEqual(container.securityContext.capabilities.drop, ['ALL']);
  assert.equal(createHash('sha256').update(JSON.stringify(container.command)).digest('hex'), plan.commandSHA256);
  assert.ok(!container.envFrom?.length);
  assert.ok(!container.env.some((entry: any) => ['DATABASE_URL', 'PGPASSWORD', 'PGSERVICE'].includes(entry.name)));
  assert.ok(container.env.every((entry: any) => !entry.valueFrom || entry.name === 'POD_UID' && entry.valueFrom.fieldRef?.fieldPath === 'metadata.uid'));
  assert.equal(new Set(container.env.map((entry: any) => entry.name)).size, container.env.length);
  const environment = (name: string) => container.env.find((entry: any) => entry.name === name)?.value;
  assert.equal(environment('PARENT_REVIEW_APPROVED'), 'true');
  assert.equal(environment('HARD_CAPACITY_REVIEW_APPROVED'), 'true');
  assert.equal(environment('EXPECTED_SOURCE_SHA256'), plan.archiveSHA256);
  assert.equal(environment('CAPACITY_MIN_FREE_INODES'), '1024');
  assert.equal(environment('HOME'), '/scratch');
  for (const side of restoreSides) {
    const identity = side === 'backup' ? archiveIdentity : plan.scratch;
    const claim = claims.find((volume: any) => volume.name === side);
    const mount = container.volumeMounts.find((entry: any) => entry.name === side);
    assert.equal(claim.persistentVolumeClaim.claimName, identity.name);
    assert.equal(claim.persistentVolumeClaim.readOnly === true, side === 'backup');
    assert.equal(mount.mountPath, '/' + side);
    assert.equal(mount.readOnly === true, side === 'backup');
    assert.equal(mount.subPath, undefined);
    assert.equal(mount.subPathExpr, undefined);
    const prefix = side.toUpperCase();
    assert.equal(environment(`${prefix}_UUID_ATTESTATION`), 'external-csi-lease');
    assert.equal(environment(`EXPECTED_${prefix}_FS_UUID`), identity.filesystemUUID);
    assert.equal(environment(`EXPECTED_${prefix}_DEVICE`), identity.device);
    assert.equal(environment(`${prefix}_MAX_BYTES`), String(identity.capacityGiB * 1024 ** 3));
    assert.equal(environment(`${prefix}_MIN_BYTES`), String((side === 'backup' ? 26 : 60) * 1024 ** 3));
    assert.equal(environment(`${prefix}_RESERVE_BYTES`), String(side === 'backup' ? 256 * 1024 ** 2 : 1024 ** 3));
  }
  const status = pod.status.containerStatuses.find((entry: any) => entry.name === 'verify');
  assert.equal(status.restartCount, 0);
  assert.match(status.containerID, /^containerd:\/\/[a-f0-9]{64}$/);
  if (terminal) {
    assert.equal(pod.status.phase, 'Succeeded');
    assert.equal(status.state.terminated.exitCode, 0);
  } else {
    assert.equal(pod.status.phase, 'Running');
    assert.ok(status.ready && status.state.running);
  }
}

export function renewRestoreLease(read: InventoryReader, side: RestoreSide, podName: string, uid: string, plan: RestorePlan, clock: () => number = Date.now): void {
  const identity = side === 'backup' ? archiveIdentity : plan.scratch;
  const getPod = () => JSON.parse(read(['-n', identity.namespace, 'get', 'pod', podName, '-o', 'json']));
  const before = getPod();
  validateRestorePod(before, plan, uid);
  const lease = attestBackupVolume(read, before, identity, 'verify', clock, side);
  const after = getPod();
  validateRestorePod(after, plan, uid);
  assert.deepEqual(after.spec, before.spec);
  assert.deepEqual(after.status.containerStatuses, before.status.containerStatuses);
  const age = Math.floor(clock() / 1000) - Number(lease.split(' ')[0]);
  assert.ok(age >= 0 && age <= 45);
  read(['-n', identity.namespace, 'exec', podName, '-c', 'verify', '--', '/bin/sh', '-ec',
    `umask 077; test "$POD_UID" = '${uid}'; printf '%s\\n' '${lease}' > /tmp/${side}-volume.lease.partial; mv /tmp/${side}-volume.lease.partial /tmp/${side}-volume.lease`]);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  assert.equal(process.env.PARENT_REVIEW_APPROVED, 'true');
  assert.equal(process.argv.length, 6, 'restore-guard.ts SIDE POD UID PLAN');
  const [side, podName, uid, planFile] = process.argv.slice(2) as [RestoreSide, string, string, string];
  assert.ok(restoreSides.includes(side));
  const plan: RestorePlan = JSON.parse(readFileSync(planFile, 'utf8'));
  validateRestorePlan(plan);
  assert.ok(process.connected, 'Restore guard requires its owning controller IPC connection');
  process.once('disconnect', () => { process.exitCode = 1; process.exit(); });
  const read: InventoryReader = args => execFileSync('kubectl', ['--request-timeout=8s', ...args], { encoding: 'utf8', timeout: 10_000, killSignal: 'SIGKILL', maxBuffer: 8 * 1024 ** 2, stdio: ['ignore', 'pipe', 'pipe'] });
  try {
    await watchCopyLease(() => renewRestoreLease(read, side, podName, uid, plan), () => { console.log('RESTORE_LEASE_READY'); });
  } catch {
    console.error(JSON.stringify({ event: 'restore-guard-stopped', side, at: new Date().toISOString(), leaseSeconds: 45 }));
    process.exitCode = 1;
  }
}
