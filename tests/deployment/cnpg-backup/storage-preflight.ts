import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath, pathToFileURL } from 'node:url';
import { sourceBudget } from './source-space.ts';
import { readSourceFilesystem, sourceFilesystem } from './source-filesystem.ts';

const directory = dirname(fileURLToPath(import.meta.url));
export const storagePath = join(directory, 'exact-storage.json');
export const storagePlan = JSON.parse(readFileSync(join(directory, 'storage-plan.json'), 'utf8'));
export const storageResources = JSON.parse(readFileSync(storagePath, 'utf8')).items;
const gib = 1024 ** 3;

export function validateStorageResources(resources: any[] = storageResources, volumes: any[] = storagePlan.volumes): void {
  assert.equal(resources.length, volumes.length * 4);
  for (const volume of volumes) {
    const find = (kind: string, name: string) => {
      const matches = resources.filter(resource => resource.kind === kind && resource.metadata.name === name);
      assert.equal(matches.length, 1);
      return matches[0];
    };
    const storageClass = find('StorageClass', volume.storageClass);
    assert.equal(storageClass.provisioner, 'driver.longhorn.io');
    assert.equal(storageClass.reclaimPolicy, 'Retain');
    assert.equal(storageClass.allowVolumeExpansion, false);
    assert.equal(storageClass.volumeBindingMode, 'WaitForFirstConsumer');
    assert.equal(storageClass.parameters.nodeSelector, volume.nodeTags.join(','));
    assert.equal(storageClass.parameters.diskSelector, volume.diskTags.join(','));
    assert.equal(storageClass.parameters.numberOfReplicas, '1');
    assert.equal(storageClass.parameters.dataLocality, 'strict-local');
    assert.equal(storageClass.parameters.recurringJobSelector, '[]');
    assert.equal(storageClass.parameters.snapshotMaxCount, undefined);
    assert.equal(storageClass.parameters.snapshotMaxSize, undefined);
    const longhorn = find('Volume', volume.name);
    assert.equal(longhorn.metadata.namespace, 'longhorn-system');
    assert.equal(longhorn.metadata.labels['recurring-job-group.longhorn.io/default'], 'disabled');
    assert.ok(!Object.entries(longhorn.metadata.labels).some(([name, value]) => name.startsWith('recurring-job') && value === 'enabled'));
    assert.equal(longhorn.spec.image, storagePlan.engine);
    assert.equal(longhorn.spec.dataEngine, 'v1');
    assert.equal(longhorn.spec.nodeID, '');
    assert.equal(longhorn.spec.numberOfReplicas, 1);
    assert.equal(longhorn.spec.dataLocality, 'strict-local');
    assert.equal(longhorn.spec.size, String(volume.sizeGiB * gib));
    assert.equal(longhorn.spec.snapshotMaxCount, 2);
    assert.equal(longhorn.spec.snapshotMaxSize, String(2 * volume.sizeGiB * gib));
    assert.equal(longhorn.spec.replicaAutoBalance, 'disabled');
    assert.equal(longhorn.spec.offlineRebuilding, 'disabled');
    assert.equal(longhorn.spec.restoreVolumeRecurringJob, 'disabled');
    assert.deepEqual(longhorn.spec.nodeSelector, volume.nodeTags);
    assert.deepEqual(longhorn.spec.diskSelector, volume.diskTags);
    const persistent = find('PersistentVolume', volume.name);
    assert.equal(persistent.spec.csi.driver, 'driver.longhorn.io');
    assert.equal(persistent.spec.csi.volumeHandle, volume.name);
    assert.equal(persistent.spec.csi.fsType, 'xfs');
    assert.equal(persistent.spec.persistentVolumeReclaimPolicy, 'Retain');
    assert.equal(persistent.spec.storageClassName, volume.storageClass);
    assert.equal(persistent.spec.capacity.storage, `${volume.sizeGiB}Gi`);
    assert.equal(persistent.spec.claimRef.name, volume.name);
    assert.equal(persistent.spec.claimRef.namespace, 'memeloop-token-center');
    assert.deepEqual(persistent.spec.nodeAffinity.required.nodeSelectorTerms, [{ matchExpressions: [{ key: 'kubernetes.io/hostname', operator: 'In', values: [volume.node] }] }]);
    const claim = find('PersistentVolumeClaim', volume.name);
    assert.equal(claim.metadata.namespace, 'memeloop-token-center');
    assert.equal(claim.spec.storageClassName, volume.storageClass);
    assert.equal(claim.spec.volumeName, volume.name);
    assert.equal(claim.spec.resources.requests.storage, `${volume.sizeGiB}Gi`);
    for (const resource of [storageClass, longhorn, persistent, claim]) {
      assert.equal(resource.metadata.annotations['recovery.mtc/expected-disk-uuid'], volume.diskUUID);
      assert.equal(resource.metadata.annotations['recovery.mtc/physical-budget-bytes'], String((4 * volume.sizeGiB + 2) * gib));
    }
  }
}

