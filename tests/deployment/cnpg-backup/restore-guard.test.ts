import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { createHash } from 'node:crypto';
import { mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { setTimeout as delay } from 'node:timers/promises';
import test from 'node:test';
import { parseAllDocuments } from 'yaml';
import { archiveIdentity } from './volume-identity.ts';
import { boundedJobs, preparedResources } from './hard-capacity.ts';
import { prepareRestoreArtifact } from './prepare-restore-artifact.ts';
import { renewRestoreLease, restoreClaim, restoreSides, validateRestorePlan, validateRestorePod, type RestorePlan, type RestoreSide } from './restore-guard.ts';
import { verifyRestoreReceipt } from './restore.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true');
const directory = dirname(fileURLToPath(import.meta.url));

function fixture(): any {
  const job = preparedResources().find(resource => resource.kind === 'Job' && resource.metadata.name === boundedJobs.restore);
  const spec = structuredClone(job.spec.template.spec);
  spec.nodeName = archiveIdentity.node;
  const scratch = { ...archiveIdentity, name: restoreClaim, device: `/dev/longhorn/${restoreClaim}`, capacityGiB: 64,
    claimUID: '11111111-1111-1111-1111-111111111111', persistentUID: '22222222-2222-2222-2222-222222222222',
    longhornUID: '33333333-3333-3333-3333-333333333333', filesystemUUID: '44444444-4444-4444-4444-444444444444' };
  const plan: RestorePlan = { commandSHA256: createHash('sha256').update(JSON.stringify(spec.containers[0].command)).digest('hex'), archiveSHA256: 'a'.repeat(64), scratch };
  const overrides: Record<string, string> = { PARENT_REVIEW_APPROVED: 'true', HARD_CAPACITY_REVIEW_APPROVED: 'true', EXPECTED_SOURCE_SHA256: plan.archiveSHA256,
    EXPECTED_BACKUP_FS_UUID: archiveIdentity.filesystemUUID, EXPECTED_SCRATCH_FS_UUID: scratch.filesystemUUID, EXPECTED_SCRATCH_DEVICE: scratch.device };
  for (const entry of spec.containers[0].env) if (entry.name in overrides) entry.value = overrides[entry.name];
  const pod = { metadata: { namespace: archiveIdentity.namespace, name: `${job.metadata.name}-abcde`, uid: 'restore-pod-uid',
    labels: { ...job.spec.template.metadata.labels, 'job-name': job.metadata.name }, ownerReferences: [{ kind: 'Job', name: job.metadata.name, controller: true }] }, spec,
    status: { phase: 'Running', containerStatuses: [{ name: 'verify', ready: true, restartCount: 0, state: { running: {} }, containerID: 'containerd://' + 'b'.repeat(64) }] } };
  const volumes = Object.fromEntries([archiveIdentity, scratch].map(identity => [identity.name, {
    pvc: { metadata: { name: identity.name, uid: identity.claimUID, namespace: identity.namespace }, spec: { volumeName: identity.name }, status: { phase: 'Bound' } },
    pv: { metadata: { name: identity.name, uid: identity.persistentUID }, status: { phase: 'Bound' }, spec: { claimRef: { name: identity.name, namespace: identity.namespace, uid: identity.claimUID },
      volumeMode: 'Filesystem', persistentVolumeReclaimPolicy: 'Retain', capacity: { storage: `${identity.capacityGiB}Gi` }, csi: { driver: 'driver.longhorn.io', fsType: 'xfs', volumeHandle: identity.name } } },
    'volumes.longhorn.io': { metadata: { name: identity.name, uid: identity.longhornUID, namespace: 'longhorn-system' }, spec: { size: String(identity.capacityGiB * 1024 ** 3), numberOfReplicas: 1, dataLocality: 'strict-local' }, status: { state: 'attached', robustness: 'healthy', currentNodeID: identity.node } },
    'replicas.longhorn.io': { items: [{ metadata: {}, spec: { volumeName: identity.name, nodeID: identity.node, diskID: identity.diskUUID, diskPath: identity.diskPath, failedAt: '' }, status: { currentState: 'running' } }] },
    uuid: identity.filesystemUUID,
  }]));
  const receipt = { schema: 1, state: 'isolated-logical-restore-verified', archive_sha256: plan.archiveSHA256, archive_bytes: 1024, public_relations: 254,
    backup_fs_uuid: archiveIdentity.filesystemUUID, scratch_fs_uuid: scratch.filesystemUUID, verified_at: '2026-10-06T20:00:00Z',
    original_ownership_acl_verified: false, application_acceptance_verified: false, physical_wal_protection_verified: false };
  return { plan, pod, volumes, receipt,
    plugins: { items: [{ metadata: { name: 'reviewed-csi' }, spec: { nodeName: archiveIdentity.node }, status: { phase: 'Running', containerStatuses: [{ name: 'longhorn-csi-plugin', ready: true }] } }] } };
}

