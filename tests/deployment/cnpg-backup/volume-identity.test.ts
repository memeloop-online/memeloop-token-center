import assert from 'node:assert/strict';
import test from 'node:test';
import { attestStageVolume, stageIdentity, stageIdentityForPod, verifyStageBinding } from './volume-identity.ts';
import { planStageExpansion, stageExpansionClass } from './stage-expansion.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const expected = stageIdentity;
const inventory = () => ({
  claim: { metadata: { name: expected.name, uid: expected.claimUID, namespace: expected.namespace }, spec: { volumeName: expected.name }, status: { phase: 'Bound' } },
  persistent: {
    metadata: { name: expected.name, uid: expected.persistentUID }, status: { phase: 'Bound' },
    spec: { claimRef: { name: expected.name, namespace: expected.namespace, uid: expected.claimUID }, volumeMode: 'Filesystem', persistentVolumeReclaimPolicy: 'Retain', capacity: { storage: '28Gi' }, csi: { driver: 'driver.longhorn.io', fsType: 'xfs', volumeHandle: expected.name } },
  },
  volume: {
    metadata: { name: expected.name, uid: expected.longhornUID, namespace: 'longhorn-system' },
    spec: { size: String(28 * 1024 ** 3), numberOfReplicas: 1, dataLocality: 'strict-local' },
    status: { state: 'attached', robustness: 'healthy', currentNodeID: expected.node },
  },
  replicas: [{ metadata: {}, spec: { volumeName: expected.name, nodeID: expected.node, diskID: expected.diskUUID, diskPath: expected.diskPath, failedAt: '' }, status: { currentState: 'running' } }],
});
const pod = () => ({
  metadata: { namespace: expected.namespace, uid: 'fixture-pod' }, spec: { nodeName: expected.node, containers: [{ name: 'export', env: [
    { name: 'EXPECTED_BACKUP_FS_UUID', value: expected.filesystemUUID },
    { name: 'EXPECTED_BACKUP_DEVICE', value: expected.device },
    { name: 'BACKUP_UUID_ATTESTATION', value: 'external-csi-lease' },
    { name: 'POD_UID', valueFrom: { fieldRef: { fieldPath: 'metadata.uid' } } },
  ] }] },
});

test('explicit stage32 profile still pins the same filesystem and requires actual expanded PV and device capacity', () => {
  const candidate = pod();
  candidate.spec.containers[0]!.env.push(
    { name: 'REVIEWED_STAGE_CAPACITY_GIB', value: '32' },
    { name: 'BACKUP_MAX_BYTES', value: String(32 * 1024 ** 3) },
    { name: 'BACKUP_MIN_BYTES', value: String(30 * 1024 ** 3) },
    { name: 'HARD_CAPACITY_REVIEW_APPROVED', value: 'true' },
  );
  const identity = stageIdentityForPod(candidate, 'export');
  assert.deepEqual(identity, { ...stageIdentity, capacityGiB: 32 });
  const resources = inventory();
  assert.throws(() => verifyStageBinding(resources.claim, resources.persistent, resources.volume, resources.replicas, identity));
  resources.persistent.spec.capacity.storage = '32Gi';
  resources.volume.spec.size = String(32 * 1024 ** 3);
  verifyStageBinding(resources.claim, resources.persistent, resources.volume, resources.replicas, identity);
  assert.throws(() => verifyStageBinding(resources.claim, resources.persistent, resources.volume, resources.replicas));
  for (const [name, value] of [['REVIEWED_STAGE_CAPACITY_GIB', '64'], ['BACKUP_MAX_BYTES', String(64 * 1024 ** 3)], ['BACKUP_MIN_BYTES', '0'], ['HARD_CAPACITY_REVIEW_APPROVED', 'false']] as const) {
    const altered = structuredClone(candidate);
    altered.spec.containers[0]!.env.find(entry => entry.name === name)!.value = value;
    assert.throws(() => stageIdentityForPod(altered, 'export'));
  }
});

