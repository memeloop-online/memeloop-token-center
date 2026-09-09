import assert from 'node:assert/strict';
import test from 'node:test';

import { upstreamAvailabilityPath } from '../src/operator/upstreamAvailabilityWindow.js';

test('account availability requires an explicit tenant and encodes one inclusive 24 hour window', () => {
  const now = Date.UTC(2026, 8, 9, 12);
  const url = new URL(upstreamAvailabilityPath('tenant & private', now), 'https://example.test');
  assert.equal(url.pathname, '/internal/v1/upstream-availability');
  assert.equal(url.searchParams.get('tenant_external_id'), 'tenant & private');
  assert.equal(url.searchParams.get('from_created_at'), String(now - 86_400_000));
  assert.equal(url.searchParams.get('to_created_at'), String(now));
  assert.equal(url.searchParams.has('scope'), false);
  assert.throws(() => upstreamAvailabilityPath('', now));
  assert.throws(() => upstreamAvailabilityPath('  ', now));
});
