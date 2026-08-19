import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import type { RJSFSchema } from '@rjsf/utils';

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

test('browser validator source contains no dynamic-code execution', async () => {
  const validatorUrl = new URL('../src/safeValidator.ts', import.meta.url);
  const source = await readFile(validatorUrl, 'utf8');

  assert.doesNotMatch(source, /\beval\s*\(/u);
  assert.doesNotMatch(source, /\bnew\s+Function\b/u);
});