function expansionInventory(): any {
  const existing: any = inventory();
  existing.claim.metadata.resourceVersion = '100';
  existing.claim.spec.storageClassName = stageExpansionClass;
  existing.claim.spec.resources = { requests: { storage: '28Gi' } };
  existing.claim.status.capacity = { storage: '28Gi' };
  existing.volume.status.state = 'detached';
  existing.replicas[0].status.currentState = 'stopped';
  existing.volume.spec.snapshotMaxCount = 2;
  existing.volume.spec.snapshotMaxSize = String(56 * 1024 ** 3);
  const disk = { storageMaximum: 1000 * 1024 ** 3, storageScheduled: 150 * 1024 ** 3, storageAvailable: 850 * 1024 ** 3,
    diskUUID: stageIdentity.diskUUID, conditions: [{ type: 'Ready', status: 'True' }, { type: 'Schedulable', status: 'True' }] };
  return { ...existing, observedAt: '2026-10-06T21:00:00Z', consumers: [], classClaimUIDs: [stageIdentity.claimUID],
    storageClass: { metadata: { name: stageExpansionClass, uid: '55555555-5555-5555-5555-555555555555', resourceVersion: '99' }, provisioner: 'driver.longhorn.io', reclaimPolicy: 'Retain', allowVolumeExpansion: false },
    node: { metadata: { name: stageIdentity.node }, spec: { allowScheduling: true, disks: { data1: { allowScheduling: true, path: stageIdentity.diskPath, storageReserved: 0 } } }, status: { diskStatus: { data1: disk } } },
    filesystem: { diskUUID: stageIdentity.diskUUID, availableBytes: 850 * 1024 ** 3, freeInodes: 100_000 },
    settings: { overProvisioning: '100', minimalAvailable: '25' },
  };
}

test('stage40 requires actual matching capacity and preserves the original stage identity', () => {
  const candidate = pod();
  candidate.spec.containers[0]!.env.push(
    { name: 'REVIEWED_STAGE_CAPACITY_GIB', value: '40' },
    { name: 'BACKUP_MAX_BYTES', value: String(40 * 1024 ** 3) },
    { name: 'BACKUP_MIN_BYTES', value: String(38 * 1024 ** 3) },
    { name: 'HARD_CAPACITY_REVIEW_APPROVED', value: 'true' },
  );
  const identity = stageIdentityForPod(candidate, 'export');
  assert.deepEqual(identity, { ...stageIdentity, capacityGiB: 40 });
  const resources = inventory();
  resources.persistent.spec.capacity.storage = '32Gi';
  resources.volume.spec.size = String(32 * 1024 ** 3);
  assert.throws(() => verifyStageBinding(resources.claim, resources.persistent, resources.volume, resources.replicas, identity));
  resources.persistent.spec.capacity.storage = '40Gi';
  resources.volume.spec.size = String(40 * 1024 ** 3);
  verifyStageBinding(resources.claim, resources.persistent, resources.volume, resources.replicas, identity);
  candidate.spec.containers[0]!.env.find(entry => entry.name === 'BACKUP_MIN_BYTES')!.value = String(30 * 1024 ** 3);
  assert.throws(() => stageIdentityForPod(candidate, 'export'));
});

test('32-to40 planning charges the complete new budget and cannot reuse 28Gi or active inventories', () => {
  const snapshot = expansionInventory();
  const now = Date.parse(snapshot.observedAt);
  assert.throws(() => planStageExpansion(snapshot, now, 40));
  snapshot.claim.spec.resources.requests.storage = '32Gi';
  snapshot.claim.status.capacity.storage = '32Gi';
  snapshot.persistent.spec.capacity.storage = '32Gi';
  snapshot.volume.spec.size = String(32 * 1024 ** 3);
  snapshot.volume.spec.snapshotMaxSize = String(64 * 1024 ** 3);
  const plan: any = planStageExpansion(snapshot, now, 40);
  assert.equal(plan.oldGiB, 32);
  assert.equal(plan.targetGiB, 40);
  assert.equal(plan.physicalBudgetBytes, 162 * 1024 ** 3);
  assert.equal(plan.backingAfterWorstCaseBytes, 538 * 1024 ** 3);
  assert.equal(plan.executionAuthorized, false);
  assert.equal(plan.partialDeletionAuthorized, false);
  assert.equal(plan.sourceProtectionChanged, false);
  assert.equal(plan.restoreAllocationAuthorized, false);
  assert.deepEqual(plan.patches.map((entry: any) => entry.resource), ['storageclass', 'pvc']);
  assert.deepEqual(plan.patches[1].patch.slice(-2), [
    { op: 'test', path: '/spec/resources/requests/storage', value: '32Gi' },
    { op: 'replace', path: '/spec/resources/requests/storage', value: '40Gi' },
  ]);
  assert.throws(() => planStageExpansion(snapshot, now));
  assert.throws(() => planStageExpansion(snapshot, now, 64 as 40));
  const insufficient = structuredClone(snapshot);
  insufficient.filesystem.availableBytes = 550 * 1024 ** 3;
  assert.throws(() => planStageExpansion(insufficient, now, 40));
  snapshot.consumers = [{ status: { phase: 'Running', containerStatuses: [{ state: { running: {} } }] } }];
  assert.throws(() => planStageExpansion(snapshot, now, 40));
});

