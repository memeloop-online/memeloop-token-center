import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

test('summary monetary surfaces label local settlement without changing their amount source', () => {
  for (const path of ['operator/MonitoringSnapshot.tsx', 'operator/UsageSummaryMetrics.tsx', 'self/OverviewPage.tsx', 'self/UsagePage.tsx']) {
    const source=readFileSync(new URL(`../src/${path}`,import.meta.url),'utf8');
    assert.match(source,/label=\{localSettlementLabel\(locale\)\}/);
    assert.match(source,/labelContent=\{<LocalSettlementNotice\s*\/>\}/);
  }
  const notice=readFileSync(new URL('../src/LocalSettlementNotice.tsx',import.meta.url),'utf8');
  assert.match(notice,/DetailTooltip content=\{detail\}/);
  assert.match(notice,/<span tabIndex=\{0\}/);
  assert.match(notice,/可能包含保守上限结算/);
  assert.match(notice,/不是供应商实际消耗或发票/);
  assert.match(notice,/历史用量来源未记录/);
  assert.doesNotMatch(notice,/fetch\(|api\(|reduce\(/);
});

test('standalone aggregate cost surfaces keep the local-settlement caveat visible', () => {
  for (const path of ['operator/UsageAnalysis.tsx', 'operator/OverviewTrends.tsx']) {
    const source=readFileSync(new URL(`../src/${path}`,import.meta.url),'utf8');
    assert.match(source,/className="analytics-settlement-note"><LocalSettlementNotice\s*\/>/);
    assert.match(source,/localSettlementTrendLabel\(locale\)/);
    assert.match(source,/<th><LocalSettlementNotice\s*\/><\/th>/);
  }
  const selfUsage=readFileSync(new URL('../src/self/UsagePage.tsx',import.meta.url),'utf8');
  assert.match(selfUsage,/localSettlementTrendLabel\(locale\)/);
  assert.match(selfUsage,/<th><LocalSettlementNotice\s*\/><\/th>/);
});
