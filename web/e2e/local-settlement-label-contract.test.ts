import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

test('summary monetary surfaces label local settlement without changing their amount source', () => {
  for (const path of ['operator/MonitoringSnapshot.tsx', 'operator/UsageSummaryMetrics.tsx', 'self/OverviewPage.tsx', 'self/UsagePage.tsx']) {
    const source=readFileSync(new URL(`../src/${path}`,import.meta.url),'utf8');
    assert.match(source,/label=\{localSettlementLabel\(locale\)\}/);
    assert.match(source,/<LocalSettlementNotice\s*\/>/);
  }
  const notice=readFileSync(new URL('../src/LocalSettlementNotice.tsx',import.meta.url),'utf8');
  assert.match(notice,/DetailTooltip content=\{detail\}/);
  assert.match(notice,/<Button[^>]*type="button"/);
  assert.match(notice,/可能包含保守上限结算/);
  assert.match(notice,/不是供应商实际消耗或发票/);
  assert.match(notice,/历史用量来源未记录/);
  assert.doesNotMatch(notice,/fetch\(|api\(|reduce\(/);
});