test('stage expansion plans only two identity/version guarded changes, never apply or shrink', () => {
  const snapshot = expansionInventory();
  const plan: any = planStageExpansion(snapshot, Date.parse(snapshot.observedAt));
  assert.equal(plan.executionAuthorized, false);
  assert.equal(plan.targetGiB, 32);
  assert.equal(plan.physicalBudgetBytes, 130 * 1024 ** 3);
  assert.equal(plan.backingAfterWorstCaseBytes, 570 * 1024 ** 3);
  assert.equal(plan.sourceProtectionChanged, false);
  assert.equal(plan.partialDeletionAuthorized, false);
  assert.equal(plan.restoreAllocationAuthorized, false);
  assert.deepEqual(plan.patches.map((entry: any) => entry.resource), ['storageclass', 'pvc']);
  assert.deepEqual(plan.patches[0].patch.slice(0, 2), [{ op: 'test', path: '/metadata/uid', value: snapshot.storageClass.metadata.uid }, { op: 'test', path: '/metadata/resourceVersion', value: '99' }]);
  assert.deepEqual(plan.patches[1].patch, [
    { op: 'test', path: '/metadata/uid', value: stageIdentity.claimUID }, { op: 'test', path: '/metadata/resourceVersion', value: '100' },
    { op: 'test', path: '/spec/resources/requests/storage', value: '28Gi' }, { op: 'replace', path: '/spec/resources/requests/storage', value: '32Gi' },
  ]);
  assert.match(plan.closeout, /allowVolumeExpansion=false/);
  assert.match(plan.rollback, /do not shrink/);
});

test('source substitution, active consumers, expansion already pending, stale inventory and insufficient backing space reject planning', () => {
  for (const mutate of [
    (value: any) => { value.claim.metadata.uid = 'source-primary'; },
    (value: any) => { value.persistent.spec.csi.volumeHandle = 'source-primary'; },
    (value: any) => { value.volume.status.state = 'attached'; },
    (value: any) => { value.claim.status.capacity.storage = '32Gi'; },
    (value: any) => { value.claim.status.conditions = [{ type: 'Resizing', status: 'True' }]; },
    (value: any) => { value.storageClass.allowVolumeExpansion = true; },
    (value: any) => { value.classClaimUIDs.push('another-claim'); },
    (value: any) => { value.consumers.push({ status: { phase: 'Running' } }); },
    (value: any) => { value.replicas[0].spec.diskID = 'source-disk'; },
    (value: any) => { value.replicas[0].status.currentState = 'running'; },
    (value: any) => { value.persistent.spec.claimRef.namespace = 'another-namespace'; },
    (value: any) => { value.filesystem.freeInodes = 100; },
    (value: any) => { value.filesystem.availableBytes = 500 * 1024 ** 3; },
    (value: any) => { value.settings.overProvisioning = '200'; },
    (value: any) => { value.observedAt = '2026-10-06T20:58:29Z'; },
    (value: any) => { value.claim.metadata.resourceVersion = ''; },
  ]) {
    const snapshot = expansionInventory();
    mutate(snapshot);
    assert.throws(() => planStageExpansion(snapshot, Date.parse('2026-10-06T21:00:00Z')));
  }
});