function reader(value: any, commands: string[][]): (args: string[]) => string {
  return args => {
    commands.push(args);
    if (args.includes('get')) {
      const index = args.indexOf('get');
      const kind = args[index + 1]!;
      if (kind === 'pod') return JSON.stringify(value.pod);
      if (kind === 'pods') return JSON.stringify(value.plugins);
      const name = kind === 'replicas.longhorn.io' ? args.find(entry => entry.startsWith('longhornvolume='))!.split('=')[1]! : args[index + 2]!;
      return JSON.stringify(value.volumes[name][kind]);
    }
    if (args.includes('blkid')) return `UUID=${value.volumes[args.at(-1)!.split('/').at(-1)!].uuid}\nTYPE=xfs\n`;
    if (args.includes('stat')) return args.at(-1)!.includes('scratch') ? '8:21' : '8:20';
    assert.ok(args.includes('verify') && args.at(-1)!.includes('-volume.lease.partial'));
    return '';
  };
}

test('restore CSI publisher pins each distinct 28Gi/64Gi volume without allocation or source access', () => {
  for (const side of restoreSides) {
    const value = fixture();
    const commands: string[][] = [];
    renewRestoreLease(reader(value, commands), side, value.pod.metadata.name, value.pod.metadata.uid, value.plan, () => 1_791_309_000_000);
    const writes = commands.filter(args => args.includes('/bin/sh'));
    assert.equal(writes.length, 1);
    assert.ok(writes[0]!.at(-1)!.includes(`/tmp/${side}-volume.lease.partial`));
    assert.ok(writes[0]!.at(-1)!.includes(side === 'backup' ? archiveIdentity.filesystemUUID : value.plan.scratch.filesystemUUID));
    assert.equal(commands.filter(args => args.includes('blkid')).length, 1);
    assert.ok(!commands.flat().some(arg => ['psql', 'pg_dump', 'apply', 'create', 'patch', 'delete'].includes(arg)));
  }
});

test('identity, command, capacity, replacement and credential mutations cannot publish a restore lease', () => {
  const mutations: Array<(value: any, side: RestoreSide) => void> = [
    value => { value.pod.metadata.uid = 'replacement'; },
    value => { value.pod.spec.containers[0].command = ['/bin/sleep', '43200']; },
    value => { value.pod.spec.containers[0].image = 'mutable:latest'; },
    value => { value.pod.status.containerStatuses[0].restartCount = 1; },
    value => { value.pod.spec.hostPID = true; },
    value => { value.pod.spec.volumes[0].persistentVolumeClaim.readOnly = false; },
    value => { value.pod.spec.containers[0].env.push({ name: 'DATABASE_URL', value: 'production' }); },
    value => { value.pod.spec.containers[0].env.push({ name: 'SECRET', valueFrom: { secretKeyRef: { name: 'production', key: 'token' } } }); },
    value => { value.pod.spec.containers[0].env.find((entry: any) => entry.name === 'SCRATCH_RESERVE_BYTES').value = '0'; },
    value => { value.plan.scratch.filesystemUUID = archiveIdentity.filesystemUUID; },
    (value, side) => { value.volumes[side === 'backup' ? archiveIdentity.name : restoreClaim].pvc.metadata.uid = 'replaced'; },
    (value, side) => { value.volumes[side === 'backup' ? archiveIdentity.name : restoreClaim].pv.spec.capacity.storage = '100Gi'; },
    (value, side) => { value.volumes[side === 'backup' ? archiveIdentity.name : restoreClaim]['volumes.longhorn.io'].status.currentNodeID = 'haixia'; },
    (value, side) => { value.volumes[side === 'backup' ? archiveIdentity.name : restoreClaim].uuid = 'wrong'; },
  ];
  for (const side of restoreSides) {
    for (const mutate of mutations) {
      const value = fixture();
      const uid = value.pod.metadata.uid;
      mutate(value, side);
      const commands: string[][] = [];
      assert.throws(() => renewRestoreLease(reader(value, commands), side, value.pod.metadata.name, uid, value.plan));
      assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 0);
    }
    for (const elapsed of [-1000, 46_000]) {
      const value = fixture();
      const commands: string[][] = [];
      let calls = 0;
      assert.throws(() => renewRestoreLease(reader(value, commands), side, value.pod.metadata.name, value.pod.metadata.uid, value.plan, () => 1_791_309_000_000 + (calls++ ? elapsed : 0)));
      assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 0);
    }
    const value = fixture();
    const commands: string[][] = [];
    const read = reader(value, commands);
    let podReads = 0;
    assert.throws(() => renewRestoreLease(args => {
      if (args.includes('get') && args.includes('pod') && ++podReads === 2) value.pod.status.containerStatuses[0].containerID = 'containerd://' + 'c'.repeat(64);
      return read(args);
    }, side, value.pod.metadata.name, value.pod.metadata.uid, value.plan));
    assert.equal(commands.filter(args => args.includes('/bin/sh')).length, 0);
  }
});

