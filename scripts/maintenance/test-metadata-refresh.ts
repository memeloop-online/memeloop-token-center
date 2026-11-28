/** Synthetic contracts; execute through GHA. */
import assert from 'node:assert/strict';
import { mkdtempSync, readFileSync, statSync, rmSync, existsSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, basename } from 'node:path';
import test from 'node:test';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import * as m from './metadata-refresh.ts';

function snapshot() {
  return { metadata_updated_at: '2030-01-02T00:00:00Z', route_count: 0,
    route_pagination_complete: true, account_pagination_complete: true,
    account_pages_including_empty_terminal: 2, routes: [],
    accounts: [{ id: 'synthetic-a', name: 'Synthetic provider', status: 'active',
      catalog_read: 'success', catalog: { status: 'ready',
        models: [{ id: 'Exact-Alias', protocol: 'any' }], disabled_models: [] } }] };
}

test('all metadata pages retain only allowlisted fields', async () => {
  for (const [endpoint, fields] of [['upstreams', m.ACCOUNT_FIELDS], ['model-routes', m.ROUTE_FIELDS]] as const) {
    const calls: { path: string; params: Record<string, unknown> }[] = [];
    const [rows, pages] = await m.readPages(async (path, params) => {
      calls.push({ path, params });
      if (calls.length === 3) return [];
      return [{ id: `item-${calls.length}`, created_at: 30 - calls.length,
        credential: 'SYNTHETIC_SECRET', proxy_password: 'SYNTHETIC_SECRET' }];
    }, endpoint, fields);
    assert.deepEqual([rows.length, pages], [2, 3]);
    assert.equal(calls[1]!.params.before_id, 'item-1');
    assert.equal(calls[2]!.params.before_created_at, 28);
    assert.equal(JSON.stringify(rows).includes('SYNTHETIC_SECRET'), false);
  }
});

test('duplicate metadata cursor prevents publication', async () => {
  await assert.rejects(m.readPages(async () => [{ id: 'duplicate', created_at: 1 }],
    'upstreams', m.ACCOUNT_FIELDS), /pagination repeated an ID/);
});

test('refresh preserves exact historical evidence and explicit provider scope', () => {
  const data = snapshot();
  const canonical = { records: [{ record_id: 'record-a', account_id: 'synthetic-a',
    channel_id: 'channel-a', model_id: 'Exact-Alias' }] };
  const evidence = { entries: [{ account_id: 'synthetic-a', model_id: 'Exact-Alias',
    protocol: 'any', tested_at: '2029-12-01T00:00:00Z', outcome: 'tested_success' }] };
  const before = structuredClone(evidence);
  const first = m.render(data, canonical, evidence);
  data.metadata_updated_at = '2030-02-01T00:00:00Z';
  const second = m.render(data, canonical, evidence);
  assert.equal(first.models[0]!.tested_at, second.models[0]!.tested_at);
  assert.deepEqual(evidence, before);
  assert.equal(second.models[0]!.provider_scope, 'unknown_provider_scope');
  assert.deepEqual(second.models[0]!.canonical_record_ids, ['record-a']);
  assert.equal(second.account_pagination_complete, true);
});

