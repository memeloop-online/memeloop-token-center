import assert from 'node:assert/strict';
import test from 'node:test';
import { runWatcher, type Config, type Runtime } from '../../ops/watch-mtc-vs-standby.ts';

const name = 'memeloop-token-center-pg-vs-standby';
const sourceUID = 'cbcf3e22-0173-4898-a81a-6fb3a4b06266';
const originalUID = '587427b2-ca1d-4497-8352-7e9897d205a7';
const config: Config = { mode: 'stop', context: 'mtc-argocd-release', operation: 'vs-standby-20261012', authorization: 'OWN_STANDBY_BOOTSTRAP_WATCH_STOP', receipt: '/var/lib/mwc/build-scratch/tmp/mtc-20261008/mock.jsonl', attemptFile: '/var/lib/mwc/build-scratch/tmp/mtc-20261008/mock-attempt.json' };
function fixture(options: { watch?: boolean; preflightFailure?: boolean; firstSourceFailure?: boolean; firstOwnReadFailure?: boolean; orphanChild?: boolean; unknownSlot?: boolean; changedApplication?: boolean; replacedSource?: boolean; replacedOwn?: boolean; unknownAck?: boolean; slow?: boolean } = {}) {
  let elapsed = 0;
  const wall = Date.parse('2026-10-12T00:00:00Z');
  let created = !options.watch;
  let deleted = false;
  let sqlReads = 0;
  let firstOwnReadFailed = false;
  const events: any[] = [];
  const calls: { args: string[]; input?: string; timeout: number }[] = [];
  const own = { metadata: { name, uid: 'own-cluster-uid', resourceVersion: '123', creationTimestamp: new Date(wall - 50000).toISOString(), annotations: { 'recovery.mtc/operation': config.operation } }, spec: { instances: 1, replica: { enabled: true, source: 'pg7-canonical' }, storage: { size: '40Gi', storageClass: 'mtc-cnpg-archive-versetensor-20261005' }, affinity: { nodeSelector: { 'kubernetes.io/hostname': 'versetensor-hv' } }, externalClusters: [{ connectionParameters: { application_name: 'mtc_vs_standby_20261012' } }], resources: { requests: { cpu: '1', memory: '2Gi' }, limits: { cpu: '1', memory: '2Gi' } } }, status: {} };
  let saved: any = { version: 1, operation: config.operation, sourceUID, originalClusterUID: originalUID, wallStarted: wall - 100000, armed: true, baselineWal: 1024, baselineFree: 16 * 1024 ** 3, baselineSlots: ['pg10'], baselineSenders: [], cluster: structuredClone(own), children: [], observedOwnPids: [], observedOwnSlots: [] };
  if (options.slow) saved.children = Array.from({ length: 12 }, (_, index) => ({ kind: 'jobs', name: `${name}-job-${index}`, uid: `own-job-${index}` }));
  if (options.orphanChild) saved.children = [{ kind: 'pods', name: `${name}-1`, uid: 'own-pod-uid' }];
  const runtime: Runtime = {
    now: () => elapsed, wall: () => wall + elapsed,
    pause: async milliseconds => { elapsed += milliseconds; },
    read: () => structuredClone(saved), write: (_path, value) => { saved = structuredClone(value); },
    record: (_path, value) => { events.push(value); }, interrupted: () => false,
    emit: message => { if (message.startsWith('ARMED:')) { created = true; own.metadata.creationTimestamp = new Date(wall + elapsed).toISOString(); } },
    command: (args, input, timeout) => {
      calls.push({ args, input, timeout });
      assert.ok(timeout > 0 && timeout <= 5000);
      if (options.slow) { elapsed += timeout; throw Error('MOCK_TIMEOUT'); }
      if (args.includes('delete')) {
        assert.ok(args.find(argument => argument.startsWith('--raw='))?.includes(`/namespaces/memeloop-token-center/`));
        const body = JSON.parse(input!);
        assert.ok(['own-cluster-uid', 'own-pod-uid'].includes(body.preconditions.uid));
        if (body.preconditions.uid === 'own-pod-uid') { assert.ok(args.some(argument => argument.endsWith(`/pods/${name}-1`))); return '{}'; }
        assert.ok(args.some(argument => argument.endsWith(`/clusters/${name}`)));
        assert.equal(body.propagationPolicy, 'Foreground');
        if (options.replacedOwn) throw Error('UID_PRECONDITION_CONFLICT');
        deleted = true; created = false;
        if (options.unknownAck) throw Error('ACK_LOST_AFTER_ACCEPTANCE');
        return '{}';
      }
      const getIndex = args.indexOf('get');
      if (getIndex >= 0) {
        const kind = args[getIndex + 1];
        const resource = args[getIndex + 2];
        if (kind === 'cluster' && resource === name) {
          if (!created) return '';
          if (options.firstOwnReadFailure && !firstOwnReadFailed) { firstOwnReadFailed = true; throw Error('OWN_FIRST_READ_FAILED'); }
          const current = structuredClone(own);
          if (options.replacedOwn) current.metadata.uid = 'someone-else-uid';
          return JSON.stringify(current);
        }
        if (kind === 'cluster' && resource === 'memeloop-token-center-pg') return JSON.stringify({ metadata: { uid: originalUID }, status: { currentPrimary: 'memeloop-token-center-pg-7' } });
        if (kind === 'pod' && resource === 'memeloop-token-center-pg-7') {
          if (options.preflightFailure || options.firstSourceFailure && created) throw Error('SOURCE_GET_FAILED');
          return JSON.stringify({ metadata: { uid: options.replacedSource ? 'replacement-source-uid' : sourceUID }, status: { conditions: [{ type: 'Ready', status: 'True' }] } });
        }
        if (kind === 'application') return JSON.stringify({ spec: { syncPolicy: {} } });
        if (kind === 'pods' && resource === `${name}-1`) return JSON.stringify({ metadata: { uid: 'own-pod-uid' } });
        if (['jobs', 'pods', 'pods,jobs'].includes(kind)) return JSON.stringify({ items: [] });
        assert.fail(`UNEXPECTED_GET:${kind}:${resource}`);
      }
      if (args.includes('psql')) {
        sqlReads++;
        assert.ok(input?.startsWith('BEGIN READ ONLY; SET LOCAL ROLE pg_monitor;'));
        assert.doesNotMatch(input!, /pg_terminate_backend|pg_drop_replication_slot|ALTER\s|DROP\s/);
        return JSON.stringify({ identity: 'pg_monitor', readOnly: 'on', superuser: false, recovery: false, wal: 1024, slots: [{ name: 'pg10', activePid: null }, ...(options.unknownSlot ? [{ name: 'unregistered-bootstrap-slot', activePid: 8123, temporary: true }] : [])], senders: options.changedApplication ? [{ pid: 8123, started: '2026-10-12T00:00:01Z', application: 'different-real-application', client: '100.64.0.8', port: 49000 }] : [] });
      }
      if (args.includes('df')) return 'Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/source 40000000 10000000 16777216 25% /var/lib/postgresql/data\n';
      assert.fail('UNEXPECTED_COMMAND_NO_REAL_KUBECTL_FALLBACK');
    },
  };
  return { runtime, events, calls, get saved() { return saved; }, get elapsed() { return elapsed; }, get sqlReads() { return sqlReads; }, config: { ...config, mode: options.watch ? 'watch' : 'stop' } as Config };
}
function assertScopedDeletes(mock: ReturnType<typeof fixture>) {
  for (const call of mock.calls.filter(call => call.args.includes('delete'))) {
    const uid = JSON.parse(call.input!).preconditions.uid;
    assert.ok(['own-cluster-uid', 'own-pod-uid'].includes(uid));
    assert.ok(call.args.some(argument => argument.endsWith(uid === 'own-cluster-uid' ? `/clusters/${name}` : `/pods/${name}-1`)));
  }
  assert.ok(mock.calls.every(call => !call.args.some(argument => /pg_terminate_backend|pg_drop_replication_slot|\/persistentvolumeclaims\/|\/memeloop-token-center-pg-10\b/.test(argument))));
}

