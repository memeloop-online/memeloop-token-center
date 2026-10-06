import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test, { type TestContext } from 'node:test';
import { fileURLToPath } from 'node:url';
import { sourceSpace } from './source-space.ts';
import { stageIdentity } from './volume-identity.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const collector = fileURLToPath(new URL('./source-space.ts', import.meta.url));
const fixture = fileURLToPath(new URL('./collector-api-fixture.ts', import.meta.url));
const sentinel = 'SENSITIVE_API_BODY_SENTINEL';
const exportName = `${sourceSpace.job}-fixture`;
const exportPod = {
  metadata: { namespace: sourceSpace.namespace, uid: 'fixture-export', labels: { 'job-name': sourceSpace.job }, ownerReferences: [{ kind: 'Job', name: sourceSpace.job }] },
  spec: { nodeName: sourceSpace.node, volumes: [{ persistentVolumeClaim: { claimName: sourceSpace.stageClaim } }], containers: [{ name: 'export', env: [
    { name: 'EXPECTED_BACKUP_FS_UUID', value: stageIdentity.filesystemUUID },
    { name: 'EXPECTED_BACKUP_DEVICE', value: stageIdentity.device },
    { name: 'BACKUP_UUID_ATTESTATION', value: 'external-csi-lease' },
    { name: 'POD_UID', valueFrom: { fieldRef: { fieldPath: 'metadata.uid' } } },
  ] }] },
  status: { phase: 'Running' },
};
type Step = { contains: string[]; reply?: any; delayMs?: number; sample?: { mode?: string; ageMs?: number }; stderr?: string; exitCode?: number };
const sourceCalls = (sample: Step['sample'] = {}, delayMs = 0): Step[] => [
  { contains: ['get', 'clusters.postgresql.cnpg.io'], reply: { status: { currentPrimary: sourceSpace.pod } } },
  { contains: ['get', 'pod', sourceSpace.pod], reply: { metadata: { uid: 'fixture-source' }, spec: { nodeName: sourceSpace.node }, status: { containerStatuses: [{ name: 'postgres', ready: true }] } } },
  { contains: ['get', '--raw'], delayMs, sample, reply: { node: { nodeName: sourceSpace.node }, pods: [{ podRef: { name: sourceSpace.pod, namespace: sourceSpace.namespace, uid: 'fixture-source' }, volume: [{ name: 'pgdata', pvcRef: { name: sourceSpace.claim, namespace: sourceSpace.namespace }, availableBytes: 10 * 1024 ** 3, capacityBytes: 30 * 1024 ** 3, inodesFree: 100_000 }] }] } },
];
const exportCall = (): Step => ({ contains: ['get', 'pod', exportName], reply: exportPod });
const volumeCalls = (): Step[] => [
  { contains: ['get', 'pvc'], reply: { metadata: { name: stageIdentity.name, uid: stageIdentity.claimUID, namespace: stageIdentity.namespace }, spec: { volumeName: stageIdentity.name }, status: { phase: 'Bound' } } },
  { contains: ['get', 'pv'], reply: { metadata: { name: stageIdentity.name, uid: stageIdentity.persistentUID }, status: { phase: 'Bound' }, spec: { claimRef: { name: stageIdentity.name, namespace: stageIdentity.namespace, uid: stageIdentity.claimUID }, volumeMode: 'Filesystem', persistentVolumeReclaimPolicy: 'Retain', capacity: { storage: '28Gi' }, csi: { driver: 'driver.longhorn.io', fsType: 'xfs', volumeHandle: stageIdentity.name } } } },
  { contains: ['get', 'volumes.longhorn.io'], reply: { metadata: { name: stageIdentity.name, uid: stageIdentity.longhornUID, namespace: 'longhorn-system' }, spec: { size: String(28 * 1024 ** 3), numberOfReplicas: 1, dataLocality: 'strict-local' }, status: { state: 'attached', robustness: 'healthy', currentNodeID: stageIdentity.node } } },
  { contains: ['get', 'replicas.longhorn.io'], reply: { items: [{ metadata: {}, spec: { volumeName: stageIdentity.name, nodeID: stageIdentity.node, diskID: stageIdentity.diskUUID, diskPath: stageIdentity.diskPath, failedAt: '' }, status: { currentState: 'running' } }] } },
  { contains: ['get', 'pods'], reply: { items: [{ metadata: { name: 'fixture-csi' }, spec: { nodeName: stageIdentity.node }, status: { phase: 'Running', containerStatuses: [{ name: 'longhorn-csi-plugin', ready: true }] } }] } },
  { contains: ['exec', 'fixture-csi', 'blkid', stageIdentity.device], reply: `UUID=${stageIdentity.filesystemUUID}\nTYPE=xfs\n` },
  { contains: ['exec', 'fixture-csi', 'stat', stageIdentity.device], reply: '8:20\n' },
];
const revokeCalls = (): Step[] => [exportCall(), { contains: ['exec', exportName, '/bin/rm', '/tmp/source-space.lease', '/tmp/backup-volume.lease'], reply: '' }];

