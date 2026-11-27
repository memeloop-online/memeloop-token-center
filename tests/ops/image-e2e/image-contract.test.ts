import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { once } from 'node:events';
import test from 'node:test';
import { client, decode, HttpFailure, segment, sha256, verifyArchiveCopies, verifyStream } from './image-contract.ts';

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
  } finally { server.closeAllConnections(); await new Promise<void>((resolve) => server.close(() => resolve())); }
});
