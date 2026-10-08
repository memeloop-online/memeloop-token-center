import assert from 'node:assert/strict';
import test from 'node:test';
import { prepareSecretForm } from '../src/secretSchema.js';
import { safeValidator } from '../src/safeValidator.js';
import { createSchemaUtils, type RJSFSchema } from '@rjsf/utils';
import { builtinApiKeyCredential } from './fixtures/builtin-api-key-credential.js';
import { schemaTextValue } from '../src/schemaTextValue.js';

test('credential branch clearing removes secrets while undefined prefix displays the API default', () => {
  const anonymous: RJSFSchema = { type: 'object', additionalProperties: false, required: ['type'], properties: { type: { const: 'none' } } };
  const schema: RJSFSchema = { type: 'object', properties: { credential: { oneOf: [builtinApiKeyCredential, anonymous] } } };
  const prepared = prepareSecretForm(schema, safeValidator);
  const utils = createSchemaUtils(safeValidator, prepared.schema, { emptyObjectFields: 'skipEmptyDefaults' });
  const credentialSchema = prepared.schema.properties?.credential as RJSFSchema;
  const apiKey = credentialSchema.oneOf?.[0] as RJSFSchema;
  const noAuth = credentialSchema.oneOf?.[1] as RJSFSchema;
  const prefixDefault = (apiKey.properties?.prefix as RJSFSchema).default;
  for (const prefix of ['Bearer ', '', 'Token ']) {
    const draft = { type: 'api_key', value: 'fixture-only-discarded-api-secret', header: 'x-api-key', prefix };
    const snapshot = structuredClone(draft);
    const cleared = utils.sanitizeDataForNewSchema(noAuth, apiKey, draft);
    assert.equal(cleared.value, undefined);
    assert.equal(cleared.prefix, undefined);
    const returned = utils.getDefaultFormState(apiKey, utils.sanitizeDataForNewSchema(apiKey, noAuth, cleared), 'excludeObjectChildren');
    assert.equal(returned.value, undefined);
    assert.equal(returned.prefix, undefined);
    assert.equal(Object.hasOwn(returned, 'prefix'), true);
    assert.equal(Object.hasOwn(JSON.parse(JSON.stringify(returned)), 'prefix'), false);
    assert.equal(schemaTextValue(returned.prefix, prefixDefault, ''), 'Bearer ');
    assert.equal(schemaTextValue('', prefixDefault, ''), '');
    assert.deepEqual(draft, snapshot);
  }
});

test('API-key draft defaults distinguish an explicit empty prefix from an absent prefix', () => {
  const schema: RJSFSchema = { type: 'object', properties: { credential: builtinApiKeyCredential } };
  const snapshot = structuredClone(schema);
  const prepared = prepareSecretForm(schema, safeValidator);
  const utils = createSchemaUtils(safeValidator, prepared.schema, { emptyObjectFields: 'skipEmptyDefaults' });
  for (const prefix of [undefined, '', 'Bearer ', 'Token ']) {
    const credential = { type: 'api_key', value: 'fixture-only-secret', header: 'x-api-key', ...(prefix === undefined ? {} : { prefix }) };
    const draft = { credential };
    const draftSnapshot = structuredClone(draft);
    const normalized = utils.getDefaultFormState(prepared.schema, draft) as typeof draft;
    assert.equal(normalized.credential.prefix, prefix === undefined ? 'Bearer ' : prefix);
    assert.equal(normalized.credential.value, credential.value);
    assert.equal(safeValidator.isValid(prepared.schema, normalized, prepared.schema), true);
    assert.deepEqual(JSON.parse(JSON.stringify(normalized)).credential, { ...credential, prefix: prefix === undefined ? 'Bearer ' : prefix });
    assert.deepEqual(draft, draftSnapshot);
    const edit = prepareSecretForm(schema, safeValidator, draft);
    assert.deepEqual(edit.formData, { credential: { type: 'api_key', header: 'x-api-key', ...(prefix === undefined ? {} : { prefix }) } });
  }
  assert.deepEqual(schema, snapshot);
});

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

test('schema keyword names remain valid secret field and definition names', () => {
  for (const key of ['default', 'examples', 'const', 'enum']) {
    const secret: RJSFSchema = { type: 'string', writeOnly: true, default: 'synthetic-default', examples: ['synthetic-example'] };
    for (const schema of [
      { type: 'object', properties: { [key]: secret } },
      { type: 'object', $defs: { [key]: secret }, properties: { [key]: { $ref: `#/$defs/${key}` } } },
    ] as RJSFSchema[]) {
      const prepared = prepareSecretForm(schema, safeValidator, { [key]: 'synthetic-existing' });
      assert.deepEqual(prepared.formData, {});
      assert.equal(JSON.stringify(prepared).includes('synthetic-'), false);
    }
  }
});