async function run(context: TestContext, plan: Step[], watch = false) {
  const directory = mkdtempSync(join(tmpdir(), 'cnpg-collector-'));
  context.after(() => rmSync(directory, { recursive: true, force: true }));
  writeFileSync(join(directory, 'plan.json'), JSON.stringify(plan));
  writeFileSync(join(directory, 'state.json'), JSON.stringify({ index: 0 }));
  writeFileSync(join(directory, 'kubectl'), `#!/bin/sh\nexec '${process.execPath}' '${fixture}' "$@"\n`, { mode: 0o700 });
  const child = spawn(process.execPath, [collector, ...(watch ? ['--watch', exportName] : ['--check'])], {
    detached: true, stdio: ['ignore', 'pipe', 'pipe'],
    env: { ...process.env, PATH: `${directory}:${process.env.PATH}`, KUBECONFIG: join(directory, 'no-cluster-credentials'), COLLECTOR_FIXTURE_DIRECTORY: directory, PARENT_REVIEW_APPROVED: 'true' },
  });
  const stop = () => { try { if (child.pid) process.kill(-child.pid, 'SIGKILL'); } catch { } };
  context.after(stop);
  const deadline = setTimeout(stop, 110_000);
  let stdout = '';
  let stderr = '';
  child.stdout!.on('data', chunk => { stdout += chunk; });
  child.stderr!.on('data', chunk => { stderr += chunk; });
  let status: number | null;
  try {
    status = await new Promise<number | null>((resolve, reject) => { child.on('error', reject); child.on('close', resolve); });
  } finally { clearTimeout(deadline); }
  assert.doesNotMatch(stdout + stderr, new RegExp(sentinel));
  const events = stderr.trim().split('\n').filter(Boolean).map(line => JSON.parse(line));
  assert.equal(JSON.parse(readFileSync(join(directory, 'state.json'), 'utf8')).index, plan.length, stderr);
  for (const ended of events.filter(event => event.event === 'api-end' || event.event === 'collection-end')) {
    const started = events.find(event => event.event === ended.event.replace('-end', '-start') && event.cycle === ended.cycle && event.attempt === ended.attempt && event.call === ended.call);
    assert.ok(started, JSON.stringify(ended));
    assert.ok(ended.durationMs >= 0 && ended.monotonicMs >= started.monotonicMs);
    assert.ok(Number.isFinite(Date.parse(started.observedAt)) && Number.isFinite(Date.parse(ended.observedAt)));
  }
  context.diagnostic(JSON.stringify({ status, observations: events.filter(event => ['source-sample', 'lease-publication-start', 'lease-publication-end', 'transient-api-retry-no-lease-renewal', 'collector-failed'].includes(event.event)), delayedCalls: events.filter(event => event.event === 'api-end' && event.durationMs >= 200) }));
  return { status, events, stdout };
}

test('real collector subprocess separates invalid, future and stale samples without API body leakage', { timeout: 120_000 }, async context => {
  for (const [sample, category] of [
    [{ mode: 'invalid' }, 'SOURCE_SAMPLE_INVALID'],
    [{ ageMs: -60_000 }, 'SOURCE_SAMPLE_FUTURE'],
    [{ ageMs: 91_000 }, 'SOURCE_SAMPLE_STALE'],
  ] as const) {
    await context.test(category, async childContext => {
      const result = await run(childContext, sourceCalls(sample, 250));
      assert.equal(result.status, 1);
      const observed = result.events.find(event => event.event === 'source-sample');
      assert.equal(observed.category, category);
      if (category === 'SOURCE_SAMPLE_INVALID') assert.equal(observed.sourceSampleAt, null);
      assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
      assert.ok(result.events.some(event => event.phase === 'source-summary' && event.durationMs >= 250));
    });
  }
});

