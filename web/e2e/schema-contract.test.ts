import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import type { RJSFSchema } from '@rjsf/utils';

import { localizeSchema, translationCatalogs } from '../src/i18n.js';
import { safeValidator } from '../src/safeValidator.js';

interface ParityFixture {
  name: string;
  schema: RJSFSchema;
  cases: Array<{ valid: boolean; value: unknown }>;
}

test('CSP-safe browser validation matches the service contract fixtures', async () => {
  const fixtureUrl = new URL('../../tests/fixtures/schema-parity.json', import.meta.url);
  const fixtures = JSON.parse(await readFile(fixtureUrl, 'utf8')) as ParityFixture[];

  for (const fixture of fixtures) {
    for (const [index, validationCase] of fixture.cases.entries()) {
      assert.equal(
        safeValidator.isValid(fixture.schema, validationCase.value, fixture.schema),
        validationCase.valid,
        `${fixture.name} case ${index}`,
      );
    }
  }
});

test('localized object const, enum, defaults and examples retain original-schema validation', () => {
  const instance = { title: 'API key', description: 'API key', properties: { title: 'API key' } };
  const schema: RJSFSchema = {
    oneOf: [{
      type: 'object', additionalProperties: false, required: ['type'],
      properties: {
        type: { const: 'none' },
        tag: { type: 'object', enum: [instance] },
        fixed: { type: 'object', const: instance },
        fromDefault: { type: 'object', const: instance, default: instance },
        fromExample: { type: 'object', enum: [instance], examples: [instance] },
        title: { const: 'API key' }, description: { const: 'API key' },
      },
    }],
  };
  const original = structuredClone(schema);
  const cases = [
    { valid: true, value: { type: 'none' } },
    { valid: true, value: { type: 'none', tag: instance, fixed: instance, title: 'API key', description: 'API key' } },
    { valid: false, value: { type: 'none', tag: { ...instance, title: 'API credential' } } },
    { valid: false, value: { type: 'none', fixed: { ...instance, description: 'API credential' } } },
    { valid: false, value: { type: 'other', tag: instance } },
  ];
  for (const locale of ['zh-CN', 'en'] as const) {
    const localized = localizeSchema(schema, locale);
    for (const { valid, value } of cases) {
      assert.equal(safeValidator.isValid(schema, value, schema), valid, `${locale} original schema`);
      assert.equal(safeValidator.isValid(localized, value, localized), valid, `${locale} localized schema`);
    }
    const branch = localized.oneOf![0];
    assert.ok(branch && typeof branch === 'object');
    const fields = branch.properties!;
    const data = {
      type: 'none',
      tag: (fields.tag as RJSFSchema).enum![0],
      fixed: (fields.fixed as RJSFSchema).const,
      fromDefault: (fields.fromDefault as RJSFSchema).default,
      fromExample: (fields.fromExample as RJSFSchema).examples![0],
    };
    assert.deepEqual(data, { type: 'none', tag: instance, fixed: instance, fromDefault: instance, fromExample: instance });
    assert.equal(safeValidator.isValid(schema, data, schema), true, `${locale} values extracted from localized schema`);
    const translatedData = { type: 'none', tag: { ...instance, title: translationCatalogs[locale]['schema.API key'] } };
    assert.equal(safeValidator.isValid(schema, translatedData, schema), false);
    assert.equal(safeValidator.isValid(localized, translatedData, localized), false);
    for (const value of [[instance, true, false, null], true, false, null]) {
      const valueSchema: RJSFSchema = { const: value, enum: [value], default: value, examples: [value] };
      const localizedValueSchema = localizeSchema(valueSchema, locale);
      assert.deepEqual(localizedValueSchema, valueSchema);
      assert.equal(safeValidator.isValid(valueSchema, localizedValueSchema.default, valueSchema), true);
      assert.equal(safeValidator.isValid(localizedValueSchema, value, localizedValueSchema), true);
      assert.equal(safeValidator.isValid(valueSchema, 'unexpected', valueSchema), false);
      assert.equal(safeValidator.isValid(localizedValueSchema, 'unexpected', localizedValueSchema), false);
    }
  }
  assert.deepEqual(schema, original);
});

test('local definitions and combinator annotations preserve validation and reference strings', () => {
  const schema: RJSFSchema = {
    $defs: { title: { title: 'API key', description: 'API key', const: { title: 'API key' } } },
    allOf: [{ title: 'API key', $ref: '#/$defs/title' }],
    anyOf: [{ description: 'API key', type: 'object' }, { type: 'null' }],
  };
  const original = structuredClone(schema);
  for (const locale of ['zh-CN', 'en'] as const) {
    const localized = localizeSchema(schema, locale);
    const definition = localized.$defs!.title;
    assert.ok(definition && typeof definition === 'object');
    assert.equal(definition.title, translationCatalogs[locale]['schema.API key']);
    const reference = localized.allOf![0];
    assert.ok(reference && typeof reference === 'object');
    assert.equal(reference.$ref, '#/$defs/title');
    for (const [value, valid] of [[{ title: 'API key' }, true], [{ title: 'API credential' }, false], [null, false]] as const) {
      assert.equal(safeValidator.isValid(schema, value, schema), valid);
      assert.equal(safeValidator.isValid(localized, value, localized), valid);
    }
  }
  assert.deepEqual(schema, original);
});

test('browser validator source contains no dynamic-code execution', async () => {
  const validatorUrl = new URL('../src/safeValidator.ts', import.meta.url);
  const source = await readFile(validatorUrl, 'utf8');

  assert.doesNotMatch(source, /\beval\s*\(/u);
  assert.doesNotMatch(source, /\bnew\s+Function\b/u);
});
