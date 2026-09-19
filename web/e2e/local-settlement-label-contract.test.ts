import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

test('summary monetary surfaces label local settlement without changing their amount source', () => {
  for (const path of ['operator/MonitoringSnapshot.tsx', 'operator/UsageSummaryMetrics.tsx', 'self/OverviewPage.tsx']) {
    const source=readFileSync(new URL(`../src/${path}`,import.meta.url),'utf8');
    assert.match(source,/label=\{localSettlementLabel\(locale\)\}/);
    assert.match(source,/labelContent=\{<LocalSettlementNotice\s*\/>\}/);
  }
  const selfUsage=readFileSync(new URL('../src/self/UsagePage.tsx',import.meta.url),'utf8');
  assert.match(selfUsage,/UsageSummaryMetrics/);
  assert.match(selfUsage,/<UsageSummaryMetrics stats=\{stats\} currency=\{credentialView\.currency\} timeZone=\{timeZone\} \/>/);
  const sharedMetrics=readFileSync(new URL('../src/operator/UsageSummaryMetrics.tsx',import.meta.url),'utf8');
  assert.match(sharedMetrics,/label=\{localSettlementLabel\(locale\)\}/);
  assert.match(sharedMetrics,/labelContent=\{<LocalSettlementNotice\s*\/\>\}/);
  const notice=readFileSync(new URL('../src/LocalSettlementNotice.tsx',import.meta.url),'utf8');
  assert.match(notice,/DetailTooltip content=\{detail\}/);
  assert.match(notice,/<span tabIndex=\{0\}/);
  assert.match(notice,/失败请求的费用按 0 计算/);
  assert.match(notice,/供应商已回传用量的请求保留对应结算金额/);
  assert.match(notice,/供应商账单以供应商记录为准/);
  assert.doesNotMatch(notice,/历史汇总可能仍包含尚未完成调整的脏账/);
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