test('R2-1 ARMED creation then first source GET failure recovers own UID and deletes, never exits success', async () => {
  const mock = fixture({ watch: true, firstSourceFailure: true });
  const code = await runWatcher(mock.config, mock.runtime);
  assert.ok(mock.events.some(event => event.event === 'ARMED_BEFORE_EXTERNAL_EXACT_RESOURCE_SYNC'));
  assert.equal(mock.saved.cluster.metadata.uid, 'own-cluster-uid');
  assert.ok(mock.calls.some(call => call.args.includes('delete')));
  assert.notEqual(code, 0);
  assertScopedDeletes(mock);
});
test('R2-2 restart STOP restores old attempt time and UID, does not reject existing own Cluster', async () => {
  const mock = fixture();
  assert.equal(await runWatcher(mock.config, mock.runtime), 3);
  assert.ok(mock.events.some(event => event.event === 'OWN_STOP_CONFIRMED'));
  assertScopedDeletes(mock);
});
test('R2-1 failure before first own UID binding uses independent STOP lookup', async () => {
  const mock = fixture({ watch: true, firstOwnReadFailure: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 3);
  assert.equal(mock.saved.cluster.metadata.uid, 'own-cluster-uid');
  assertScopedDeletes(mock);
});
test('R2-3 lost labels do not hide a saved live own Pod UID', async () => {
  const mock = fixture({ orphanChild: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 2);
  assert.ok(!mock.events.some(event => event.event === 'OWN_STOP_CONFIRMED'));
  assertScopedDeletes(mock);
});
test('R2-3 unregistered new slot cannot become cleanup PASS', async () => {
  const mock = fixture({ unknownSlot: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 2);
  assert.ok(!mock.events.some(event => event.event === 'OWN_STOP_CONFIRMED'));
  assertScopedDeletes(mock);
});
test('R2-3 different application name on a new sender cannot become cleanup PASS', async () => {
  const mock = fixture({ changedApplication: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 2);
  assert.ok(!mock.events.some(event => event.event === 'OWN_STOP_CONFIRMED'));
  assertScopedDeletes(mock);
});
test('R2-3 source replacement prevents SQL and cleanup confirmation', async () => {
  const mock = fixture({ replacedSource: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 2);
  assert.equal(mock.sqlReads, 0);
  assert.ok(!mock.events.some(event => event.event === 'OWN_STOP_CONFIRMED'));
  assertScopedDeletes(mock);
});
test('UID replacement can only attempt the old trusted UID; no foreign delete', async () => {
  const mock = fixture({ replacedOwn: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 2);
  assertScopedDeletes(mock);
});
test('unknown DELETE ACK is resolved by reads, never another DELETE', async () => {
  const mock = fixture({ unknownAck: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 3);
  assert.equal(mock.calls.filter(call => call.args.includes('delete')).length, 1);
});
test('slow API and many saved children share one total 30-second STOP budget', async () => {
  const mock = fixture({ slow: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 2);
  assert.ok(mock.elapsed <= 30000);
  assert.ok(!mock.events.some(event => event.event === 'OWN_STOP_CONFIRMED'));
});
test('preflight failure never arms, never deletes and exits nonzero', async () => {
  const mock = fixture({ watch: true, preflightFailure: true });
  assert.equal(await runWatcher(mock.config, mock.runtime), 1);
  assert.ok(!mock.events.some(event => event.event === 'ARMED_BEFORE_EXTERNAL_EXACT_RESOURCE_SYNC'));
  assert.equal(mock.calls.filter(call => call.args.includes('delete')).length, 0);
});
