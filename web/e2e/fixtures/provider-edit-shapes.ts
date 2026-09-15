// Two observed production shapes, with synthetic values only. No credentials.
export function providerEditShape(variant: string | null) {
  if (!variant) return undefined;
  const models = [variant === 'lindongwu' ? 'gpt-5.5' : 'gpt-5.3-codex-spark', 'gpt-5.6-luna', 'gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-6-astra'];
  return {
    config: {
      network_scope: 'public',
      reservation_token_bounds: Object.fromEntries(models.map((model, index) => [model, variant === 'retry-only' && index === 0 ? 1_000_000_000 : 64000])),
      transport_policy: variant === 'retry-only'
        ? { candidate_attempts: 3, connect_attempts: 4, connect_retry_delay_millis: 150, failover_deadline_millis: 300000, shared_probe_attempts: 4, version: 1 }
        : { connect_attempts: 2, connect_timeout_millis: 10000 },
      future_setting: 'preserve-unknown-field',
    },
    schema: {
      required: ['network_scope', 'reservation_token_bounds'],
      properties: {
        network_scope: { type: 'string', const: 'public' },
        reservation_token_bounds: { type: 'object', description: 'Conservative token reservation bounds keyed by exact upstream model.', additionalProperties: { type: 'integer', minimum: 1 } },
        transport_policy: { type: 'object', properties: {
          connect_attempts: { type: 'integer', minimum: 1 },
          connect_timeout_millis: { type: 'integer', minimum: 1, default: 5000, description: 'Pre-delivery connection deadline; must be lower than the total request timeout.' },
          read_timeout_millis: { type: 'integer', minimum: 1, default: 600000, description: 'Maximum inactivity from response headers to the first body read and between later body reads.' },
          request_timeout_millis: { type: 'integer', minimum: 1, default: 1260000, description: 'One absolute budget from the first send through the complete response body, including the sole permitted classified HTTP 400 replay.' },
        } },
      },
    },
  };
}
