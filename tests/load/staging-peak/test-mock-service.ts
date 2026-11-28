import assert from 'node:assert/strict';
import test from 'node:test';
import { serviceAction } from './staging-mock-service.ts';

const meta = { context: 'fixture', clusterUid: 'cluster', namespaceUid: 'namespace' };
const uid = '12345678-1234-1234-1234-123456789abc';
function fixture(options: { cluster?: string; namespace?: string; serviceUid?: string; run?: string; status?: number; lost?: boolean; proxyFailure?: boolean } = {}) {
  const calls: { args: string[]; input?: string }[] = [];
  let deletes = 0, stopped = 0, proxies = 0;
  const deps = {
    kubectl(args: string[], input?: string) {
      calls.push({ args, input });
      if (args.includes('create')) return { metadata: { uid } };
      if (args.includes('kube-system')) return { metadata: { uid: options.cluster ?? 'cluster' } };
      if (args.includes('service')) return { metadata: { uid: options.serviceUid ?? uid, labels: { 'mtc.memeloop.ai/peak-run': options.run ?? 'fixture' } } };
      return { metadata: { uid: options.namespace ?? 'namespace' } };
    },
    async proxy(context: string) {
      assert.equal(context, 'fixture'); proxies++;
      if (options.proxyFailure) throw new Error('proxy exited before readiness');
      return { url: 'http://127.0.0.1:12345', stop: async () => { stopped++; } };
    },
    async remove(url: string, body: string) {
      deletes++;
      assert.equal(url, 'http://127.0.0.1:12345/api/v1/namespaces/memeloop-token-center-staging/services/mtc-peak-fixture-mock');
      assert.deepEqual(JSON.parse(body), { apiVersion: 'v1', kind: 'DeleteOptions', preconditions: { uid } });
      if (options.lost) throw new Error('response lost');
      return options.status ?? 202;
    },
  };
  return { deps, calls, counts: () => ({ deletes, stopped, proxies }) };
}

test('create preserves exact Service and stdout contract without apply/proxy', async () => {
  const f = fixture();
  assert.deepEqual(await serviceAction('create', meta, 'fixture', undefined, f.deps), { name: 'mtc-peak-fixture-mock', uid, namespaceUid: 'namespace', runId: 'fixture' });
  assert.equal(f.calls.length, 3);
  const request = JSON.parse(f.calls[2].input!);
  assert.deepEqual(request.spec, { selector: { 'app.kubernetes.io/part-of': 'mtc-staging-peak', 'mtc.memeloop.ai/peak-run': 'fixture', 'app.kubernetes.io/component': 'mock' }, ports: [{ port: 18080, targetPort: 8080 }] });
  assert.equal(request.metadata.namespace, 'memeloop-token-center-staging');
  assert(f.calls[2].args.includes('create'));
  assert(!f.calls.flatMap(c => c.args).includes('apply'));
  assert.equal(f.counts().proxies, 0);
});

test('identity mismatches cannot mutate or start proxy', async () => {
  for (const options of [{ cluster: 'wrong' }, { namespace: 'wrong' }, { serviceUid: 'wrong' }, { run: 'wrong' }]) {
    const f = fixture(options);
    await assert.rejects(serviceAction('delete', meta, 'fixture', uid, f.deps), /mismatch/);
    assert.equal(f.counts().deletes, 0); assert.equal(f.counts().proxies, 0);
    assert(!f.calls.some(c => c.args.includes('create')));
  }
});

test('delete accepts only confirmed 200 or 202 and stops own proxy', async () => {
  for (const status of [200, 202]) {
    const f = fixture({ status });
    assert.deepEqual(await serviceAction('delete', meta, 'fixture', uid, f.deps), { name: 'mtc-peak-fixture-mock', deletedUid: uid });
    assert.deepEqual(f.counts(), { deletes: 1, stopped: 1, proxies: 1 });
  }
});

test('replacement 409, absent 404 and response loss never issue a second DELETE', async () => {
  for (const options of [{ status: 409 }, { status: 404 }, { lost: true }]) {
    const f = fixture(options);
    await assert.rejects(serviceAction('delete', meta, 'fixture', uid, f.deps), /unconfirmed/);
    assert.deepEqual(f.counts(), { deletes: 1, stopped: 1, proxies: 1 });
  }
});

test('proxy early exit prevents DELETE', async () => {
  const f = fixture({ proxyFailure: true });
  await assert.rejects(serviceAction('delete', meta, 'fixture', uid, f.deps), /before readiness/);
  assert.equal(f.counts().deletes, 0);
});

test('invalid CLI identity is rejected before any command', async () => {
  for (const [action, run, value] of [['apply', 'fixture', uid], ['delete', '../other', uid], ['delete', 'fixture', 'wrong']]) {
    const f = fixture();
    await assert.rejects(serviceAction(action, meta, run, value, f.deps));
    assert.equal(f.calls.length, 0);
  }
});
