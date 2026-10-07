import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import { stageIdentity } from './volume-identity.ts';

const gib = 1024 ** 3;
export const stageExpansionClass = 'mtc-cnpg-stage-haixia-data1-20261005';
export const expandedStageGiB = 32;

export function planStageExpansion(snapshot: any, now = Date.now(), targetGiB: 32 | 40 | 56 = expandedStageGiB): object {
  assert.ok([32, 40, 56].includes(targetGiB), 'Unknown reviewed expansion target');
  const oldGiB = targetGiB === 32 ? 28 : targetGiB === 40 ? 32 : 40;
  const age = now - Date.parse(snapshot.observedAt);
  assert.ok(Number.isFinite(age) && age >= 0 && age <= 90_000, 'Expansion inventory must be fresh');
  const { claim, persistent, volume, storageClass, node, replicas, consumers, filesystem, settings } = snapshot;
  for (const [resource, uid] of [[claim, stageIdentity.claimUID], [persistent, stageIdentity.persistentUID], [volume, stageIdentity.longhornUID]]) {
    assert.equal(resource.metadata.uid, uid);
    assert.equal(resource.metadata.name, stageIdentity.name);
    assert.ok(!resource.metadata.deletionTimestamp);
  }
  assert.equal(claim.metadata.namespace, stageIdentity.namespace);
  assert.equal(claim.status.phase, 'Bound');
  assert.equal(claim.spec.storageClassName, stageExpansionClass);
  assert.equal(claim.spec.volumeName, stageIdentity.name);
  assert.equal(claim.spec.resources.requests.storage, `${oldGiB}Gi`);
  assert.equal(claim.status.capacity.storage, `${oldGiB}Gi`);
  assert.ok(!claim.status.conditions?.some((condition: any) => condition.status === 'True'));
  assert.equal(persistent.spec.capacity.storage, `${oldGiB}Gi`);
  assert.equal(persistent.status.phase, 'Bound');
  assert.equal(persistent.spec.volumeMode, 'Filesystem');
  assert.equal(persistent.spec.claimRef.uid, stageIdentity.claimUID);
  assert.equal(persistent.spec.claimRef.name, stageIdentity.name);
  assert.equal(persistent.spec.claimRef.namespace, stageIdentity.namespace);
  assert.equal(persistent.spec.persistentVolumeReclaimPolicy, 'Retain');
  assert.equal(persistent.spec.csi.driver, 'driver.longhorn.io');
  assert.equal(persistent.spec.csi.volumeHandle, stageIdentity.name);
  assert.equal(persistent.spec.csi.fsType, 'xfs');
  assert.equal(volume.spec.size, String(oldGiB * gib));
  assert.equal(volume.metadata.namespace, 'longhorn-system');
  assert.equal(volume.spec.numberOfReplicas, 1);
  assert.equal(volume.spec.dataLocality, 'strict-local');
  assert.equal(volume.spec.snapshotMaxCount, 2);
  assert.equal(volume.spec.snapshotMaxSize, String(2 * oldGiB * gib));
  assert.equal(volume.status.state, 'detached');
  assert.equal(replicas.length, 1);
  assert.ok(!replicas[0].metadata.deletionTimestamp);
  assert.equal(replicas[0].spec.volumeName, stageIdentity.name);
  assert.equal(replicas[0].spec.nodeID, stageIdentity.node);
  assert.equal(replicas[0].spec.diskID, stageIdentity.diskUUID);
  assert.equal(replicas[0].spec.diskPath, stageIdentity.diskPath);
  assert.equal(replicas[0].spec.failedAt, '');
  assert.equal(replicas[0].status.currentState, 'stopped');
  assert.equal(storageClass.metadata.name, stageExpansionClass);
  assert.match(storageClass.metadata.uid, /^[a-f0-9-]{36}$/);
  assert.ok(!storageClass.metadata.deletionTimestamp);
  assert.equal(storageClass.provisioner, 'driver.longhorn.io');
  assert.equal(storageClass.allowVolumeExpansion, false);
  assert.equal(storageClass.reclaimPolicy, 'Retain');
  assert.deepEqual(snapshot.classClaimUIDs, [stageIdentity.claimUID], 'Dedicated class must have no other claims');
  for (const consumer of consumers) {
    assert.ok(['Succeeded', 'Failed'].includes(consumer.status.phase));
    assert.ok(consumer.status.containerStatuses?.every((status: any) => status.state.terminated));
    assert.ok((consumer.status.initContainerStatuses ?? []).every((status: any) => status.state.terminated));
  }
  assert.equal(node.metadata.name, stageIdentity.node);
  assert.equal(node.spec.allowScheduling, true);
  const matches = Object.entries(node.status.diskStatus).filter(([, status]: [string, any]) => status.diskUUID === stageIdentity.diskUUID);
  assert.equal(matches.length, 1);
  const [diskName, status] = matches[0]! as [string, any];
  const disk = node.spec.disks[diskName];
  assert.equal(disk.path, stageIdentity.diskPath);
  assert.equal(disk.allowScheduling, true);
  for (const type of ['Ready', 'Schedulable']) assert.equal(status.conditions.find((condition: any) => condition.type === type)?.status, 'True');
  assert.equal(settings.overProvisioning, '100');
  assert.equal(settings.minimalAvailable, '25');
  assert.equal(filesystem.diskUUID, stageIdentity.diskUUID);
  for (const value of [filesystem.availableBytes, filesystem.freeInodes, status.storageMaximum, status.storageScheduled, status.storageAvailable, disk.storageReserved ?? 0]) assert.ok(Number.isSafeInteger(value) && value >= 0);
  assert.ok(filesystem.freeInodes >= 65_536);
  const budget = (4 * targetGiB + 2) * gib;
  const floor = Math.max(status.storageMaximum / 4, disk.storageReserved ?? 0, 32 * gib);
  const remaining = Math.min(filesystem.availableBytes, status.storageAvailable) - status.storageScheduled - budget;
  assert.ok(remaining >= floor);
  assert.ok(status.storageScheduled + budget <= status.storageMaximum - (disk.storageReserved ?? 0));
  const patch = (resource: any, path: string, before: unknown, after: unknown) => {
    assert.match(resource.metadata.resourceVersion, /^[0-9]+$/);
    return [
      { op: 'test', path: '/metadata/uid', value: resource.metadata.uid },
      { op: 'test', path: '/metadata/resourceVersion', value: resource.metadata.resourceVersion },
      { op: 'test', path, value: before },
      { op: 'replace', path, value: after },
    ];
  };
  return {
    schema: 1, executionAuthorized: false, oldGiB, targetGiB,
    claimUID: stageIdentity.claimUID, persistentUID: stageIdentity.persistentUID, longhornUID: stageIdentity.longhornUID,
    filesystemUUID: stageIdentity.filesystemUUID, device: stageIdentity.device, node: stageIdentity.node, diskUUID: stageIdentity.diskUUID,
    physicalBudgetBytes: budget, backingReserveFloorBytes: floor, backingAfterWorstCaseBytes: remaining,
    patches: [
      { resource: 'storageclass', name: stageExpansionClass, patch: patch(storageClass, '/allowVolumeExpansion', false, true) },
      { resource: 'pvc', namespace: stageIdentity.namespace, name: stageIdentity.name, patch: patch(claim, '/spec/resources/requests/storage', `${oldGiB}Gi`, `${targetGiB}Gi`) },
    ],
    closeout: `Read fresh identities and resourceVersions, require CSI-controlled PV/Longhorn/filesystem growth to${targetGiB}Gi with same UUID and all partials retained; restore dedicated StorageClass allowVolumeExpansion=false using fresh UID/resourceVersion preconditions. No manual PV size patch, format, shrink, primary mount, source SQL or export.`,
    rollback: 'Before PVC request changes, the sole reversible capability change is disabling expansion with fresh identity/version. After PVC request changes do not shrink or restore old capacity assertions; preserve data, stop and investigate reconciliation. No automatic retry.',
    sourceProtectionChanged: false, partialDeletionAuthorized: false, restoreAllocationAuthorized: false,
  };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  assert.ok([3, 4].includes(process.argv.length), 'stage-expansion.ts REVIEWED_FRESH_INVENTORY_JSON [32|40|56]; plan only, never applies');
  console.log(JSON.stringify(planStageExpansion(JSON.parse(readFileSync(process.argv[2]!, 'utf8')), Date.now(), Number(process.argv[3] ?? 32) as 32 | 40 | 56), null, 2));
}
