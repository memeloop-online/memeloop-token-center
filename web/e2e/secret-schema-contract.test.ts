import assert from 'node:assert/strict';
import test from 'node:test';
import { prepareSecretForm } from '../src/secretSchema.js';
import { safeValidator } from '../src/safeValidator.js';
import type { RJSFSchema } from '@rjsf/utils';

test('resolved refs, allOf siblings and existing config never prefill secret fields', () => {
  const schema: RJSFSchema = { type: 'object', $defs: { secret: { type: 'string', writeOnly: true } }, properties: {
    config: { type: 'object', required: ['token', 'combined'], properties: {
      token: { $ref: '#/$defs/secret', default: 'synthetic-ref-default', examples: ['synthetic-example'] },
      combined: { allOf: [{ $ref: '#/$defs/secret' }, { default: 'synthetic-allof-default' }] },
      label: { type: 'string', default: 'ordinary-default' },
    } },
  } };
  const create = prepareSecretForm(schema, safeValidator);
  assert.equal(JSON.stringify(create.schema).includes('synthetic-'), false);
  assert.equal(JSON.stringify(create.schema).includes('ordinary-default'), true);
  const edit = prepareSecretForm(schema, safeValidator, { config: { token: 'synthetic-stored', combined: 'synthetic-stored', label: 'existing' } });
  assert.deepEqual(edit.formData, { config: { label: 'existing' } });
  assert.deepEqual((edit.schema.properties?.config as RJSFSchema).required, []);
  assert.equal(safeValidator.isValid(edit.schema, edit.formData, edit.schema), true);
  assert.equal(JSON.stringify(edit).includes('synthetic-'), false);
});

test('nested secret arrays and dynamic or conditional secrets are opaque and unprefilled', () => {
  for (const shape of [
    { type: 'array', minItems: 1, items: { type: 'object', properties: { token: { type: 'string', writeOnly: true } } } },
    { type: 'object', additionalProperties: { type: 'object', properties: { token: { type: 'string', writeOnly: true } } } },
    { type: 'object', if: { properties: { mode: { const: 'private' } } }, then: { properties: { token: { type: 'string', writeOnly: true } } } },
  ]) {
    const schema = { type: 'object', properties: { config: shape } } as RJSFSchema;
    const edit = prepareSecretForm(schema, safeValidator, { config: shape.type === 'array' ? [{ token: 'synthetic-stored' }] : { mode: 'private', token: 'synthetic-stored', dynamic: { token: 'synthetic-stored' } } });
    assert.deepEqual(edit.formData, {});
    assert.equal((edit.schema.properties?.config as RJSFSchema).writeOnly, true);
    assert.equal(JSON.stringify(edit).includes('synthetic-stored'), false);
  }
});
