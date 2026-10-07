import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { copyContainerCommand, copyIdentities, copySides, renewCopyLease, validateCopyPod, watchCopyLease, type CopySide } from './copy-guard.ts';
import { boundedJobs, preparedResources } from './hard-capacity.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const resources = preparedResources();
const directory = dirname(fileURLToPath(import.meta.url));

function fixture(side: CopySide): any {
  const expected = copyIdentities[side];
  const job = resources.find(resource => resource.kind === 'Job' && resource.metadata.name === boundedJobs[side]);
  const spec = structuredClone(job.spec.template.spec);
  spec.nodeName = expected.node;
  spec.containers[0].env.find((entry: any) => entry.name === 'EXPECTED_BACKUP_FS_UUID').value = expected.filesystemUUID;
  return {
    pod: {
      metadata: { namespace: expected.namespace, uid: `${side}-pod-uid`, name: `${job.metadata.name}-abcde`,
        labels: { ...job.spec.template.metadata.labels, 'job-name': job.metadata.name },
        ownerReferences: [{ kind: 'Job', name: job.metadata.name, controller: true }] },
      spec,
      status: { phase: 'Running', containerStatuses: [{ name: 'copy', ready: true, restartCount: 0, state: { running: {} }, containerID: 'containerd://' + 'a'.repeat(64) }] },
    },
    pvc: { metadata: { name: expected.name, uid: expected.claimUID, namespace: expected.namespace }, spec: { volumeName: expected.name }, status: { phase: 'Bound' } },
    pv: { metadata: { name: expected.name, uid: expected.persistentUID }, status: { phase: 'Bound' }, spec: {
      claimRef: { name: expected.name, namespace: expected.namespace, uid: expected.claimUID }, volumeMode: 'Filesystem', persistentVolumeReclaimPolicy: 'Retain',
      capacity: { storage: '28Gi' }, csi: { driver: 'driver.longhorn.io', fsType: 'xfs', volumeHandle: expected.name },
    } },
    'volumes.longhorn.io': { metadata: { name: expected.name, uid: expected.longhornUID, namespace: 'longhorn-system' },
      spec: { size: String(28 * 1024 ** 3), numberOfReplicas: 1, dataLocality: 'strict-local' }, status: { state: 'attached', robustness: 'healthy', currentNodeID: expected.node } },
    'replicas.longhorn.io': { items: [{ metadata: {}, spec: { volumeName: expected.name, nodeID: expected.node, diskID: expected.diskUUID, diskPath: expected.diskPath, failedAt: '' }, status: { currentState: 'running' } }] },
    pods: { items: [{ metadata: { name: 'reviewed-csi' }, spec: { nodeName: expected.node }, status: { phase: 'Running', containerStatuses: [{ name: 'longhorn-csi-plugin', ready: true }] } }] },
    filesystemUUID: expected.filesystemUUID,
  };
}

function reader(value: any, commands: string[][]): (args: string[]) => string {
  return args => {
    commands.push(args);
    if (args.includes('get')) return JSON.stringify(value[args[args.indexOf('get') + 1]!]);
    if (args.includes('blkid')) return `UUID=${value.filesystemUUID}\nTYPE=xfs\n`;
    if (args.includes('stat')) return '8:20';
    assert.ok(args.includes('copy') && args.at(-1)!.includes('/tmp/backup-volume.lease.partial'));
    return '';
  };
}

test('both copy sides use pinned distinct CSI identity and publish only a pod-bound tmpfs lease', () => {
  for (const side of copySides) {
    const value = fixture(side);
    const commands: string[][] = [];
    renewCopyLease(reader(value, commands), side, value.pod.metadata.name, value.pod.metadata.uid, () => 1_791_309_000_000);
    const writes = commands.filter(args => args.includes('/bin/sh'));
    assert.equal(writes.length, 1);
    assert.match(writes[0]!.at(-1)!, /1791309000/);
    assert.ok(writes[0]!.at(-1)!.includes(copyIdentities[side].device));
    assert.ok(writes[0]!.at(-1)!.includes(copyIdentities[side].filesystemUUID));
    assert.equal(commands.filter(args => args.includes('blkid')).length, 1);
    assert.ok(!commands.flat().some(arg => ['psql', 'pg_dump', 'apply', 'create', 'patch', 'delete'].includes(arg)));
    validateCopyPod(value.pod, side, value.pod.metadata.uid);
    assert.throws(() => validateCopyPod(value.pod, side === 'source' ? 'destination' : 'source'));
  }
});

