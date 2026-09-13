import assert from 'node:assert/strict';
import test from 'node:test';
import { prepareSecretForm } from '../src/secretSchema';
import { safeValidator } from '../src/safeValidator';
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
