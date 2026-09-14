// Two observed production shapes, with synthetic values only. No credentials.
export function providerEditShape(variant: string | null) {
  if (!variant) return undefined;
  const models = [variant === 'csil' ? 'gpt-5.3-codex-spark' : 'gpt-5.5', 'gpt-5.6-luna', 'gpt-5.6-sol', 'gpt-5.6-terra', 'gpt-6-astra'];
  return {
    config: {
      network_scope: 'public',
      reservation_token_bounds: Object.fromEntries(models.map(model => [model, 64000])),
      transport_policy: { connect_attempts: 2, connect_timeout_millis: 10000 },
      future_setting: 'preserve-unknown-field',
    },
    schema: {
      required: ['network_scope', 'reservation_token_bounds'],
      properties: {
        network_scope: { type: 'string', const: 'public' },
        reservation_token_bounds: { type: 'object', description: 'Conservative token reservation bounds keyed by exact upstream model.', additionalProperties: { type: 'integer', minimum: 1 } },
        transport_policy: { type: 'object', properties: { connect_attempts: { type: 'integer', minimum: 1 }, connect_timeout_millis: { type: 'integer', minimum: 1 } } },
      },
    },
  };
}
