import assert from 'node:assert/strict';
import test from 'node:test';
import { averageRequestOutputTps, generationRequestOutputTps, nonCachedRequestInput, requestCostCopy, requestCredentialLabel, requestIsPending, requestUsageIsActual } from '../src/requestTablePresentation.js';
import { requestViewFromEvent } from '../src/operator/traffic/requestTraffic.js';
import type { RequestEvent } from '../src/types.js';
import type { RequestView } from '../src/types.js';
import { requestOutcome, requestStatusCopy } from '../src/requestStatusPresentation.js';

const request: RequestView = {
  usage_basis: 'provider_reported',
  request_id: 'fixture', created_at: 1, protocol: 'openai', model: 'fixture', status_code: 200,
  duration_ms: 2000, input_tokens: 160, cached_input_tokens: 40, cache_write_tokens: 20,
  output_tokens: 32, cost: '0', error_code: null,
};

test('generation TPS requires observed output interval and does not relabel total-duration fallback', () => {
  const timed = { ...request, first_output_ms: 1000, generation_duration_ms: 500 };
  assert.equal(generationRequestOutputTps(timed), 64);
  assert.equal(averageRequestOutputTps(timed), 16);
  assert.equal(generationRequestOutputTps(request), null);
  for (const generation_duration_ms of [null, 0, -1, Number.NaN, Infinity]) assert.equal(generationRequestOutputTps({ ...timed, generation_duration_ms }), null);
  assert.equal(generationRequestOutputTps({ ...timed, status_code: 499 }), 64, 'a later disconnect does not erase reported usage and a complete recorded interval');
  for (const usage_basis of [undefined, null, 'contract_ceiling', 'provider_estimated', 'not_observed'] as const) {
    assert.equal(generationRequestOutputTps({ ...timed, usage_basis }), null);
    assert.equal(averageRequestOutputTps({ ...timed, usage_basis }), null);
  }
  assert.equal(averageRequestOutputTps({ ...timed, usage_basis: 'contract_ceiling', output_tokens: 100000, duration_ms: 11681 }), null, 'the reported 8560.91 reservation-ceiling ratio must not appear as actual throughput');
  assert.equal(generationRequestOutputTps({ ...timed, first_output_ms: undefined }), null);
  assert.equal(generationRequestOutputTps({ ...timed, generation_duration_ms: 1500 }), null, 'an interval cannot extend beyond total duration');
  assert.equal(generationRequestOutputTps({ ...timed, duration_ms: null }), null);
  assert.equal(generationRequestOutputTps({ ...timed, compaction: true }), null);
  assert.equal(averageRequestOutputTps({ ...timed, compaction: true }), null);
  assert.equal(generationRequestOutputTps({ ...timed, generation_duration_ms: 1 }), 32000, 'no arbitrary high-rate threshold masks an otherwise recorded gateway interval');
});

test('unreliable failed usage and cost remain unknown without erasing reported settlements', () => {
  for (const usage_basis of [undefined, null, 'provider_estimated', 'contract_ceiling', 'not_observed'] as const) {
    const failed = { ...request, status_code: 499, usage_basis, cost: '35' };
    assert.equal(requestUsageIsActual(failed), false);
    assert.equal(requestCostCopy(failed, 'en').unknown, true);
    assert.equal(failed.cost, '35');
  }
  const reported = { ...request, status_code: 499, cost: '0.2' };
  assert.equal(requestUsageIsActual(reported), true);
  assert.equal(requestCostCopy(reported, 'en').unknown, false);
  assert.equal(requestUsageIsActual({ ...request, usage_basis: 'not_observed' }), false);
});

test('legacy and archive SSE patches retain recorded timing', () => {
  const timed = { ...request, first_output_ms: 1000, generation_duration_ms: 500 };
  const event = { ...request, event_id: 'event', event_at: 3000, event_kind: 'finished', key_id: 'key' } as RequestEvent;
  assert.equal(requestViewFromEvent(event, timed)?.generation_duration_ms, 500);
  assert.equal(requestViewFromEvent({ ...event, first_output_ms: null }, timed)?.first_output_ms, 1000);
  assert.equal(requestViewFromEvent({ ...event, first_output_ms: 1200, generation_duration_ms: 400 }, timed)?.generation_duration_ms, 400);
  assert.equal(requestViewFromEvent({ ...event, usage_basis: undefined }, timed)?.usage_basis, 'provider_reported');
  assert.equal(requestViewFromEvent({ ...event, usage_basis: null }, timed)?.usage_basis, null, 'explicit unknown clears old provenance; absence only preserves it');
  assert.equal(requestViewFromEvent({ ...event, usage_basis: 'contract_ceiling' }, timed)?.usage_basis, 'contract_ceiling');
});

