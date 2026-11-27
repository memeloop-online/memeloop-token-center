import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import test from 'node:test';
import { client, decode, errorFacts, HttpFailure, segment, sha256, verifyArchiveCopies, verifyStream } from './image-contract.ts';

const frames = [
  { choices: [{ delta: { content: 'synthetic' } }] },
  { choices: [{ finish_reason: 'stop' }] },
  { choices: [], usage: { prompt_tokens: 2, completion_tokens: 1 } },
];
function wire(values: unknown[], done = 'data: [DONE]') { return Buffer.from([...values.map((value) => `data: ${JSON.stringify(value)}`), done].join('\n\n') + '\n\n'); }

test('shared SSE oracle requires unique ordered finish usage and exact EOF', () => {
  verifyStream(wire(frames));
  for (const invalid of [
    wire([frames[0], frames[2], frames[1]]),
    wire([frames[0], frames[1], frames[1], frames[2]]),
    wire([frames[0], frames[1], frames[2], frames[2]]),
    wire([frames[0], frames[1], { usage: { prompt_tokens: 3, completion_tokens: 1 } }]),
    wire([frames[0], frames[1]]),
    wire(frames, ''),
    wire(frames, 'data: [DONE]\n\ndata: [DONE]'),
    wire([{ choices: [{ delta: {} }] }, frames[1], frames[2]]),
    wire([{ choices: [{ delta: { content: 'data: [DONE]' } }] }, frames[1], frames[2]]),
  ]) assert.throws(() => verifyStream(invalid));
});

test('shared archive oracle compares actual request identity and byte-exact response', () => {
  const body = wire(frames), request = Buffer.from('{"input":"nonce-😀"}');
  verifyArchiveCopies('nonce-😀', body, { request, response: body });
  assert.throws(() => verifyArchiveCopies('different-identity', body, { request, response: body }));
  const corrupted = Buffer.from(body); corrupted[0] ^= 1;
  assert.throws(() => verifyArchiveCopies('nonce-😀', body, { request, response: corrupted }));
  assert.notEqual(sha256(body), sha256(corrupted));
});

test('shared decoder rejects damaged UTF8 and path segments retain exact escaped identity', () => {
  const text = `large-body-${'😀'.repeat(40000)}`;
  assert.equal(decode(Buffer.from(text)), text);
  assert.throws(() => decode(Buffer.from([0xf0, 0x9f])));
  assert.equal(segment("a/b !'()*😀"), 'a%2Fb%20%21%27%28%29%2A%F0%9F%98%80');
});

