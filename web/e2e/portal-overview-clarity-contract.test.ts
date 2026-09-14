import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { formatCompactCurrency, formatMetricDisplay } from '../src/format.js';

test('large balances remain finite and retain exact decimal precision', () => {
  const value = '9223372036691.748787';
  assert.deepEqual(formatCompactCurrency(value, 'USD', 'zh-CN'), { text: '9.22万亿 USD', title: 'US$9,223,372,036,691.748787' });
  assert.equal(formatCompactCurrency('100000000', 'USD', 'zh-CN').text, '1亿 USD');
  assert.equal(formatCompactCurrency('0.000001', 'USD', 'en').title, '$0.000001');
  assert.equal(formatCompactCurrency(undefined, 'USD', 'en').text, '—');
  assert.equal(formatMetricDisplay(4294967295, 'zh-CN').text, '42.95亿');
});

test('overview discloses separate statistics and recent-request windows', () => {
  const page = readFileSync(new URL('../src/self/OverviewPage.tsx', import.meta.url), 'utf8');
  assert.match(page, /self-overview-caption.*usage.preset.24h/);
  assert.match(page, /All time/);
  assert.match(page, /requestsPath\(emptyRequestFilters/);
  assert.match(page, /title=\{currentKey.key_id\}/);
  assert.doesNotMatch(page, /<code>\{currentKey.key_id\}/);
});

test('limit display requires explicit unlimited enforcement, never magnitude', () => {
  const source = readFileSync(new URL('../src/LimitSnapshot.tsx', import.meta.url), 'utf8');
  assert.match(source, /enforcementMode === 'metered_unlimited'/);
  assert.match(source, /value.limit === null/);
  assert.doesNotMatch(source, /MAX_SAFE_INTEGER|4294967295|>=/);
});
