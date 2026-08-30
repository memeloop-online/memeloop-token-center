import assert from 'node:assert/strict';
import test from 'node:test';

import { formatCurrency, formatPercent } from '../src/format.js';

test('currency formatting preserves fixed-decimal strings beyond Number precision', () => {
  assert.equal(formatCurrency('9007199254740993.000000000123400', 'USD', 'en'), '$9,007,199,254,740,993.000000000123400');
  assert.equal(formatCurrency('9007199254740993.000000000123400', 'USD', 'zh-CN'), 'US$9,007,199,254,740,993.000000000123400');
  assert.equal(formatCurrency('-123456789012345678901234.50', 'CNY', 'zh-CN'), '-\u00a5123,456,789,012,345,678,901,234.50');
  assert.equal(formatCurrency('0.000000000000000001', 'EUR', 'en'), '€0.000000000000000001');
});

test('currency formatting preserves scale and follows locale-specific currency affixes', () => {
  assert.equal(formatCurrency('1234.5000', 'CNY', 'en'), 'CN¥1,234.5000');
  assert.equal(formatCurrency('1234.5000', 'CNY', 'zh-CN'), '¥1,234.5000');
  assert.equal(formatCurrency('1234.5000', 'KWD', 'en'), 'KWD\u00a01,234.5000');
  assert.equal(formatCurrency('1.2300e2', 'JPY', 'en'), '¥123.00');
  assert.equal(formatCurrency('1234.500', 'NOT_A_CURRENCY', 'en'), '1,234.500 NOT_A_CURRENCY');
});

test('currency formatting handles finite number callers without imposing six decimal places', () => {
  assert.equal(formatCurrency(9.488113, 'USD', 'en'), '$9.488113');
  assert.equal(formatCurrency(1e-7, 'USD', 'en'), '$0.0000001');
  assert.equal(formatCurrency(Number.NaN, 'USD', 'en'), 'NaN USD');
});

test('success percentages below one never round up to a displayed 100 percent', () => {
  assert.equal(formatPercent(9_999 / 10_000, 'en'), '99.99%');
  assert.equal(formatPercent(9_999 / 10_000, 'zh-CN'), '99.99%');
  assert.equal(formatPercent(99_999 / 100_000, 'en'), '99.999%');
  assert.equal(formatPercent(0.999_999_999, 'zh-CN'), '99.9999999%');
  assert.equal(formatPercent(1, 'en'), '100%');
});