test('uncached input uses both recorded cache components and does not fabricate missing counts', () => {
  assert.equal(nonCachedRequestInput(request), 100);
  assert.equal(nonCachedRequestInput({ ...request, cached_input_tokens: undefined }), null);
  assert.equal(nonCachedRequestInput({ ...request, cache_write_tokens: undefined }), null);
  assert.equal(nonCachedRequestInput({ ...request, cached_input_tokens: 150 }), null);
  assert.equal(nonCachedRequestInput({ ...request, status_code: null, input_tokens: 0, cached_input_tokens: 0, cache_write_tokens: 0 }), null);
  assert.equal(nonCachedRequestInput({ ...request, input_tokens: 0, cached_input_tokens: 0, cache_write_tokens: 0 }), 0);
});

test('average output TPS uses recorded total seconds, distinguishes valid zero from unavailable and running', () => {
  assert.equal(averageRequestOutputTps(request), 16);
  assert.equal(averageRequestOutputTps({ ...request, output_tokens: 0 }), 0);
  for (const duration_ms of [null, 0, -1, Number.NaN, Number.POSITIVE_INFINITY]) assert.equal(averageRequestOutputTps({ ...request, duration_ms }), null);
  assert.equal(averageRequestOutputTps({ ...request, status_code: null }), null);
  assert.equal(averageRequestOutputTps({ ...request, output_tokens: -1 }), null);
});

test('unobserved local zero is presented as unknown supplier cost', () => {
  const unknown = requestCostCopy({ ...request, usage_basis: 'not_observed', cost: '0' }, 'en');
  assert.equal(unknown.unknown, true);
  assert.equal(unknown.label, 'Cost unknown');
  assert.match(unknown.hint, /does not prove/);
  assert.equal(requestCostCopy(request, 'en').unknown, false);
});

test('credential identity remains meaningful without inventing an alias for explicitly unbound history', () => {
  assert.deepEqual(requestCredentialLabel(request, 'Portal key'), { label: 'Portal key' });
  assert.deepEqual(requestCredentialLabel({ ...request, credential_identity: null }, 'Portal key'), { key: 'request.missingCredential' });
  assert.deepEqual(requestCredentialLabel({ ...request, credential_identity: { tenant_external_id: 'fixture', key_id: 'key', key_alias: '', principal_external_id: 'user' } }, 'Portal key'), { key: 'request.unnamedCredential' });
});

test('terminal imported history need not have completion timestamps; live started events remain pending', () => {
  assert.equal(requestIsPending({ ...request, status_code: null, completed_at: null }), true);
  assert.equal(requestIsPending({ ...request, completed_at: null }), false);
  assert.equal(averageRequestOutputTps({ ...request, completed_at: null }), 16);
  assert.equal(requestOutcome(request), 'unknown', 'a historical status alone does not prove completed delivery');
  assert.equal(requestOutcome({ ...request, completed_at: 3000 }), 'completed');
  assert.equal(requestOutcome({ ...request, completed_at: 3000, error_code: 'upstream_incomplete_response' }), 'interrupted', 'even 200 must not hide a recorded terminal error');
  assert.equal(requestOutcome({ ...request, status_code: 499, error_code: 'client_cancelled' }), 'cancelled');
  assert.equal(requestOutcome({ ...request, status_code: null, error_code: 'delivery_started' }), 'delivering');
  assert.match(requestStatusCopy({ ...request, completed_at: 3000 }, 'en').hint, /not client acknowledgement/);
});

test('Anthropic uses persisted normalized total input, not raw provider input', () => {
  // Provider input=100 + cache_read=40 + cache_creation=20 persists as input_tokens=160.
  assert.equal(nonCachedRequestInput({ ...request, protocol: 'anthropic', input_tokens: 160 }), 100);
});