test('real delayed summary crosses the unchanged 90-second threshold before publication', { timeout: 120_000 }, async context => {
  const result = await run(context, [...sourceCalls(), exportCall(), ...volumeCalls(), ...sourceCalls({ ageMs: 89_000 }, 1600), ...revokeCalls()], true);
  assert.equal(result.status, 1);
  const observed = result.events.filter(event => event.event === 'source-sample').at(-1);
  assert.equal(observed.category, 'SOURCE_SAMPLE_STALE');
  assert.ok(observed.sampleAgeMs >= 90_600);
  assert.ok(result.events.some(event => event.phase === 'source-summary' && event.durationMs >= 1600));
  assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
  assert.ok(result.events.some(event => event.phase === 'lease-revoke' && event.outcome === 'success'));
});

test('cached source sample, serial CSI latency and lease publication have distinct measured timestamps', { timeout: 120_000 }, async context => {
  const firstVolume = volumeCalls();
  firstVolume[0]!.delayMs = 350;
  firstVolume[5]!.delayMs = 350;
  const result = await run(context, [
    ...sourceCalls({ ageMs: 75_000 }), exportCall(), ...firstVolume, ...sourceCalls({ mode: 'cached' }),
    { contains: ['exec', exportName, '/bin/sh'], reply: '', delayMs: 350 },
    exportCall(), ...volumeCalls(), ...sourceCalls({ mode: 'cached' }), ...revokeCalls(),
  ], true);
  assert.equal(result.status, 1);
  const samples = result.events.filter(event => event.event === 'source-sample');
  assert.equal(samples.length, 3);
  assert.equal(new Set(samples.map(event => event.sourceSampleAt)).size, 1);
  assert.equal(samples[2].category, 'SOURCE_SAMPLE_STALE');
  assert.ok(samples[2].sampleAgeMs > 90_000);
  const published = result.events.filter(event => event.event === 'lease-publication-end');
  assert.equal(published.length, 1);
  assert.equal(published[0].leaseSeconds, 45);
  assert.equal(published[0].sourceSampleAt, samples[0].sourceSampleAt);
  assert.ok(Number.isSafeInteger(published[0].sourceLeaseEpoch));
  assert.ok(Number.isSafeInteger(published[0].volumeLeaseEpoch));
  const publishing = result.events.find(event => event.event === 'lease-publication-start');
  assert.ok(published[0].sampleAgeMs >= publishing.sampleAgeMs + 350);
  for (const phase of ['volume-claim', 'volume-filesystem-identity', 'lease-publish']) assert.ok(result.events.some(event => event.phase === phase && event.durationMs >= 350));
  assert.ok(result.events.some(event => event.phase === 'lease-revoke' && event.outcome === 'success'));
});

test('real timed-out API subprocess is bounded, redacted and retried with a complete fresh collection', { timeout: 120_000 }, async context => {
  const result = await run(context, [
    { contains: ['get', 'clusters.postgresql.cnpg.io'], delayMs: 60_000, reply: sentinel },
    ...sourceCalls({}, 250),
  ]);
  assert.equal(result.status, 0);
  assert.equal(JSON.parse(result.stdout).sourceSpace.leaseSeconds, 45);
  const timeout = result.events.find(event => event.category === 'API_TIMEOUT' && event.event === 'api-end');
  assert.ok(timeout.durationMs >= 20_000 && timeout.durationMs < 30_000);
  assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 1);
  assert.equal(result.events.filter(event => event.event === 'collection-start').length, 2);
});

test('three transient subprocess failures never publish or leak stderr; malformed JSON is not retried', { timeout: 120_000 }, async context => {
  const failure: Step = { contains: ['get', 'clusters.postgresql.cnpg.io'], delayMs: 250, stderr: `Client.Timeout ${sentinel}`, reply: sentinel, exitCode: 1 };
  const result = await run(context, [failure, failure, failure]);
  assert.equal(result.status, 1);
  assert.equal(result.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 2);
  assert.equal(result.events.filter(event => event.event === 'lease-publication-start').length, 0);
  const malformed = await run(context, [{ contains: ['get', 'clusters.postgresql.cnpg.io'], reply: sentinel }]);
  assert.equal(malformed.status, 1);
  assert.equal(malformed.events.at(-1).category, 'API_INVALID_JSON');
  assert.equal(malformed.events.filter(event => event.event === 'transient-api-retry-no-lease-renewal').length, 0);
});
