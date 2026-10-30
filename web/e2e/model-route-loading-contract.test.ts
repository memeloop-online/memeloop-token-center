import assert from 'node:assert/strict';
import test from 'node:test';
import { loadModelRoutePages } from '../src/operator/modelRouteLoading.js';
import type { ModelRouteView } from '../src/types.js';

const page = Array.from({ length: 100 }, (_, index) => ({
  id: `route-${String(100 - index).padStart(3, '0')}`, created_at: 1_000 - index,
  public_model: `model-${index}`, enabled: true,
} as ModelRouteView));
const oldRoute = { ...page[0], id: 'older-astra', created_at: 899, public_model: 'gpt-6-astra' };

test('credential catalogs include older pages and retain tenant and keyset boundaries', async () => {
  const paths: string[] = [];
  const controller = new AbortController();
  const result = await loadModelRoutePages('tenant-a', controller.signal, async (path, signal) => {
    paths.push(path);
    assert.equal(signal, controller.signal);
    return paths.length === 1 ? page : [oldRoute];
  }, 'incomplete');
  assert.equal(result.length, 101);
  assert.equal(result.at(-1)?.public_model, 'gpt-6-astra');
  const first = new URL(paths[0], 'http://fixture').searchParams;
  const second = new URL(paths[1], 'http://fixture').searchParams;
  assert.equal(first.get('tenant_external_id'), 'tenant-a');
  assert.equal(first.get('limit'), '100');
  assert.equal(second.get('tenant_external_id'), 'tenant-a');
  assert.equal(second.get('before_created_at'), String(page.at(-1)!.created_at));
  assert.equal(second.get('before_id'), page.at(-1)!.id);
});

test('malformed pagination fails visibly instead of repeatedly fetching or publishing a partial catalog', async () => {
  let calls = 0;
  await assert.rejects(loadModelRoutePages('tenant-a', new AbortController().signal, async () => {
    calls += 1;
    return page;
  }, 'incomplete'), /incomplete/);
  assert.equal(calls, 2);
  let failedCalls = 0;
  await assert.rejects(loadModelRoutePages('tenant-a', new AbortController().signal, async () => {
    if (++failedCalls === 1) return page;
    throw new Error('second page failed');
  }, 'incomplete'), /second page failed/);
});

test('cancelling a credential scope never publishes a page or continues pagination', async () => {
  const controller = new AbortController();
  let calls = 0;
  await assert.rejects(loadModelRoutePages('tenant-a', controller.signal, async () => {
    calls += 1;
    controller.abort();
    return page;
  }, 'incomplete'), { name: 'AbortError' });
  assert.equal(calls, 1);
});

test('ordinary route lists stay one page and focused navigation stops at its target', async () => {
  let calls = 0;
  const fetchPage = async () => (++calls === 1 ? page : [oldRoute]);
  const first = await loadModelRoutePages('tenant-a', new AbortController().signal, fetchPage, 'incomplete', { firstPageOnly: true });
  assert.equal(first.length, 100);
  assert.equal(calls, 1);
  calls = 0;
  const focused = await loadModelRoutePages('tenant-a', new AbortController().signal, fetchPage, 'incomplete', { focusRouteId: 'older-astra' });
  assert.equal(focused.at(-1)?.id, oldRoute.id);
  assert.equal(calls, 2);
});