test('Kubernetes may omit destination readOnly=false; source must remain explicitly read-only at both layers', () => {
  for (const layer of ['claim', 'mount', 'both']) {
    for (const side of copySides) {
      const value = fixture(side);
      const claim = value.pod.spec.volumes.find((volume: any) => volume.persistentVolumeClaim).persistentVolumeClaim;
      const mount = value.pod.spec.containers[0].volumeMounts.find((entry: any) => entry.name === 'backup');
      if (layer !== 'mount') delete claim.readOnly;
      if (layer !== 'claim') delete mount.readOnly;
      const commands: string[][] = [];
      const renew = () => renewCopyLease(reader(value, commands), side, value.pod.metadata.name, value.pod.metadata.uid, () => 1_791_309_000_000);
      if (side === 'source') {
        assert.throws(renew);
        assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 0);
      } else {
        renew();
        assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 1);
      }
    }
    const destination = fixture('destination');
    if (layer !== 'mount') destination.pod.spec.volumes.find((volume: any) => volume.persistentVolumeClaim).persistentVolumeClaim.readOnly = true;
    if (layer !== 'claim') destination.pod.spec.containers[0].volumeMounts.find((entry: any) => entry.name === 'backup').readOnly = true;
    assert.throws(() => validateCopyPod(destination.pod, 'destination'));
  }
});

for (const capacity of [32, 40]) test(`explicit${capacity}Gi copy source refuses unexpanded28Gi and leaves destination unchanged`, () => {
  const value = fixture('source');
  const environment = value.pod.spec.containers[0].env;
  environment.push({ name: 'REVIEWED_STAGE_CAPACITY_GIB', value: String(capacity) });
  for (const [name, selected] of [['BACKUP_MAX_BYTES', String(capacity * 1024 ** 3)], ['BACKUP_MIN_BYTES', String((capacity - 2) * 1024 ** 3)], ['HARD_CAPACITY_REVIEW_APPROVED', 'true']]) environment.find((entry: any) => entry.name === name).value = selected;
  const commands: string[][] = [];
  assert.throws(() => renewCopyLease(reader(value, commands), 'source', value.pod.metadata.name, value.pod.metadata.uid));
  assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 0);
  value.pv.spec.capacity.storage = `${capacity}Gi`;
  value['volumes.longhorn.io'].spec.size = String(capacity * 1024 ** 3);
  renewCopyLease(reader(value, commands), 'source', value.pod.metadata.name, value.pod.metadata.uid);
  assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 1);
  validateCopyPod(fixture('destination').pod, 'destination');
});

test('replaced identity, wrong mounts and stale observations cannot renew either copy lease', () => {
  const mutations: Array<(value: any) => void> = [
    value => { value.pvc.metadata.uid = 'replaced'; },
    value => { value.pv.spec.csi.volumeHandle = 'production-pgdata'; },
    value => { value['volumes.longhorn.io'].metadata.uid = 'replaced'; },
    value => { value['replicas.longhorn.io'].items[0].spec.diskID = 'wrong-disk'; },
    value => { value.filesystemUUID = 'wrong-filesystem'; },
    value => { value.pod.metadata.uid = 'replaced'; },
    value => { value.pod.spec.nodeName = 'wrong-node'; },
    value => { value.pod.spec.volumes[0].persistentVolumeClaim.claimName = 'production-pgdata'; },
    value => { value.pod.spec.volumes[0].persistentVolumeClaim.readOnly = !value.pod.spec.volumes[0].persistentVolumeClaim.readOnly; },
    value => { value.pod.spec.containers[0].image = 'mutable:latest'; },
    value => { value.pod.spec.containers[0].command = ['/bin/sleep', '43200']; },
    value => { value.pod.status.containerStatuses[0].restartCount = 1; },
    value => { value.pod.spec.containers[0].env.push({ name: 'CREDENTIAL', valueFrom: { secretKeyRef: { name: 'production', key: 'token' } } }); },
    value => { value.pod.spec.hostPID = true; },
  ];
  for (const side of copySides) {
    for (const mutate of mutations) {
      const value = fixture(side);
      const uid = value.pod.metadata.uid;
      mutate(value);
      const commands: string[][] = [];
      assert.throws(() => renewCopyLease(reader(value, commands), side, value.pod.metadata.name, uid));
      assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 0);
    }
    for (const elapsed of [-1000, 46_000]) {
      const value = fixture(side);
      const commands: string[][] = [];
      let clockReads = 0;
      assert.throws(() => renewCopyLease(reader(value, commands), side, value.pod.metadata.name, value.pod.metadata.uid, () => 1_791_309_000_000 + (clockReads++ === 0 ? 0 : elapsed)));
      assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 0);
    }
    const value = fixture(side);
    const commands: string[][] = [];
    const read = reader(value, commands);
    let podReads = 0;
    assert.throws(() => renewCopyLease(args => {
      if (args.includes('get') && args.includes('pod') && ++podReads === 2) value.pod.status.containerStatuses[0].containerID = 'containerd://' + 'b'.repeat(64);
      return read(args);
    }, side, value.pod.metadata.name, value.pod.metadata.uid));
    assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 0);
  }
});

