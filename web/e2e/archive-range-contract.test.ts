import assert from 'node:assert/strict';
import test from 'node:test';
import { ArchiveChangedError, ArchiveUnavailableError, readArchiveRange } from '../src/archiveRange.js';

test('authorized archive byte pages require precise ranges and carry the first strong ETag', async (context) => {
  let request: RequestInit | undefined;
  context.mock.method(globalThis, 'fetch', async (_url: unknown, init: RequestInit) => {
    request = init;
    return new Response(new Uint8Array([3, 4]), { status: 206, headers: { ETag: '"archive-version"', 'Content-Range': 'bytes 2-3/4' } });
  });
  const result = await readArchiveRange('/self/v1/requests/fixture/archive/response', 'fixture-only', 2, 2, '"archive-version"', new AbortController().signal);
  assert.deepEqual([...result.bytes], [3, 4]);
  assert.equal(result.totalBytes, 4);
  assert.equal(new Headers(request?.headers).get('range'), 'bytes=2-3');
  assert.equal(new Headers(request?.headers).get('if-match'), '"archive-version"');
  assert.equal(new Headers(request?.headers).get('authorization'), 'Bearer fixture-only');
  assert.equal(request?.cache, 'no-store');
});

test('changed archives and ignored ranges never append unverified bytes', async (context) => {
  let response = new Response(null, { status: 412 });
  context.mock.method(globalThis, 'fetch', async () => response);
  const read = () => readArchiveRange('/fixture', 'fixture-only', 0, 2, '"first"', new AbortController().signal);
  await assert.rejects(read, ArchiveChangedError);
  response = new Response('ab', { status: 206, headers: { ETag: '"second"', 'Content-Range': 'bytes 0-1/2' } });
  await assert.rejects(read, ArchiveChangedError);
  response = new Response('entire archive', { status: 200 });
  await assert.rejects(read, /HTTP 200/);
});

test('malformed range headers, weak ETags, and short or excess bodies are rejected', async (context) => {
  let response: Response;
  context.mock.method(globalThis, 'fetch', async () => response);
  for (const [body, range, etag] of [
    ['ab', 'bytes 1-2/3', '"v"'], ['ab', 'bytes 0-1/2', 'W/"v"'],
    ['a', 'bytes 0-1/2', '"v"'], ['abc', 'bytes 0-1/2', '"v"'],
  ]) {
    response = new Response(body, { status: 206, headers: { ETag: etag, 'Content-Range': range } });
    await assert.rejects(() => readArchiveRange('/fixture', 'fixture-only', 0, 2, undefined, new AbortController().signal));
  }
});

test('unreadable bound objects preserve the product API reason without exposing server messages', async (context) => {
  context.mock.method(globalThis, 'fetch', async () => new Response(JSON.stringify({ error: { code: 'archive_content_unavailable', reason: 'archive_object_unavailable' } }), { status: 409 }));
  await assert.rejects(() => readArchiveRange('/fixture', 'fixture-only', 0, 2, undefined, new AbortController().signal),
    error => error instanceof ArchiveUnavailableError && error.reason === 'archive_object_unavailable');
});