test('standalone restore bundle binds source hashes and never invents scratch identity or provisions storage', context => {
  const parent = mkdtempSync(join(tmpdir(), 'mtc-restore-artifact-'));
  context.after(() => rmSync(parent, { recursive: true, force: true }));
  const output = join(parent, 'bundle');
  prepareRestoreArtifact(output, { sourceHead: '1'.repeat(40), testedCommit: '2'.repeat(40), runId: '37524816649' });
  assert.equal(readdirSync(output).length, 8);
  const resources = parseAllDocuments(readFileSync(join(output, 'cnpg-restore-preparation.yaml'), 'utf8')).map(document => { assert.deepEqual(document.errors, []); return document.toJS({ maxAliasCount: 0 }); });
  assert.deepEqual(resources.map(resource => resource.kind).sort(), ['ConfigMap', 'Job', 'NetworkPolicy']);
  const job = resources.find(resource => resource.kind === 'Job');
  assert.equal(job.spec.suspend, true);
  assert.equal(job.spec.backoffLimit, 0);
  for (const field of ['PARENT_REVIEW_APPROVED', 'HARD_CAPACITY_REVIEW_APPROVED']) assert.equal(job.spec.template.spec.containers[0].env.find((entry: any) => entry.name === field).value, 'false');
  const plan = JSON.parse(readFileSync(join(output, 'review-required-plan.json'), 'utf8'));
  assert.throws(() => validateRestorePlan(plan));
  assert.equal(plan.commandSHA256, createHash('sha256').update(JSON.stringify(job.spec.template.spec.containers[0].command)).digest('hex'));
  const provenance = JSON.parse(readFileSync(join(output, 'provenance.json'), 'utf8'));
  assert.equal(provenance.executionAuthorized, false);
  assert.equal(provenance.storageAllocationAuthorized, false);
  for (const line of readFileSync(join(output, 'SHA256SUMS'), 'utf8').trim().split('\n')) {
    const [digest, name] = line.split('  ');
    assert.equal(createHash('sha256').update(readFileSync(join(output, name!))).digest('hex'), digest);
  }
  for (const name of ['restore.ts', 'restore-guard.ts', 'copy-guard.ts', 'volume-identity.ts']) {
    for (const imported of readFileSync(join(output, name), 'utf8').matchAll(/from '([^']+)'/g)) assert.ok(imported[1]!.startsWith('node:') || readdirSync(output).includes(imported[1]!.replace('./', '')));
  }
  assert.throws(() => prepareRestoreArtifact(output, { sourceHead: '1'.repeat(40), testedCommit: '2'.repeat(40), runId: '37524816649' }));
});

test('receipt validation never upgrades SHA/TOC or Pod readiness to real restore/application acceptance', () => {
  const value = fixture();
  assert.deepEqual(verifyRestoreReceipt(JSON.stringify(value.receipt), value.plan), value.receipt);
  for (const mutate of [
    (receipt: any) => { receipt.archive_sha256 = '0'.repeat(64); },
    (receipt: any) => { receipt.scratch_fs_uuid = archiveIdentity.filesystemUUID; },
    (receipt: any) => { receipt.public_relations = 253; },
    (receipt: any) => { receipt.archive_bytes = 0; },
    (receipt: any) => { receipt.application_acceptance_verified = true; },
  ]) {
    const receipt = structuredClone(value.receipt);
    mutate(receipt);
    assert.throws(() => verifyRestoreReceipt(JSON.stringify(receipt), value.plan));
  }
  assert.throws(() => verifyRestoreReceipt('SHA and TOC passed', value.plan));
  assert.throws(() => verifyRestoreReceipt(JSON.stringify(value.receipt) + '\n' + JSON.stringify(value.receipt), value.plan));
  assert.throws(() => validateRestorePod(value.pod, value.plan, value.pod.metadata.uid, true));
});

