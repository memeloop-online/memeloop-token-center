import test from 'node:test';
import { contains, excludes } from './contract-helpers.ts';

test('focused source modules retain their extracted boundaries', () => {
  // Guard responsibility placement, not physical line counts: comments,
  // formatting and smaller implementations must not break a release gate.

  for (const needle of ['mod streaming;', 'mod conversation_hints;', 'mod response_metadata;', 'mod routing;', 'mod sse_capture;', 'plan_proxy_route(', 'materialize_proxy_route(', 'send_proxy_route(', 'streaming::stream_response']) {
    contains('src/api/proxy.rs', needle);
  }
  contains('src/api/proxy/routing.rs', 'pub(super) fn plan_proxy_route');
  contains('src/api/proxy/routing.rs', 'pub(super) async fn materialize_proxy_route');
  contains('src/api/proxy/routing.rs', 'pub(super) async fn send_proxy_route');
  contains('src/api/proxy/routing.rs', 'pub(super) fn retryable_upstream_status');
  contains('src/api/proxy/streaming.rs', 'pub(super) async fn stream_response');
  excludes('src/api/proxy.rs', 'pub(super) fn plan_proxy_route');
  excludes('src/api/proxy.rs', 'pub(super) async fn materialize_proxy_route');
  excludes('src/api/proxy.rs', 'pub(super) async fn send_proxy_route');
  excludes('src/api/proxy.rs', 'tokio::spawn(async move');

  contains('src/db/generation/jobs.rs', 'mod finish;');
  excludes('src/db/generation/jobs.rs', 'pub async fn finish_generation_job');
  contains('src/db/generation/jobs/finish.rs', 'pub async fn finish_generation_job');
});
