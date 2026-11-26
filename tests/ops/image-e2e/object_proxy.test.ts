import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { createServer, request, type Server } from 'node:http';
import { once } from 'node:events';
import test from 'node:test';
import { createObjectProxy } from './object_proxy.ts';

async function listen(server: Server): Promise<number> {
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const address = server.address(); assert(address && typeof address !== 'string'); return address.port;
}
async function close(server: Server) { server.closeAllConnections(); await new Promise<void>((resolve) => server.close(() => resolve())); }
function exchange(port: number, path: string, method = 'GET', chunks: Buffer[] = [], headers: Record<string, string> = {}) {
  return new Promise<{ status: number; headers: Record<string, string | string[] | undefined>; bytes: Buffer }>((resolve, reject) => {
    const outgoing = request({ hostname: '127.0.0.1', port, method, path, headers, agent: false }, async (response) => {
      try { const parts: Buffer[] = []; for await (const part of response) parts.push(Buffer.from(part)); resolve({ status: response.statusCode!, headers: response.headers, bytes: Buffer.concat(parts) }); }
      catch (error) { reject(error); }
    });
    outgoing.on('error', reject); for (const part of chunks) outgoing.write(part); outgoing.end();
  });
}

test('object proxy preserves signed raw target, Host, binary and encoded response without redirect', { timeout: 10_000 }, async () => {
  const body = Buffer.from([0, 255, 17, 128, 10]);
  const encoded = Buffer.from([31, 139, 8, 0, 255]);
  let observed: { method?: string; path?: string; host?: string; authorization?: string; bytes: Buffer } | undefined;
  const upstream = createServer(async (req, res) => {
    const parts: Buffer[] = []; for await (const chunk of req) parts.push(Buffer.from(chunk));
    observed = { method: req.method, path: req.url, host: req.headers.host, authorization: req.headers.authorization, bytes: Buffer.concat(parts) };
    res.writeHead(307, { 'content-encoding': 'gzip', location: '/must-not-follow', 'x-amz-request-id': 'synthetic' }); res.end(encoded);
  });
  const target = '/bucket/a%2Fb+%2520?uploadId=x%2Fy&partNumber=1&x=2&x=1';
  const upstreamPort = await listen(upstream); const proxy = createObjectProxy({ upstreamPort }); const port = await listen(proxy);
  try {
    const reply = await exchange(port, target, 'PUT', [body.subarray(0, 2), body.subarray(2)], { Host: 'signed-host.invalid', Authorization: 'AWS4-HMAC-SHA256 synthetic-signed-value' });
    assert.deepEqual(observed, { method: 'PUT', path: target, host: 'signed-host.invalid', authorization: 'AWS4-HMAC-SHA256 synthetic-signed-value', bytes: body });
    assert.equal(reply.status, 307); assert.equal(reply.headers['content-encoding'], 'gzip'); assert.equal(reply.headers.location, '/must-not-follow'); assert.deepEqual(reply.bytes, encoded);
  } finally { await close(proxy); await close(upstream); }
});

test('object proxy normal slow fail count reset preserves non-PUT and concurrent requests', { timeout: 10_000 }, async () => {
  let puts = 0;
  const upstream = createServer((req, res) => { if (req.method === 'PUT') puts++; req.resume(); req.on('end', () => { res.writeHead(200); res.end('accepted'); }); });
  const upstreamPort = await listen(upstream); const proxy = createObjectProxy({ upstreamPort, slowMillis: 25 }); const port = await listen(proxy);
  try {
    assert.equal((await exchange(port, '/__test__/slow')).status, 204);
    const began = performance.now();
    await Promise.all([exchange(port, '/bucket/a', 'PUT'), exchange(port, '/bucket/b', 'PUT')]);
    assert(performance.now() - began >= 20, 'slow mode must delay before forwarding');
    assert.equal((await exchange(port, '/__test__/count')).bytes.toString(), '2'); assert.equal(puts, 2);
    assert.equal((await exchange(port, '/__test__/fail')).status, 204);
    assert.equal((await exchange(port, '/bucket/a', 'PUT')).status, 503); assert.equal(puts, 2);
    for (const method of ['GET', 'HEAD', 'DELETE', 'POST']) assert.equal((await exchange(port, '/bucket/a?delete', method)).status, 200);
    assert.equal((await exchange(port, '/__test__/count')).bytes.toString(), '1');
    assert.equal((await exchange(port, '/__test__/normal')).status, 204);
    assert.equal((await exchange(port, '/__test__/count')).bytes.toString(), '0');
    assert.equal((await exchange(port, '/bucket/a', 'PUT')).status, 200); assert.equal(puts, 3);
  } finally { await close(proxy); await close(upstream); }
});