test('shared HTTP client preserves authorization status errors and complete response bytes', { timeout: 10_000 }, async () => {
  const body = Buffer.from([0, 255, 128, 17]); let authorized = false;
  const server = createServer((request, response) => {
    authorized = request.headers.authorization === 'Bearer synthetic-key';
    if (request.url === '/ok') { response.writeHead(200); response.write(body.subarray(0, 2)); response.end(body.subarray(2)); }
    else { response.writeHead(Number(request.url?.slice(1))); response.end('{"error":"synthetic"}'); }
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const address = server.address(); assert(address && typeof address !== 'string');
  try {
    const http = client(`http://127.0.0.1:${address.port}`, 'synthetic-key');
    const response = await http.request('/ok'); assert(authorized); assert.equal(response.status, 200); assert.deepEqual(response.raw, body);
    for (const code of [403, 404, 409, 500]) await assert.rejects(http.request(`/${code}`), (error: unknown) => error instanceof HttpFailure && error.status === code);
    assert.deepEqual(await errorFacts(new Response(JSON.stringify({ error: { code: 'archive_content_unavailable', reason: 'raw_retired_semantic_archive', message: 'secret must never be copied' } }))), { code: 'archive_content_unavailable', reason: 'raw_retired_semantic_archive' });
    for (const payload of ['invalid-json', JSON.stringify({ error: { code: 'untrusted secret', reason: 'untrusted secret' } }), 'x'.repeat(4097)]) assert.deepEqual(await errorFacts(new Response(payload)), { code: 'unknown', reason: 'unknown' });
  } finally { server.closeAllConnections(); await new Promise<void>((resolve) => server.close(() => resolve())); }
});

test('retired raw requires same identity side generation dual sources and complete manifest', async () => {
  const { blake3, preciseJson, verifyRetirementProof } = await import('./retirement-contract.ts');
  const tenant = '11111111-1111-4111-8111-111111111111', request = '22222222-2222-4222-8222-222222222222';
  const hash = blake3(Buffer.alloc(32)), leaf = 'b'.repeat(64), revision = '18446744073709551614';
  assert.equal(preciseJson(`{"revision":${revision}}`).revision, revision);
  const source = { object_locator: `tenants/${tenant}/cas/v1/blake3/${hash.slice(0,2)}/${hash}`, blake3_digest: hash, size_bytes: 32 };
  const coverage = { observed_terminal: true, projection_state: 'complete', gaps: [], omissions: [] };
  const scope = { tenant_id: tenant, request_id: request, leaf, revision, lengths: { request: 32, response: 32 }, digests: { request: hash, response: hash } };
  const publication = { format_version: 2, tenant_id: tenant, request_id: request, revision, projection_version: 2, manifest: source };
  const manifest = { ...publication, semantic_head: { node_hash: leaf }, coverage, raw_sources: { request: source, response: source } };
  const receipt = { format_version: 2, publication, binding: { tenant_id: tenant, request_id: request, node_hash: leaf, projected_at: 1 }, coverage, raw_sources: manifest.raw_sources };
  const fence = { format_version: 1, tenant_id: tenant, request_id: request, side: 'request', source, semantic_revision: revision, semantic_leaf: leaf, release_generation: '00000000-0000-0000-ffff-fffffffffffe' };
  verifyRetirementProof(scope, 'request', fence, receipt, publication, manifest);
  for (const mutate of [
    (v: any) => { const changed = blake3(Buffer.alloc(32, 1)); for (const name of ['request', 'response']) { v.manifest.raw_sources[name].blake3_digest = changed; v.manifest.raw_sources[name].object_locator = `tenants/${tenant}/cas/v1/blake3/${changed.slice(0,2)}/${changed}`; } },
    (v: any) => v.fence.release_generation = request,
    (v: any) => v.fence.side = 'response',
    (v: any) => v.fence.tenant_id = request,
    (v: any) => v.fence.semantic_revision = '1',
    (v: any) => v.fence.semantic_leaf = hash,
    (v: any) => v.fence.release_generation = '00000000-0000-0000-0000-000000000000',
    (v: any) => v.fence.source.blake3_digest = leaf,
    (v: any) => v.receipt.raw_sources.response.size_bytes = 31,
    (v: any) => v.manifest.coverage.gaps = ['missing'],
    (v: any) => v.receipt.coverage.projection_state = 'partial',
    (v: any) => v.manifest.semantic_head.node_hash = hash,
  ]) {
    const value = structuredClone({ fence, receipt, publication, manifest }); mutate(value);
    assert.throws(() => verifyRetirementProof(scope, 'request', value.fence, value.receipt, value.publication, value.manifest));
  }
});

test('retired canonical oracle reads the shared formal atom API and rejects wrong content or cursor stalling', async () => {
  const { verifyPublishedContents } = await import('./retirement-contract.ts');
  const coverage = { observed_terminal: true, projection_state: 'complete', gaps: [] };
  const fixture = (badText = false, stalled = false) => ({ request: async (path: string) => {
    const query = new URL(path, 'http://fixture.invalid').searchParams; let value: any;
    if (query.has('atom_cursor')) {
      const id = query.get('atom_cursor'); value = { role: id === 'u' ? 'user' : 'assistant', parts: [{ part_index: 0, kind: 'text', text: badText ? 'wrong' : id === 'u' ? 'synthetic😀' : 'reply' }] };
    } else value = { status: 'published', manifest_revision: '1', coverage, next_cursor: stalled ? 'same-page' : null, instances: ['u', 'a'].map((id) => ({ request_id: 'request', instance_id: id, atom_cursor: id, role: id === 'u' ? 'user' : 'assistant' })) };
    return { raw: Buffer.from(JSON.stringify(value) + '\n') };
  } });
  const expected = [{ role: 'user', text: 'synthetic😀' }, { role: 'assistant', text: 'reply' }];
  await verifyPublishedContents(fixture(), 'key', 'session', 'request', '1', expected);
  await assert.rejects(verifyPublishedContents(fixture(true), 'key', 'session', 'request', '1', expected));
  await assert.rejects(verifyPublishedContents(fixture(false, true), 'key', 'session', 'request', '1', expected));
});
