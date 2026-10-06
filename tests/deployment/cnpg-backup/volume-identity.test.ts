import assert from 'node:assert/strict';
import test from 'node:test';
import { attestStageVolume, stageIdentity, verifyStageBinding } from './volume-identity.ts';

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
