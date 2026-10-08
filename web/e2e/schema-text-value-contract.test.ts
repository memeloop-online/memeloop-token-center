import assert from 'node:assert/strict';
import test from 'node:test';
import { schemaTextValue } from '../src/schemaTextValue.js';

test('explicit-empty text fields display an undefined value as the advertised default', () => {
  assert.equal(schemaTextValue(undefined, 'Bearer ', ''), 'Bearer ');
  assert.equal(schemaTextValue(undefined, 'Custom ', ''), 'Custom ');
  assert.equal(schemaTextValue('', 'Bearer ', ''), '');
  assert.equal(schemaTextValue('Token ', 'Bearer ', ''), 'Token ');
  assert.equal(schemaTextValue(undefined, '', ''), '');
  assert.equal(schemaTextValue(undefined, undefined, ''), '');
  assert.equal(schemaTextValue(undefined, 1, ''), '');
  assert.equal(schemaTextValue(null, 'Bearer ', ''), '');
});

test('text fields without an explicit-empty option retain their existing display behavior', () => {
  assert.equal(schemaTextValue(undefined, 'ordinary-default', undefined), '');
  assert.equal(schemaTextValue('', 'ordinary-default', undefined), '');
  assert.equal(schemaTextValue('custom', 'ordinary-default', undefined), 'custom');
  assert.equal(schemaTextValue(0, undefined, undefined), '0');
});