test('CSI attestation pins NEW PVC, PV, Longhorn UID and unique reviewed replica placement', () => {
  const check = (value: ReturnType<typeof inventory>) => verifyStageBinding(value.claim, value.persistent, value.volume, value.replicas);
  check(inventory());
  for (const mutate of [
    (value: any) => { value.claim.metadata.uid = 'replaced'; },
    (value: any) => { value.persistent.metadata.uid = 'replaced'; },
    (value: any) => { value.volume.metadata.uid = 'replaced'; },
    (value: any) => { value.claim.metadata.deletionTimestamp = 'now'; },
    (value: any) => { value.persistent.spec.claimRef.uid = 'wrong'; },
    (value: any) => { value.persistent.spec.csi.volumeHandle = 'source-pgdata'; },
    (value: any) => { value.persistent.spec.capacity.storage = '64Gi'; },
    (value: any) => { value.volume.spec.size = String(64 * 1024 ** 3); },
    (value: any) => { value.volume.spec.numberOfReplicas = 2; },
    (value: any) => { value.volume.status.state = 'detached'; },
    (value: any) => { value.volume.status.currentNodeID = 'other'; },
    (value: any) => { value.replicas[0].spec.diskID = 'source-disk'; },
    (value: any) => { value.replicas[0].spec.failedAt = 'now'; },
    (value: any) => { value.replicas.push(value.replicas[0]); },
  ]) {
    const value = inventory();
    mutate(value);
    assert.throws(() => check(value));
  }
});

test('attestor reads only the exact NEW block device and expires observations before lease publication', () => {
  const observed = Date.parse('2026-10-05T19:30:00Z');
  const value = inventory();
  const plugin = { metadata: { name: 'reviewed-csi' }, spec: { nodeName: expected.node }, status: { phase: 'Running', containerStatuses: [{ name: 'longhorn-csi-plugin', ready: true }] } };
  let plugins = [plugin];
  let identity = `UUID=${expected.filesystemUUID}\nTYPE=xfs\n`;
  let number = '8:20';
  const commands: string[][] = [];
  const read = (args: string[]) => {
    commands.push(args);
    if (args.includes('exec')) {
      const prefix = ['-n', 'longhorn-system', 'exec', 'reviewed-csi', '-c', 'longhorn-csi-plugin', '--'];
      if (args.includes('blkid')) {
        assert.deepEqual(args, [...prefix, 'blkid', '-p', '-o', 'export', expected.device]);
        return identity;
      }
      assert.deepEqual(args, [...prefix, 'stat', '-L', '-c', '%t:%T', expected.device]);
      return number;
    }
    const kind = args[args.indexOf('get') + 1];
    return JSON.stringify(kind === 'pvc' ? value.claim : kind === 'pv' ? value.persistent : kind === 'volumes.longhorn.io' ? value.volume : kind === 'replicas.longhorn.io' ? { items: value.replicas } : { items: plugins });
  };
  assert.equal(attestStageVolume(read, pod(), () => observed), `${observed / 1000} fixture-pod ${expected.device} ${expected.filesystemUUID} 8:32`);
  assert.equal(commands.filter(args => args.includes('exec')).length, 2);
  for (const invalid of ['', 'UUID=wrong\nTYPE=xfs', `UUID=${expected.filesystemUUID}\nTYPE=ext4`, identity + identity]) {
    const original = identity;
    identity = invalid;
    assert.throws(() => attestStageVolume(read, pod(), () => observed));
    identity = original;
  }
  number = '0:0';
  assert.throws(() => attestStageVolume(read, pod(), () => observed));
  number = '8:20';
  plugins = [plugin, plugin];
  assert.throws(() => attestStageVolume(read, pod(), () => observed));
  plugins = [];
  assert.throws(() => attestStageVolume(read, pod(), () => observed));
  plugins = [plugin];
  for (const elapsed of [-1000, 46_000]) {
    let calls = 0;
    assert.throws(() => attestStageVolume(read, pod(), () => observed + (calls++ === 0 ? 0 : elapsed)));
  }
  const replaced = pod();
  replaced.metadata.uid = "invalid'pod";
  assert.throws(() => attestStageVolume(read, replaced, () => observed));
});
