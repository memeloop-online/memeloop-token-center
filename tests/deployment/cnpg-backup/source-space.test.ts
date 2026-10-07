import assert from 'node:assert/strict';
import test from 'node:test';
import { exportPod, failureCategory, failureDetails, retryObservation, sourceBudget, sourceSpace } from './source-space.ts';
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

test('kubectl deadline and server timeout forms share bounded full-observation retry classification', async () => {
  for (const [stderr, category] of [
    ['Unable to connect to the server: context deadline exceeded', 'API_TIMEOUT'],
    ['error: context deadline exceeded', 'API_TIMEOUT'],
    ['Error from server (Timeout): the server was unable to return a response in the time allotted', 'API_TRANSIENT'],
    ['Error from server (ServerTimeout): the server cannot complete the operation at this time', 'API_TRANSIENT'],
  ]) {
    const failure = Object.assign(new Error('SENSITIVE_TIMEOUT_SENTINEL'), { stderr: Buffer.from(stderr!), status: 1 });
    assert.equal(failureCategory(failure), category);
    let attempts = 0;
    let publications = 0;
    const retries: number[] = [];
    await retryObservation(() => {
      attempts++;
      if (attempts === 1) throw failure;
      publications++;
    }, { pause: async () => {}, report: attempt => retries.push(attempt) });
    assert.equal(attempts, 2);
    assert.equal(publications, 1);
    assert.deepEqual(retries, [1]);
    attempts = 0;
    await assert.rejects(retryObservation(() => { attempts++; throw failure; }, { pause: async () => {} }), failure);
    assert.equal(attempts, 3);
    attempts = 0;
    let elapsed = 0;
    await assert.rejects(retryObservation(() => { attempts++; elapsed += 20_000; throw failure; }, { clock: () => elapsed, pause: async () => {} }), failure);
    assert.equal(attempts, 2);
  }
  assert.equal(sourceSpace.leaseSeconds, 45);
  assert.equal(sourceSpace.maximumDropBytes, 512 * 1024 ** 2);
});

test('safe failure diagnostics exclude raw stderr and never retry local guards or access failures', async () => {
  const sentinel = 'SENSITIVE_TIMEOUT_SENTINEL';
  const timeout = Object.assign(new Error(sentinel), { code: 'ETIMEDOUT', status: null, signal: 'SIGKILL', stderr: sentinel });
  assert.deepEqual(failureDetails(timeout), { category: 'API_TIMEOUT', exitStatus: null, processCode: 'ETIMEDOUT', signal: 'SIGKILL', hasStderr: true });
  assert.deepEqual(failureDetails(Object.assign(new Error(sentinel), { code: sentinel, signal: sentinel, stderr: sentinel, status: 1 })), { category: 'COLLECTOR_ERROR', exitStatus: 1, processCode: null, signal: null, hasStderr: true });
  assert.doesNotMatch(JSON.stringify(failureDetails(timeout)), new RegExp(sentinel));
  for (const [code, stderr, category] of [
    ['SOURCE_STAT_TIMEOUT', 'context deadline exceeded', 'SOURCE_STAT_TIMEOUT'],
    ['ERR_ASSERTION', 'context deadline exceeded', 'GUARD_ASSERTION'],
    ['', 'Error from server (Forbidden): denied', 'COLLECTOR_ERROR'],
    ['', 'Error from server (Unauthorized): denied', 'COLLECTOR_ERROR'],
    ['', 'Error from server (NotFound): claim missing', 'COLLECTOR_ERROR'],
    ['', 'x509: certificate signed by unknown authority', 'COLLECTOR_ERROR'],
  ]) {
    const failure = Object.assign(new Error(sentinel), { code, stderr });
    assert.equal(failureCategory(failure), category);
    let attempts = 0;
    await assert.rejects(retryObservation(() => { attempts++; throw failure; }, { pause: async () => {} }), failure);
    assert.equal(attempts, 1);
  }
});
