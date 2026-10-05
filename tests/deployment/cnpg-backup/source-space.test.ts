import assert from 'node:assert/strict';
import test from 'node:test';
import { exportPod, sourceBudget, sourceSample, sourceSpace } from './source-space.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');
const now = Date.parse('2026-10-05T18:09:17Z');
const summary = () => ({ node: { nodeName: sourceSpace.node }, pods: [{ podRef: { namespace: sourceSpace.namespace, name: sourceSpace.pod }, volume: [{ name: 'pgdata', pvcRef: { name: sourceSpace.claim, namespace: sourceSpace.namespace }, time: new Date(now).toISOString(), availableBytes: 10 * 1024 ** 3, capacityBytes: 30 * 1024 ** 3, inodesFree: 100_000 }] }] });

test('source reserve and cumulative loss stop the backup before the source filesystem fills', () => {
  const sample = sourceSample(summary(), now);
  sourceBudget(sample);
  assert.throws(() => sourceBudget({ availableBytes: sourceSpace.startBytes - 1 }));
  assert.throws(() => sourceBudget({ availableBytes: sourceSpace.stopBytes - 1 }, sample.availableBytes));
  assert.throws(() => sourceBudget({ availableBytes: sample.availableBytes - sourceSpace.maximumDropBytes }, sample.availableBytes));
  sourceBudget({ availableBytes: sample.availableBytes - sourceSpace.maximumDropBytes + 1 }, sample.availableBytes);
  for (const mutate of [
    (value: any) => { value.node.nodeName = 'other'; },
    (value: any) => { value.pods[0].volume[0].pvcRef.name = 'wrong'; },
    (value: any) => { value.pods[0].volume[0].time = new Date(now - 90_001).toISOString(); },
    (value: any) => { value.pods[0].volume[0].availableBytes = null; },
    (value: any) => { value.pods[0].volume[0].inodesFree = 0; },
  ]) {
    const value = summary();
    mutate(value);
    assert.throws(() => sourceSample(value, now));
  }
});

test('watcher can target only its export pod, never a primary or source PVC', () => {
  const pod = {
    metadata: { namespace: sourceSpace.namespace, labels: { 'job-name': sourceSpace.job }, ownerReferences: [{ kind: 'Job', name: sourceSpace.job }] },
    spec: { nodeName: sourceSpace.node, volumes: [{ persistentVolumeClaim: { claimName: sourceSpace.stageClaim } }], containers: [{ name: 'export' }] },
  };
  exportPod(pod);
  const changed = structuredClone(pod);
  changed.spec.volumes[0]!.persistentVolumeClaim.claimName = sourceSpace.claim;
  assert.throws(() => exportPod(changed));
  changed.metadata.labels['job-name'] = sourceSpace.cluster;
  assert.throws(() => exportPod(changed));
});