export function evaluateStorage(nodes: any[], settings: Record<string, string>, sourceReplicas: any[], filesystem: Record<string, { availableBytes: number; freeInodes: number }>): any[] {
  assert.equal(settings['storage-over-provisioning-percentage'], '100');
  assert.equal(settings['storage-minimal-available-percentage'], '25');
  assert.equal(settings['default-engine-image'], storagePlan.engine);
  assert.equal(sourceReplicas.length, 1, 'Re-review if the source replica topology changes');
  assert.equal(sourceReplicas[0].spec.nodeID, storagePlan.sourceNode);
  assert.equal(sourceReplicas[0].spec.diskID, storagePlan.sourceDiskUUID);
  const pools = new Map<string, any>();
  for (const volume of storagePlan.volumes) {
    const matchingNodes = nodes.filter(node => volume.nodeTags.every((tag: string) => node.spec.tags.includes(tag)));
    assert.equal(matchingNodes.length, 1, `Node tags are not unique for ${volume.name}`);
    const node = matchingNodes[0];
    assert.equal(node.metadata.name, volume.node);
    assert.equal(node.spec.allowScheduling, true);
    const disks = Object.entries(node.spec.disks).filter(([, disk]: [string, any]) => volume.diskTags.every((tag: string) => disk.tags.includes(tag)));
    assert.equal(disks.length, 1, `Disk tags are not unique for ${volume.name}`);
    assert.equal(disks[0]![0], volume.disk);
    const specification: any = disks[0]![1];
    assert.equal(specification.allowScheduling, true);
    assert.equal(specification.path, volume.path);
    const status = node.status.diskStatus[volume.disk];
    assert.equal(status.diskUUID, volume.diskUUID);
    assert.notEqual(status.diskUUID, storagePlan.sourceDiskUUID);
    for (const condition of ['Ready', 'Schedulable']) assert.equal(status.conditions.find((entry: any) => entry.type === condition)?.status, 'True');
    const observed = filesystem[volume.diskUUID];
    assert.ok(observed);
    assert.ok(Number.isSafeInteger(observed.freeInodes) && observed.freeInodes >= storagePlan.minimumFreeInodes);
    const key = `${volume.node}/${volume.disk}`;
    const pool = pools.get(key) ?? {
      node: volume.node, disk: volume.disk, diskUUID: status.diskUUID, path: volume.path,
      maximumBytes: status.storageMaximum, reservedBytes: specification.storageReserved ?? 0,
      scheduledBytes: status.storageScheduled,
      availableBytes: Math.min(status.storageAvailable, observed.availableBytes),
      freeInodes: observed.freeInodes, additionalWorstCaseBytes: 0, volumes: [],
    };
    pool.additionalWorstCaseBytes += (4 * volume.sizeGiB + 2) * gib;
    pool.volumes.push(volume.name);
    pools.set(key, pool);
  }
  return [...pools.values()].map(pool => {
    for (const field of ['maximumBytes', 'reservedBytes', 'scheduledBytes', 'availableBytes']) assert.ok(Number.isSafeInteger(pool[field]) && pool[field] >= 0);
    pool.remainingLogicalCommitmentBytes = pool.scheduledBytes;
    pool.reserveFloorBytes = Math.max(pool.maximumBytes * 0.25, pool.reservedBytes, storagePlan.hostFreeReserveGiB * gib);
    pool.afterWorstCaseBytes = pool.availableBytes - pool.remainingLogicalCommitmentBytes - pool.additionalWorstCaseBytes;
    assert.ok(pool.scheduledBytes + pool.additionalWorstCaseBytes <= pool.maximumBytes - pool.reservedBytes, '100% physical commitment budget exceeded');
    assert.ok(pool.afterWorstCaseBytes >= pool.reserveFloorBytes, 'Worst-case full writes/snapshots/maintenance would breach backing-disk reserve');
    return pool;
  });
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const backupOnly = process.argv.includes('--backup-only');
  assert.deepEqual(process.argv.slice(2), backupOnly ? ['--backup-only', '--server-dry-run'] : ['--server-dry-run'], 'Only read-only inventory plus server dry-run is supported');
  validateStorageResources();
  const selectedVolumes = storagePlan.volumes.filter((volume: any) => !backupOnly || volume.role !== 'scratch');
  const selectedResources = backupOnly ? JSON.parse(readFileSync(join(directory, 'backup-storage.json'), 'utf8')).items : storageResources;
  validateStorageResources(selectedResources, selectedVolumes);
  assert.deepEqual(selectedResources, storageResources.filter((resource: any) => selectedVolumes.some((volume: any) => [volume.name, volume.storageClass].includes(resource.metadata.name))));
  const kubectl = (args: string[], input?: string) => execFileSync('kubectl', ['--request-timeout=30s', ...args], { input, encoding: 'utf8', timeout: 60_000, maxBuffer: 8 * 1024 ** 2 });
  const get = (args: string[]) => JSON.parse(kubectl([...args, '-o', 'json']));
  const source = readSourceFilesystem((args, timeoutMs = 20_000) => {
    try {
      return execFileSync('kubectl', [timeoutMs === sourceFilesystem.processTimeoutMs ? '--request-timeout=8s' : '--request-timeout=15s', ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], timeout: timeoutMs, killSignal: 'SIGKILL', maxBuffer: 8 * 1024 ** 2 });
    } catch {
      throw new Error('Read-only source identity/stat preflight failed');
    }
  });
  sourceBudget(source);
  const nodes = get(['-n', 'longhorn-system', 'get', 'nodes.longhorn.io']).items;
  const settingNames = ['storage-over-provisioning-percentage', 'storage-minimal-available-percentage', 'default-engine-image'];
  const settings = Object.fromEntries(get(['-n', 'longhorn-system', 'get', 'settings.longhorn.io', ...settingNames]).items.map((item: any) => [item.metadata.name, item.value]));
  const replicas = get(['-n', 'longhorn-system', 'get', 'replicas.longhorn.io', '-l', `longhornvolume=${storagePlan.sourceVolume}`]).items;
  const managers = get(['-n', 'longhorn-system', 'get', 'pods', '-l', 'app=longhorn-manager']).items;
  const filesystem: Record<string, { availableBytes: number; freeInodes: number }> = {};
  for (const volume of storagePlan.volumes) {
    if (filesystem[volume.diskUUID]) continue;
    const manager = managers.find((pod: any) => pod.spec.nodeName === volume.node && pod.status.phase === 'Running');
    assert.ok(manager);
    const values = kubectl(['-n', 'longhorn-system', 'exec', manager.metadata.name, '-c', 'longhorn-manager', '--', 'stat', '-f', '-c', '%S %a %d', `/host/proc/1/root${volume.path}`]).trim().split(/\s+/).map(Number);
    assert.equal(values.length, 3);
    filesystem[volume.diskUUID] = { availableBytes: values[0]! * values[1]!, freeInodes: values[2]! };
  }
  const pools = evaluateStorage(nodes, settings, replicas, filesystem);
  const inventory = {
    StorageClass: get(['get', 'storageclasses']).items,
    Volume: get(['-n', 'longhorn-system', 'get', 'volumes.longhorn.io']).items,
    PersistentVolume: get(['get', 'persistentvolumes']).items,
    PersistentVolumeClaim: get(['-n', 'memeloop-token-center', 'get', 'persistentvolumeclaims']).items,
  };
  for (const resource of selectedResources) {
    assert.ok(!inventory[resource.kind as keyof typeof inventory].some((entry: any) => entry.metadata.name === resource.metadata.name), `Refusing to prepare over existing ${resource.kind}/${resource.metadata.name}`);
  }
  const admitted = selectedResources.map((resource: any) => JSON.parse(kubectl(['create', '--dry-run=server', '--validate=strict', '-f', '-', '-o', 'json'], JSON.stringify(resource))));
  validateStorageResources(admitted, selectedVolumes);
  console.log(JSON.stringify({ observedAt: new Date().toISOString(), result: 'server-dry-run-only-no-resources-persisted', source, resources: admitted.map((entry: any) => `${entry.kind}/${entry.metadata.name}`), pools, budgetIncludesDeferredRestore: true, physicalReservationCreated: false, remainingTask: storagePlan.longTermPhysicalWalProtection }, null, 2));
}
