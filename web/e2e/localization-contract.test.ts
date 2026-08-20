import assert from 'node:assert/strict';
import test from 'node:test';

import { formatMetricNumber } from '../src/format.js';
import { translationCatalogs } from '../src/i18n.js';

test('Chinese and English translation catalogs expose the same keys', () => {
  const chineseKeys = Object.keys(translationCatalogs['zh-CN']).sort();
  const englishKeys = Object.keys(translationCatalogs.en).sort();

  assert.ok(chineseKeys.length > 0, 'the translation catalog must not be empty');
  assert.deepEqual(englishKeys, chineseKeys);
});

test('product copy does not expose legacy migration or adapter terminology', () => {
  for (const [locale, catalog] of Object.entries(translationCatalogs)) {
    const exposed = Object.values(catalog).filter((value) => /CPA|bridge|桥接/i.test(value));
    assert.deepEqual(exposed, [], `${locale} must not expose migration implementation terms`);
  }
});

test('metric numbers use Chinese units through trillion and exact English grouping', () => {
  assert.deepEqual(formatMetricNumber(9_999, 'zh-CN'), { text: '9,999' });
  assert.deepEqual(formatMetricNumber(10_000, 'zh-CN'), { text: '1万', title: '10,000' });
  assert.deepEqual(formatMetricNumber(100_000_000, 'zh-CN'), { text: '1亿', title: '100,000,000' });
  assert.deepEqual(formatMetricNumber(1_000_000_000_000, 'zh-CN'), {
    text: '1万亿',
    title: '1,000,000,000,000',
  });
  assert.deepEqual(formatMetricNumber(-1_250_000_000_000, 'zh-CN'), {
    text: '-1.25万亿',
    title: '-1,250,000,000,000',
  });
  assert.deepEqual(formatMetricNumber(1_000_000_000_000, 'en'), {
    text: '1,000,000,000,000',
  });
});
