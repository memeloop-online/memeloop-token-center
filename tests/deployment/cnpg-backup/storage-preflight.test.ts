import assert from 'node:assert/strict';
import test from 'node:test';
import { evaluateStorage, storagePlan, storageResources, validateStorageResources } from './storage-preflight.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const gib = 1024 ** 3;

function fixture(): { nodes: any[]; settings: Record<string, string>; replicas: any[]; filesystem: Record<string, { availableBytes: number; freeInodes: number }> } {
  const nodes: any[] = [];
  const filesystem: Record<string, { availableBytes: number; freeInodes: number }> = {};
  for (const volume of storagePlan.volumes) {
    if (filesystem[volume.diskUUID]) continue;
    nodes.push({
      metadata: { name: volume.node },
      spec: { allowScheduling: true, tags: [...volume.nodeTags], disks: {
        [volume.disk]: { allowScheduling: true, tags: [...volume.diskTags], path: volume.path, storageReserved: 0 },
      } },
      status: { diskStatus: { [volume.disk]: {
        diskUUID: volume.diskUUID, storageAvailable: 850 * gib, storageMaximum: 1000 * gib, storageScheduled: 150 * gib,
        conditions: [{ type: 'Ready', status: 'True' }, { type: 'Schedulable', status: 'True' }],
      } } },
    });
    filesystem[volume.diskUUID] = { availableBytes: 850 * gib, freeInodes: 100_000 };
  }
  return {
    nodes, filesystem,
    settings: { 'storage-over-provisioning-percentage': '100', 'storage-minimal-available-percentage': '25', 'default-engine-image': storagePlan.engine },
    replicas: [{ spec: { nodeID: storagePlan.sourceNode, diskID: storagePlan.sourceDiskUUID } }],
  };
}

test('exact static volumes enforce snapshot limits on Volume CRs, not unsupported SC parameters', () => {
  validateStorageResources();
  for (const mutate of [
    (items: any[]) => { items.find(item => item.kind === 'Volume').spec.snapshotMaxSize = '0'; },
    (items: any[]) => { items.find(item => item.kind === 'Volume').spec.snapshotMaxCount = 250; },
    (items: any[]) => { items.find(item => item.kind === 'Volume').metadata.labels['recurring-job-group.longhorn.io/default'] = 'enabled'; },
    (items: any[]) => { items.find(item => item.kind === 'StorageClass').parameters.snapshotMaxCount = '2'; },
    (items: any[]) => { items.find(item => item.kind === 'StorageClass').allowVolumeExpansion = true; },
    (items: any[]) => { items.find(item => item.kind === 'PersistentVolume').spec.persistentVolumeReclaimPolicy = 'Delete'; },
    (items: any[]) => { items.find(item => item.kind === 'PersistentVolumeClaim').spec.volumeName = ''; },
  ]) {
    const changed = structuredClone(storageResources);
    mutate(changed);
    assert.throws(() => validateStorageResources(changed));
  }
});

test('unique current tag intersections and full physical liabilities are mandatory', () => {
  const evaluate = (candidate: ReturnType<typeof fixture>) => evaluateStorage(candidate.nodes, candidate.settings, candidate.replicas, candidate.filesystem);
  const pools = evaluate(fixture());
  assert.equal(pools.length, 2);
  assert.equal(pools[0].additionalWorstCaseBytes, 114 * gib);
  assert.equal(pools[1].additionalWorstCaseBytes, 372 * gib);
  assert.equal(pools[1].afterWorstCaseBytes, 328 * gib);
  for (const mutate of [
    (candidate: ReturnType<typeof fixture>) => { candidate.nodes.push(structuredClone(candidate.nodes[0])); },
    (candidate: ReturnType<typeof fixture>) => { const node = candidate.nodes[0]; node.spec.disks.extra = structuredClone(Object.values(node.spec.disks)[0]); },
    (candidate: ReturnType<typeof fixture>) => { candidate.settings['storage-over-provisioning-percentage'] = '200'; },
    (candidate: ReturnType<typeof fixture>) => { candidate.settings['default-engine-image'] = 'different-engine'; },
    (candidate: ReturnType<typeof fixture>) => { candidate.replicas[0].spec.diskID = storagePlan.volumes[0].diskUUID; },
    (candidate: ReturnType<typeof fixture>) => { candidate.nodes[0].spec.allowScheduling = false; },
    (candidate: ReturnType<typeof fixture>) => { candidate.filesystem[storagePlan.volumes[0].diskUUID]!.freeInodes = 100; },
    (candidate: ReturnType<typeof fixture>) => { candidate.filesystem[storagePlan.volumes[1].diskUUID]!.availableBytes = 600 * gib; },
    (candidate: ReturnType<typeof fixture>) => { candidate.nodes[1].status.diskStatus[storagePlan.volumes[1].disk].storageScheduled = 700 * gib; },
  ]) {
    const candidate = fixture();
    mutate(candidate);
    assert.throws(() => evaluate(candidate));
  }
});
