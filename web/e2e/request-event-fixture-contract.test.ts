import assert from 'node:assert/strict';
import test from 'node:test';
import { requestEventFixture } from './support/request-event-fixture.js';
import { mergeLiveRequestEvents, requestViewFromEvent } from '../src/operator/traffic/requestTraffic.js';

test('SSE reconnect fixture projects new terminal rows using recorded receipt times and deduplicates replay', () => {
  const event = requestEventFixture('event-1', 'request-1', 1_700_000_000_000, 'fixture-model');
  const projected = requestViewFromEvent(event);
  assert.ok(projected);
  assert.equal(projected.created_at, event.event_at - 42);
  assert.equal(projected.completed_at, event.event_at);
  assert.equal(projected.duration_ms, 42);
  const events = new Map([[event.request_id, event]]);
  const rows = mergeLiveRequestEvents([], events);
  assert.equal(rows.length, 1);
  assert.deepEqual(mergeLiveRequestEvents(rows, events), rows);
  assert.equal(requestViewFromEvent({ ...event, created_at: undefined }), undefined, 'legacy terminal events without receipt evidence still cannot invent a new history row');
});
