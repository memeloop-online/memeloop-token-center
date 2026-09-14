import assert from 'node:assert/strict';
import test from 'node:test';
import { providerEditSchema } from '../src/operator/providerEditSchema';

test('provider presentation changes only copy, preserving unknown schemas and constraints', () => {
  const schema = { type: 'object' as const, properties: { config: { type: 'object' as const,
    required: ['network_scope', 'reservation_token_bounds'], additionalProperties: true,
    properties: {
      network_scope: { type: 'string' as const, const: 'public', default: 'public' },
      reservation_token_bounds: { type: 'object' as const, additionalProperties: { type: 'integer' as const, minimum: 1 }, description: 'internal copy' },
      future: { type: 'object' as const, properties: { opaque: { type: 'string' as const, default: 'synthetic-default' } }, additionalProperties: true },
    },
  } } };
  const original = structuredClone(schema);
  for (const locale of ['zh-CN', 'en']) {
    const projected = providerEditSchema(schema, locale);
    const config = projected.properties!.config;
    assert.ok(config && typeof config === 'object');
    assert.deepEqual(config.required, original.properties.config.required);
    assert.equal(config.additionalProperties, true);
    assert.deepEqual(config.properties!.future, original.properties.config.properties.future);
    const bounds = config.properties!.reservation_token_bounds;
    assert.ok(bounds && typeof bounds === 'object');
    assert.deepEqual(bounds.additionalProperties, { type: 'integer', minimum: 1 });
    assert.notEqual(bounds.description, 'internal copy');
  }
  assert.deepEqual(schema, original, 'do not mutate the provider registry schema');
  const implicit = providerEditSchema({ properties: { config: { type: 'object' } } }, 'zh-CN');
  const explicitDeny = providerEditSchema({ properties: { config: { type: 'object', additionalProperties: false } } }, 'zh-CN');
  assert.equal((implicit.properties!.config as { additionalProperties: boolean }).additionalProperties, true);
  assert.equal((explicitDeny.properties!.config as { additionalProperties: boolean }).additionalProperties, false);
});