test('renewal accounts for elapsed collection time and stops on the first collector failure', async () => {
  let now = 0;
  let calls = 0;
  let ready = 0;
  const sleeps: number[] = [];
  await assert.rejects(watchCopyLease(() => {
    calls++;
    if (calls === 3) throw new Error('collector failed');
    now += calls === 1 ? 25_000 : 12_000;
  }, () => { ready++; }, () => now, async milliseconds => { sleeps.push(milliseconds); now += milliseconds; }), /collector failed/);
  assert.deepEqual(sleeps, [0, 3000]);
  assert.equal(ready, 1);
  assert.equal(calls, 3);
});

test('independent collector renews while its controller blocks and exits on owner disconnect', { timeout: 35_000 }, async context => {
  const root = mkdtempSync(join(tmpdir(), 'mtc-copy-guard-'));
  const value = fixture('destination');
  writeFileSync(join(root, 'inventory.json'), JSON.stringify(value));
  writeFileSync(join(root, 'kubectl'), `#!${process.execPath}\n` + String.raw`
import fs from 'node:fs';
const args = process.argv.slice(2);
const root = process.env.COPY_FIXTURE;
const data = JSON.parse(fs.readFileSync(root + '/inventory.json', 'utf8'));
if (args.includes('get')) process.stdout.write(JSON.stringify(data[args[args.indexOf('get') + 1]]));
else if (args.includes('blkid')) process.stdout.write('UUID=' + data.filesystemUUID + '\nTYPE=xfs\n');
else if (args.includes('stat')) process.stdout.write('8:20');
else if (args.includes('copy') && args.at(-1).includes('/tmp/backup-volume.lease.partial')) fs.appendFileSync(root + '/leases', 'renewed\n');
else process.exit(1);
`, { mode: 0o700 });
  const child = spawn(process.execPath, [join(directory, 'copy-guard.ts'), 'destination', value.pod.metadata.name, value.pod.metadata.uid], {
    env: { ...process.env, PARENT_REVIEW_APPROVED: 'true', COPY_FIXTURE: root, PATH: `${root}:${process.env.PATH}` }, stdio: ['ignore', 'pipe', 'pipe', 'ipc'],
  });
  const outputStream = child.stdout;
  assert.ok(outputStream);
  const exited = new Promise<number | null>((resolve, reject) => { child.once('exit', resolve); child.once('error', reject); });
  context.after(async () => { if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL'); await exited; rmSync(root, { recursive: true, force: true }); });
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('No first copy lease')), 8000);
    let output = '';
    outputStream.on('data', chunk => { output += chunk.toString(); if (output === 'COPY_LEASES_READY\n') { clearTimeout(timer); resolve(); } });
    child.once('exit', () => { clearTimeout(timer); reject(new Error('Collector exited before readiness')); });
  });
  execFileSync('/bin/sleep', ['17'], { timeout: 20_000 });
  assert.ok(readFileSync(join(root, 'leases'), 'utf8').trim().split('\n').length >= 2, 'Parent blocking must not block CSI renewal');
  child.disconnect();
  assert.equal(await Promise.race([exited, delay(5000).then(() => { throw new Error('Orphan collector did not stop'); })]), 1);
  assert.match(copyContainerCommand, /capacity_backup \|\| exit 1/);
});
