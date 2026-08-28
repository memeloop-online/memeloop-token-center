import assert from 'node:assert/strict';
import test from 'node:test';
import { contains, excludes, read } from './contract-helpers.ts';

function assertRange(path: string, minimum: number, maximum: number): void {
  const lines = read(path).split('\n').length - 1;
  assert.ok(lines >= minimum && lines <= maximum, `${path} has ${lines} lines; expected ${minimum}..${maximum}`);
}

test('focused source modules retain their extracted boundaries', () => {
  assertRange('src/api/proxy.rs', 1, 1700);
  assertRange('src/api/proxy/conversation_hints.rs', 180, 280);
  assertRange('src/api/proxy/routing.rs', 190, 280);
  assertRange('src/api/proxy/streaming.rs', 400, 550);
  assertRange('src/db/generation/jobs.rs', 1, 1550);
  assertRange('src/db/generation/jobs/finish.rs', 300, 450);

  for (const needle of ['mod streaming;', 'mod conversation_hints;', 'mod routing;', 'prepare_proxy_route(', 'send_proxy_route(', 'streaming::stream_response']) {
    contains('src/api/proxy.rs', needle);
  }
  contains('src/api/proxy/routing.rs', 'pub(super) async fn prepare_proxy_route');
  contains('src/api/proxy/routing.rs', 'pub(super) async fn send_proxy_route');
  contains('src/api/proxy/routing.rs', 'pub(super) fn retryable_upstream_status');
  contains('src/api/proxy/streaming.rs', 'pub(super) async fn stream_response');
  excludes('src/api/proxy.rs', 'pub(super) async fn prepare_proxy_route');
  excludes('src/api/proxy.rs', 'pub(super) async fn send_proxy_route');
  excludes('src/api/proxy.rs', 'tokio::spawn(async move');

  const proxy = read('src/api/proxy.rs').split('\n');
  const start = proxy.findIndex((line) => line.startsWith('pub(super) async fn proxy('));
  const end = proxy.findIndex((line) => line.startsWith('fn record_delivered_chunk('));
  assert.ok(start >= 0 && end >= start && end - start <= 550, `proxy entrypoint spans ${end - start} lines; expected at most 550`);

  contains('src/db/generation/jobs.rs', 'mod finish;');
  excludes('src/db/generation/jobs.rs', 'pub async fn finish_generation_job');
  contains('src/db/generation/jobs/finish.rs', 'pub async fn finish_generation_job');
});
