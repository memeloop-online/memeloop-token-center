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

test('Chinese copy does not leak English plural Tokens', () => {
  const exposed = Object.values(translationCatalogs['zh-CN']).filter((value) => /\bTokens\b/.test(value));
  assert.deepEqual(exposed, []);
});

test('application rail uses localized product labels instead of OP or SELF abbreviations', async () => {
  const source = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');
  assert.doesNotMatch(source, /operator \? ['"]OP['"] : ['"]SELF['"]/);
  assert.match(source, /t\(operator \? ['"]shell\.operator['"] : ['"]shell\.selfService['"]\)/);
});

test('usage copy accurately names stable upstream accounts and documents session scope', () => {
  assert.equal(translationCatalogs['zh-CN']['usage.tab.upstreams'], '上游账户分析');
  assert.equal(translationCatalogs.en['usage.tab.upstreams'], 'Upstream account analysis');
  assert.match(translationCatalogs['zh-CN']['usage.sessionScope'], /前 100 个会话.*不提供会话 P95.*缓存 Token.*多模态生成任务/);
  assert.match(translationCatalogs.en['usage.sessionScope'], /top 100 sessions.*session P95.*cache-token.*multimodal generation jobs/i);
});

test('routing copy consistently names provider, route, and credential groups', () => {
  for (const [locale, catalog] of Object.entries(translationCatalogs)) {
    const exposed = Object.values(catalog).filter((value) => /标签|规则组|候选池|\b(?:provider|route|credential)\s+(?:tag|pool|rule group)s?\b/i.test(value));
    assert.deepEqual(exposed, [], `${locale} must use the three product group names consistently`);
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
