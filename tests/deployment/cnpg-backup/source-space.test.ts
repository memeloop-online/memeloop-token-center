import assert from 'node:assert/strict';
import test from 'node:test';
import { exportPod, retryObservation, sourceBudget, sourceSpace } from './source-space.ts';
import { parseSourceStat } from './source-filesystem.ts';
import { sourceStatOutput } from './source-filesystem.fixture.ts';

assert.equal(process.env.GITHUB_ACTIONS, 'true', 'Run automated backup contracts only in GitHub Actions');

test('source reserve and cumulative loss stop the backup before the source filesystem fills', () => {
  const sample = parseSourceStat(sourceStatOutput());
  sourceBudget(sample);
  assert.throws(() => sourceBudget({ availableBytes: sourceSpace.startBytes - 1 }));
  assert.throws(() => sourceBudget({ availableBytes: sourceSpace.stopBytes - 1 }, sample.availableBytes));
  assert.throws(() => sourceBudget({ availableBytes: sample.availableBytes - sourceSpace.maximumDropBytes }, sample.availableBytes));
  sourceBudget({ availableBytes: sample.availableBytes - sourceSpace.maximumDropBytes + 1 }, sample.availableBytes);
});

test('budget diagnostics precede rejection and contain only bounded capacity fields', () => {
  const initial = 10 * 1024 ** 3;
  for (const [availableBytes, baseline, category, fails] of [
    [sourceSpace.startBytes - 1, undefined, 'SOURCE_BUDGET_WATERMARK', true],
    [sourceSpace.stopBytes - 1, initial, 'SOURCE_BUDGET_WATERMARK', true],
    [initial - sourceSpace.maximumDropBytes, initial, 'SOURCE_BUDGET_DROP', true],
    [initial - sourceSpace.maximumDropBytes + 1, initial, 'SOURCE_BUDGET_OK', false],
    [sourceSpace.startBytes, undefined, 'SOURCE_BUDGET_OK', false],
  ] as const) {
    const reports: Record<string, unknown>[] = [];
    const sample = { availableBytes, credential: 'SENSITIVE_BUDGET_SENTINEL', raw: 'SENSITIVE_BUDGET_SENTINEL' };
    const operation = () => sourceBudget(sample, baseline, fields => reports.push(fields));
    if (fails) assert.throws(operation); else operation();
    assert.deepEqual(reports, [{
      event: 'source-budget', phase: baseline === undefined ? 'initial' : 'renewal',
      availableBytes, initialAvailableBytes: baseline ?? null,
      minimumAvailableBytes: baseline === undefined ? sourceSpace.startBytes : sourceSpace.stopBytes,
      maximumDropBytes: sourceSpace.maximumDropBytes,
      dropBytes: baseline === undefined ? null : baseline - availableBytes,
      category,
    }]);
    assert.doesNotMatch(JSON.stringify(reports), /SENSITIVE_BUDGET_SENTINEL/);
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

test('one transient API timeout retries a complete observation without issuing a stale lease', async () => {
  let attempts = 0;
  let leases = 0;
  const retries: number[] = [];
  const result = await retryObservation(() => {
    attempts++;
    if (attempts === 1) throw Object.assign(new Error('API timeout'), { stderr: 'Client.Timeout exceeded while awaiting headers' });
    leases++;
    return 'fresh';
  }, { pause: async () => {}, report: attempt => retries.push(attempt) });
  assert.equal(result, 'fresh');
  assert.equal(attempts, 2);
  assert.equal(leases, 1);
  assert.deepEqual(retries, [1]);
});

test('persistent API failure is bounded and identity/budget assertions are never retried', async () => {
  let attempts = 0;
  let clock = 0;
  const failure = Object.assign(new Error('API timeout'), { code: 'ETIMEDOUT' });
  await assert.rejects(retryObservation(() => { attempts++; throw failure; }, { pause: async () => {} }), failure);
  assert.equal(attempts, 3);
  attempts = 0;
  await assert.rejects(retryObservation(() => { attempts++; clock += 20_000; throw failure; }, { clock: () => clock, pause: async () => {} }), failure);
  assert.equal(attempts, 2);
  attempts = 0;
  await assert.rejects(retryObservation(() => { attempts++; sourceBudget({ availableBytes: 0 }); }, { pause: async () => {} }), /watermark/);
  assert.equal(attempts, 1);
});
