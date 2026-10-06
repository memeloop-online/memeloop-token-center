import type { RJSFSchema } from '@rjsf/utils';

export const codexTransportPolicySchema = {
  type: 'object',
  additionalProperties: false,
  default: {},
  description: 'Runtime-adjustable transport policy for this account and its encrypted SOCKS5H binding. Changes apply to newly prepared requests without a service release.',
  properties: {
    version: { type: 'integer', enum: [1], default: 1 },
    connect_timeout_millis: {
      type: 'integer', minimum: 100, maximum: 60000, default: 5000,
      description: 'Pre-delivery connection deadline; must be lower than the total request timeout.',
    },
    read_timeout_millis: {
      type: 'integer', minimum: 1000, maximum: 1260000, default: 600000,
      description: 'Maximum inactivity from response headers to the first body read and between later body reads.',
    },
    request_timeout_millis: {
      type: 'integer', minimum: 1000, maximum: 1260000, default: 1260000,
      description: 'One absolute budget from the first send through the complete response body, including the sole permitted classified HTTP 400 replay.',
    },
    memory_admission_wait_millis: {
      type: 'integer', minimum: 100, maximum: 300000, default: 30000,
      title: 'Memory queue timeout (ms)',
      description: 'Maximum wait for gateway memory capacity within the request deadline.',
    },
    dispatch_max_in_flight: {
      type: 'integer', minimum: 1, maximum: 64, default: 4,
      description: 'Concurrent Codex requests per account and proxy endpoint, including retries and response streams. Runtime decreases drain existing requests.',
    },
    dispatch_max_queued: {
      type: 'integer', minimum: 0, maximum: 1024, default: 32,
      description: 'Maximum FIFO waiters before durable request admission. Zero rejects immediately when busy.',
    },
    dispatch_queue_timeout_millis: {
      type: 'integer', minimum: 1, maximum: 300000, default: 30000,
      description: 'Maximum dispatch queue wait before returning 503 with Retry-After, without request or billing admission.',
    },
    max_sse_event_bytes: {
      type: 'integer', minimum: 262144, maximum: 16777216, default: 8388608,
      title: 'Maximum SSE event bytes',
      description: 'Maximum bytes retained for one upstream SSE event. Responses terminal events may contain the complete response object.',
    },
    max_sse_framed_bytes: {
      type: 'integer', minimum: 262144, maximum: 16842752, default: 8454144,
      title: 'Maximum framed bytes per network chunk',
      description: 'Maximum completed SSE bytes materialized from one upstream network chunk. Must be at least max_sse_event_bytes.',
    },
    max_sse_terminal_hold_bytes: {
      type: 'integer', minimum: 262144, maximum: 16842752, default: 8454144,
      title: 'Maximum terminal hold bytes',
      description: 'Maximum validated Responses terminal bytes held until EOF. Must be between max_sse_event_bytes and max_sse_framed_bytes.',
    },
    candidate_attempts: { type: 'integer', minimum: 1, maximum: 8, default: 3 },
    failover_deadline_millis: { type: 'integer', minimum: 1000, maximum: 300000, default: 300000 },
    connect_attempts: { type: 'integer', minimum: 1, maximum: 4, default: 2 },
    connect_retry_delay_millis: { type: 'integer', minimum: 0, maximum: 2000, default: 150 },
    shared_probe_attempts: { type: 'integer', minimum: 0, maximum: 4, default: 1 },
    chat_controls: {
      type: 'string', enum: ['provider_default', 'strict'], default: 'provider_default', title: 'Chat controls',
      description: 'Provider default validates and removes unsupported Chat sampling and output-limit controls; the upstream applies its own defaults and does not promise the requested sampling settings or token cap. Explicit strict accepts neutral sampling values only and rejects output-limit hints. Quota reservations still use the trusted model ceiling.',
    },
    responses_output_limits: {
      type: 'string', enum: ['provider_default', 'strict'], default: 'provider_default', title: 'Responses output limits',
      description: "Provider default validates and removes the client's output-limit hint; the upstream applies its own defaults and does not enforce the requested token cap. Explicit strict rejects any output-limit hint because Codex OAuth cannot guarantee it. Quota always reserves against the trusted model ceiling and settles against observed usage.",
    },
  },
} satisfies RJSFSchema;

// Observed config shapes and transport overrides, with synthetic values only. No credentials.
export function providerEditShape(variant: string | null) {
  if (!variant) return undefined;
  const models = [variant === 'lindongwu' ? 'gpt-5.5' : 'gpt-5.3-codex-spark', 'gpt-5.6-luna', 'gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-6-astra'];
  return {
    config: {
      network_scope: 'public',
      reservation_token_bounds: Object.fromEntries(models.map((model, index) => [model, variant === 'retry-only' && index === 0 ? 1_000_000_000 : 64000])),
      transport_policy: variant === 'retry-only'
        ? { candidate_attempts: 3, connect_attempts: 4, connect_retry_delay_millis: 150, failover_deadline_millis: 300000, shared_probe_attempts: 4, version: 1 }
        : variant === 'csil'
          ? { connect_attempts: 2, connect_timeout_millis: 10000, memory_admission_wait_millis: 12000,
            dispatch_max_in_flight: 2, dispatch_max_queued: 0, dispatch_queue_timeout_millis: 15000,
            max_sse_event_bytes: 1048576, max_sse_framed_bytes: 1114112, max_sse_terminal_hold_bytes: 1048576,
            chat_controls: 'strict', responses_output_limits: 'provider_default' }
          : { connect_attempts: 2, connect_timeout_millis: 10000 },
      future_setting: 'preserve-unknown-field',
    },
    schema: {
      required: ['network_scope', 'reservation_token_bounds'],
      properties: {
        network_scope: { type: 'string', const: 'public' },
        reservation_token_bounds: { type: 'object', description: 'Conservative token reservation bounds keyed by exact upstream model.', additionalProperties: { type: 'integer', minimum: 1 } },
        transport_policy: structuredClone(codexTransportPolicySchema),
      },
    } satisfies RJSFSchema,
  };
}