test('failed generation preserves atomic latest and private report permissions', () => {
  const data = snapshot();
  const report = m.render(data, { records: [] }, { entries: [] });
  const root = mkdtempSync(join(tmpdir(), 'metadata-contract-'));
  try {
    const first = m.publish(root, data, report);
    const original = readFileSync(join(root, 'latest.json'));
    assert.equal(existsSync(join(first, 'report.md')), true);
    assert.throws(() => m.publish(root, data, report, (path, value) => {
      if (basename(path) === 'report.json') throw new Error('synthetic interruption');
      m.privateWrite(path, value);
    }), /synthetic interruption/);
    assert.deepEqual(readFileSync(join(root, 'latest.json')), original);
    assert.equal(statSync(join(first, 'report.json')).mode & 0o777, 0o600);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('management reads reject remote plaintext, URL credentials and redirected base shapes', () => {
  for (const raw of ['http://example.invalid/internal/v1/', 'https://name:secret@example.invalid/internal/v1/',
    'https://example.invalid/internal/v1/?query=1', 'https://example.invalid/internal/v1/#fragment',
    'https://example.invalid/other/']) assert.throws(() => m.managementUrl(raw));
  for (const raw of ['http://127.0.0.1/internal/v1/', 'http://[::1]/internal/v1/',
    'https://example.invalid/internal/v1/']) assert.doesNotThrow(() => m.managementUrl(raw));
});

test('explicit null catalog collections remain invalid rather than becoming empty evidence', () => {
  const data = snapshot();
  (data.accounts[0]!.catalog as Record<string, unknown>).models = null;
  assert.throws(() => m.render(data, { records: [] }, { entries: [] }));
});

function syntheticReader(catalogText: string) {
  return m.metadataReader('https://example.invalid/internal/v1/', 'SYNTHETIC_TOKEN', 'synthetic', async input => {
    const url = new URL(String(input));
    if (url.pathname.endsWith('/upstreams')) return Response.json(url.searchParams.has('before_id') ? [] : [
      { id: 'synthetic-a', name: 'A', created_at: 2 }, { id: 'synthetic-b', name: 'B', created_at: 1 }]);
    if (url.pathname.endsWith('/model-routes')) return Response.json([]);
    if (url.pathname.endsWith('/synthetic-a/models')) return new Response(catalogText);
    return Response.json({ status: 'ready', models: [], disabled_models: [] });
  });
}

test('catalog invalid JSON is bounded failed evidence while other accounts remain readable', async () => {
  const result = await m.collectMetadata(syntheticReader('{invalid'));
  assert.equal(result.accounts[0]!.catalog_read, 'failed');
  assert.equal(result.accounts[0]!.error_type, 'JSONDecodeError');
  assert.equal(result.accounts[1]!.catalog_read, 'success');
});

test('valid JSON with invalid catalog shape fails before replacing latest', async () => {
  const root = mkdtempSync(join(tmpdir(), 'metadata-shape-'));
  try {
    const initial = snapshot();
    m.publish(root, initial, m.render(initial, { records: [] }, { entries: [] }));
    const original = readFileSync(join(root, 'latest.json'));
    for (const body of ['{"models":null}', '[]', 'null']) {
      await assert.rejects(async () => {
        const collected = await m.collectMetadata(syntheticReader(body));
        m.publish(root, collected, m.render(collected, { records: [] }, { entries: [] }));
      });
    }
    assert.deepEqual(readFileSync(join(root, 'latest.json')), original);
  } finally { rmSync(root, { recursive: true, force: true }); }
});

test('known I/O failures preserve safe persisted error classifications', async () => {
  for (const [name, fetcher] of [
    ['HTTPError', async () => new Response('', { status: 503 })],
    ['URLError', async () => { throw new TypeError('SYNTHETIC_SECRET'); }],
    ['TimeoutError', async () => { throw new DOMException('SYNTHETIC_SECRET', 'TimeoutError'); }],
    ['TimeoutError', async () => new Response(new ReadableStream({
      start(controller) { controller.error(new DOMException('SYNTHETIC_SECRET', 'TimeoutError')); },
    }))],
  ] as const) {
    await assert.rejects(m.metadataReader('https://example.invalid/internal/v1/', 'SYNTHETIC_TOKEN', 'synthetic', fetcher)('upstreams', {}),
      error => error instanceof Error && error.name === name && !error.message.includes('SYNTHETIC_SECRET'));
  }
});


test('CLI help exits zero, argument mistakes exit two without credential output', () => {
  const script = fileURLToPath(new URL('./metadata-refresh.ts', import.meta.url));
  for (const args of [['--help'], ['-h']]) {
    const child = spawnSync(process.execPath, [script, ...args], { encoding: 'utf8' });
    assert.equal(child.status, 0);
    assert.match(child.stdout, /Usage:/);
  }
  for (const args of [[], ['--unknown'], ['--state-dir']]) {
    const child = spawnSync(process.execPath, [script, ...args], { encoding: 'utf8' });
    assert.equal(child.status, 2);
    assert.equal(JSON.parse(child.stdout).status, 'failed');
  }
});

test('invalid pagination shape or missing cursor never replaces latest', async () => {
  const root = mkdtempSync(join(tmpdir(), 'metadata-pagination-'));
  try {
    const initial = snapshot();
    m.publish(root, initial, m.render(initial, { records: [] }, { entries: [] }));
    const original = readFileSync(join(root, 'latest.json'));
    for (const response of [{ items: {} }, { items: { length: 0 } }, {}, null,
      [{ created_at: 1 }], [{ id: 'synthetic-a' }], [null], [[]], ['not-a-row']]) {
      await assert.rejects(async () => {
        const [accounts] = await m.readPages(async () => response, 'upstreams', m.ACCOUNT_FIELDS);
        const collected = { ...initial, accounts };
        m.publish(root, collected, m.render(collected, { records: [] }, { entries: [] }));
      });
      assert.deepEqual(readFileSync(join(root, 'latest.json')), original);
    }
  } finally { rmSync(root, { recursive: true, force: true }); }
});