test('object proxy streams upload and download under backpressure with exact byte hashes', { timeout: 10_000 }, async () => {
  const chunk = Buffer.alloc(64 * 1024, 0xa7), count = 128;
  const expected = createHash('sha256'); for (let index = 0; index < count; index++) expected.update(chunk);
  const hash = expected.digest('hex');
  let received = '';
  const upstream = createServer(async (req, res) => {
    const incoming = createHash('sha256'); for await (const bytes of req) incoming.update(bytes); received = incoming.digest('hex');
    res.writeHead(200);
    for (let index = 0; index < count; index++) if (!res.write(chunk)) await once(res, 'drain');
    res.end();
  });
  const upstreamPort = await listen(upstream); const proxy = createObjectProxy({ upstreamPort }); const port = await listen(proxy);
  try {
    let uploadBackpressure = false;
    const reply = await new Promise<string>((resolve, reject) => {
      const outgoing = request({ hostname: '127.0.0.1', port, path: '/bucket/stream?uploadId=signed', method: 'PUT', agent: false }, async (response) => {
        try { const output = createHash('sha256'); for await (const bytes of response) { response.pause(); output.update(bytes); await new Promise<void>((done) => setImmediate(done)); response.resume(); } resolve(output.digest('hex')); } catch (error) { reject(error); }
      });
      outgoing.on('error', reject);
      void (async () => { for (let index = 0; index < count; index++) if (!outgoing.write(chunk)) { uploadBackpressure = true; await once(outgoing, 'drain'); } outgoing.end(); })().catch(reject);
    });
    assert(uploadBackpressure); assert.equal(received, hash); assert.equal(reply, hash);
  } finally { await close(proxy); await close(upstream); }
});

test('object proxy propagates client upload abort to the upstream owner', { timeout: 10_000 }, async () => {
  let announce!: () => void; const entered = new Promise<void>((resolve) => { announce = resolve; });
  let disconnected!: () => void; const aborted = new Promise<void>((resolve) => { disconnected = resolve; });
  const upstream = createServer((req) => { req.on('aborted', disconnected); req.resume(); announce(); });
  const upstreamPort = await listen(upstream); const proxy = createObjectProxy({ upstreamPort }); const port = await listen(proxy);
  try {
    const client = request({ hostname: '127.0.0.1', port, path: '/bucket/unfinished', method: 'PUT', headers: { 'content-length': '1048576' }, agent: false });
    client.on('error', () => {}); client.write(Buffer.alloc(1024)); await entered; client.destroy(); await aborted;
  } finally { await close(proxy); await close(upstream); }
});


test('object proxy propagates client download abort without draining the whole upstream', { timeout: 10_000 }, async () => {
  let cancelled!: () => void; const closed = new Promise<void>((resolve) => { cancelled = resolve; });
  let written = 0;
  const upstream = createServer((req, res) => {
    req.resume();
    res.on('close', () => { if (!res.writableFinished) cancelled(); });
    res.writeHead(200);
    const chunk = Buffer.alloc(64 * 1024);
    const send = () => {
      while (!res.destroyed && written < 1024) { written++; if (!res.write(chunk)) { res.once('drain', send); return; } }
      if (written === 1024) res.end();
    };
    send();
  });
  const upstreamPort = await listen(upstream); const proxy = createObjectProxy({ upstreamPort }); const port = await listen(proxy);
  try {
    await new Promise<void>((resolve, reject) => {
      const client = request({ hostname: '127.0.0.1', port, path: '/bucket/download', agent: false }, (response) => {
        response.once('data', () => { response.destroy(); client.destroy(); resolve(); });
      });
      client.once('error', reject); client.end();
    });
    await closed; assert(written < 1024, 'cancel must stop the upstream rather than buffer the entire object');
  } finally { await close(proxy); await close(upstream); }
});