test('independent restore collectors handle owner loss and the controller accepts only terminal receipt evidence', { timeout: 60_000 }, async context => {
  const root = mkdtempSync(join(tmpdir(), 'mtc-restore-guard-'));
  const value = fixture();
  writeFileSync(join(root, 'inventory.json'), JSON.stringify(value));
  writeFileSync(join(root, 'plan.json'), JSON.stringify(value.plan));
  writeFileSync(join(root, 'kubectl'), `#!${process.execPath}\n` + String.raw`
import fs from 'node:fs';
const args = process.argv.slice(2);
const root = process.env.RESTORE_FIXTURE;
const data = JSON.parse(fs.readFileSync(root + '/inventory.json', 'utf8'));
if (args.includes('get')) {
  const index = args.indexOf('get');
  const kind = args[index + 1];
  if (kind === 'pod') {
    if (fs.existsSync(root + '/succeeded')) {
      data.pod.status.phase = 'Succeeded';
      data.pod.status.containerStatuses[0].ready = false;
      data.pod.status.containerStatuses[0].state = { terminated: { exitCode: 0 } };
    }
    process.stdout.write(JSON.stringify(data.pod));
  }
  else if (kind === 'pods') process.stdout.write(JSON.stringify(data.plugins));
  else {
    const name = kind === 'replicas.longhorn.io' ? args.find(entry => entry.startsWith('longhornvolume=')).split('=')[1] : args[index + 2];
    process.stdout.write(JSON.stringify(data.volumes[name][kind]));
  }
} else if (args.includes('logs')) process.stdout.write(JSON.stringify(data.receipt) + '\n');
else if (args.includes('blkid')) process.stdout.write('UUID=' + data.volumes[args.at(-1).split('/').at(-1)].uuid + '\nTYPE=xfs\n');
else if (args.includes('stat')) process.stdout.write(args.at(-1).includes('scratch') ? '8:21' : '8:20');
else if (args.includes('verify') && args.at(-1).includes('-volume.lease.partial')) fs.appendFileSync(root + (args.at(-1).includes('/tmp/scratch-') ? '/scratch-leases' : '/backup-leases'), 'renewed\n');
else process.exit(1);
`, { mode: 0o700 });
  const children = restoreSides.map(side => spawn(process.execPath, [join(directory, 'restore-guard.ts'), side, value.pod.metadata.name, value.pod.metadata.uid, join(root, 'plan.json')], {
    env: { ...process.env, PARENT_REVIEW_APPROVED: 'true', RESTORE_FIXTURE: root, PATH: `${root}:${process.env.PATH}` }, stdio: ['ignore', 'pipe', 'pipe', 'ipc'],
  }));
  const exits = children.map(child => new Promise<number | null>((resolve, reject) => { child.once('exit', resolve); child.once('error', reject); }));
  context.after(async () => { for (const child of children) if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL'); await Promise.all(exits); rmSync(root, { recursive: true, force: true }); });
  await Promise.all(children.map(child => new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('Missing first restore lease')), 8000);
    let output = '';
    child.stdout!.on('data', chunk => { output += chunk.toString(); if (output === 'RESTORE_LEASE_READY\n') { clearTimeout(timer); resolve(); } });
    child.once('exit', () => { clearTimeout(timer); reject(new Error('Restore guard exited early')); });
  })));
  await delay(17_000);
  for (const side of restoreSides) assert.ok(readFileSync(join(root, `${side}-leases`), 'utf8').trim().split('\n').length >= 2);
  for (const child of children) child.disconnect();
  assert.deepEqual(await Promise.race([Promise.all(exits), delay(5000).then(() => { throw new Error('Orphan restore guards'); })]), [1, 1]);
  for (const side of restoreSides) rmSync(join(root, `${side}-leases`));
  const controller = spawn(process.execPath, [join(directory, 'restore.ts'), value.pod.metadata.name, join(root, 'plan.json')], {
    env: { ...process.env, PARENT_REVIEW_APPROVED: 'true', RESTORE_FIXTURE: root, PATH: `${root}:${process.env.PATH}` }, stdio: ['ignore', 'pipe', 'pipe'],
  });
  let output = '';
  controller.stdout.on('data', chunk => { output += chunk.toString(); });
  const finished = new Promise<number | null>((resolve, reject) => { controller.once('exit', resolve); controller.once('error', reject); });
  context.after(async () => { if (controller.exitCode === null && controller.signalCode === null) controller.kill('SIGKILL'); await finished; });
  let ready = false;
  for (let attempt = 0; attempt < 100 && controller.exitCode === null; attempt++) {
    if (restoreSides.every(side => readdirSync(root).includes(`${side}-leases`))) { ready = true; break; }
    await delay(100);
  }
  assert.ok(ready, 'Controller must start both independent publishers');
  assert.equal(output, '', 'Running Pod and leases must not imply restore success');
  writeFileSync(join(root, 'succeeded'), 'synthetic terminal evidence');
  assert.equal(await Promise.race([finished, delay(10_000).then(() => { throw new Error('Controller did not finish'); })]), 0);
  assert.deepEqual(JSON.parse(output), value.receipt);
});
