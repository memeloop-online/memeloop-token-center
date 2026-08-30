import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
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
    const exposed = Object.values(catalog).filter((value) => /CPA|bridge|桥接|旧版|legacy|迁移|migration/i.test(value));
    assert.deepEqual(exposed, [], `${locale} must not expose migration implementation terms`);
  }
});

test('credential and empty-state copy does not infer bootstrap or candidate-environment context', () => {
  for (const [locale, catalog] of Object.entries(translationCatalogs)) {
    const exposed = Object.values(catalog).filter((value) => /部署引导凭据|候选环境|deployment bootstrap credential|candidate (?:data|environment)/i.test(value));
    assert.deepEqual(exposed, [], `${locale} must not infer credential provenance or deployment workflow context`);
  }
});

test('Chinese copy does not leak English plural Tokens', () => {
  const exposed = Object.values(translationCatalogs['zh-CN']).filter((value) => /\bTokens\b/.test(value));
  assert.deepEqual(exposed, []);
});

test('application rail uses localized product labels instead of OP or SELF abbreviations', async () => {
  const source = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');
  assert.doesNotMatch(source, /operator \? ['"]OP['"] : ['"]SELF['"]/);
  assert.match(source, /t\(operator \? ['"]shell\.operator['"] : ['"]shell\.selfService['"]\)/);
});

test('usage copy names stable upstream accounts without exposing analytics implementation notes', () => {
  assert.equal(translationCatalogs['zh-CN']['usage.tab.upstreams'], '上游账户分析');
  assert.equal(translationCatalogs.en['usage.tab.upstreams'], 'Upstream account analysis');
  assert.match(translationCatalogs['zh-CN']['usage.sessionScope'], /100 个会话.*未关联会话.*单独列出/);
  assert.match(translationCatalogs.en['usage.sessionScope'], /100 busiest sessions.*without a session.*separately/i);
  for (const [locale, catalog] of Object.entries(translationCatalogs)) {
    const exposed = Object.values(catalog).filter((value) => /\bSSE\b|\bepoch\b|stable cursor|indexed fields|JSON Schema|Wasmtime|稳定游标|索引字段|毫秒 epoch/i.test(value));
    assert.deepEqual(exposed, [], `${locale} must not expose transport, storage, or runtime implementation notes`);
  }
});

test('routing copy consistently names provider, route, and credential groups', () => {
  for (const [locale, catalog] of Object.entries(translationCatalogs)) {
    const exposed = Object.values(catalog).filter((value) => /标签|规则组|候选池|\b(?:provider|route|credential)\s+(?:tag|pool|rule group)s?\b/i.test(value));
    assert.deepEqual(exposed, [], `${locale} must use the three product group names consistently`);
  }
});

test('metric numbers keep locale-grouped exact units primary and expose compact text as secondary metadata', () => {
  assert.deepEqual(formatMetricNumber(9_999, 'zh-CN'), { text: '9,999' });
  assert.deepEqual(formatMetricNumber(10_000, 'zh-CN'), { text: '10,000', compact: '1万' });
  assert.deepEqual(formatMetricNumber(330_300, 'zh-CN'), { text: '330,300', compact: '33.03万' });
  assert.deepEqual(formatMetricNumber(100_000_000, 'zh-CN'), { text: '100,000,000', compact: '1亿' });
  assert.deepEqual(formatMetricNumber(1_000_000_000_000, 'zh-CN'), {
    text: '1,000,000,000,000',
    compact: '1万亿',
  });
  assert.deepEqual(formatMetricNumber(-1_250_000_000_000, 'zh-CN'), {
    text: '-1,250,000,000,000',
    compact: '-1.25万亿',
  });
  assert.deepEqual(formatMetricNumber(1_000_000_000_000, 'en'), {
    text: '1,000,000,000,000',
    compact: '1T',
  });
});
