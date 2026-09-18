import assert from 'node:assert/strict';
import test from 'node:test';
import {
  beginRequestDetailSelection, emptyRequestDetailSelection, rejectRequestDetailSelection,
  requestDetailSelectionInScope, resolveRequestDetailSelection,
} from '../src/operator/requestDetailSelection.js';
import type { RequestDetail, RequestView } from '../src/types.js';

function request(requestId = 'request-a'): RequestView {
  return {
    request_id: requestId, created_at: 1, protocol: 'openai', model: 'fixture', status_code: 200,
    duration_ms: 1, input_tokens: 1, output_tokens: 1, cost: '0', error_code: null,
  };
}

function detail(requestId = 'request-a', archiveComplete = true): RequestDetail {
  return { ...request(requestId), archive_complete: archiveComplete, request_body: { input: 'retained' }, response_body: { output: 'retained' } };
}

test('request detail selection keeps the selected context through a successful load', () => {
  const selected = request();
  const loading = beginRequestDetailSelection(selected, 'tenant-a');
  const resolved = resolveRequestDetailSelection(loading, 'tenant-a', selected.request_id, detail());

  assert.equal(resolved.phase, 'ready');
  assert.equal(resolved.request, selected);
  assert.equal(resolved.detail?.archive_complete, true);
});

test('a failed request detail keeps context available for a retry and retains incomplete archives', () => {
  const selected = request();
  const failed = rejectRequestDetailSelection(beginRequestDetailSelection(selected, 'tenant-a'), 'tenant-a', selected.request_id, 'request detail failed');
  const retried = beginRequestDetailSelection(failed.request!, 'tenant-a');
  const incomplete = resolveRequestDetailSelection(retried, 'tenant-a', selected.request_id, detail(selected.request_id, false));

  assert.deepEqual({ phase: failed.phase, requestId: failed.request?.request_id, error: failed.error }, {
    phase: 'failed', requestId: selected.request_id, error: 'request detail failed',
  });
  assert.equal(incomplete.phase, 'ready');
  assert.equal(incomplete.detail?.archive_complete, false);
});

test('cancellation and stale-scope responses cannot reopen a request detail', () => {
  const selected = request();
  const loading = beginRequestDetailSelection(selected, 'tenant-a');
  const cancelled = emptyRequestDetailSelection();
  const staleResolution = resolveRequestDetailSelection(cancelled, 'tenant-a', selected.request_id, detail());
  const otherScope = requestDetailSelectionInScope(loading, 'tenant-b');

  assert.equal(staleResolution.phase, 'idle');
  assert.equal(otherScope.phase, 'idle');
  assert.equal(requestDetailSelectionInScope(loading, 'tenant-a').request, selected);
});
