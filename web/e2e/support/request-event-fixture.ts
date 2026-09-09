import type { RequestEvent } from '../../src/types.js';

/** A recorded terminal fact, with receipt and completion distinct from its cursor identity. */
export function requestEventFixture(eventId: string, requestId: string, eventAt: number, eventModel: string): RequestEvent {
  return {
    event_id: eventId,
    request_id: requestId,
    event_at: eventAt,
    created_at: eventAt - 42,
    completed_at: eventAt,
    event_kind: 'finished',
    key_id: '019f0000-0000-7000-a000-000000000001',
    protocol: 'openai',
    model: eventModel,
    status_code: 200,
    duration_ms: 42,
    input_tokens: 5,
    output_tokens: 7,
    cost: '0.000019',
    error_code: null,
  };
}
