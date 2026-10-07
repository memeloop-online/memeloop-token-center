import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import { formatMetricNumber } from '../src/format.js';
import { translationCatalogs } from '../src/i18n.js';
import { tenantDisplayName } from '../src/tenantDisplayName.js';
import { formJourneyCopy } from '../src/operator/formJourneyCopy.js';

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

test('operator guidance gives user actions instead of development reports and keeps consequential warnings', () => {
  const chinese = translationCatalogs['zh-CN'];
  const english = translationCatalogs.en;
  for (const catalog of [chinese, english]) {
    assert.doesNotMatch(Object.values(catalog).join('\n'), /本轮交付|待交付|验收矩阵|工作日志|已具备代理组管理权限|worklog|acceptance matrix/i);
    assert.ok(catalog['providers.manualHealthNotRun'].includes(catalog['providers.runManualHealthCheck']));
    assert.ok(catalog['settings.noEnabledRouteHint'].includes(catalog['providers.title']));
    assert.ok(catalog['settings.noEnabledRouteHint'].includes(catalog['nav.routes']));
  }
  assert.match(chinese['quota.resetDiscoveryPending'], /查看额度.*不会消耗重置次数/);
  assert.match(english['quota.resetDiscoveryPending'], /View quota.*does not use a reset credit/);
  assert.match(chinese['quota.resetDiscoveryFailed'], /检查账号连接.*未执行重置.*未消耗重置次数/);
  assert.match(english['quota.resetDiscoveryFailed'], /Check the account connection.*No reset was performed.*no reset credit was used/);
  assert.match(chinese['quota.resetNotIntegrated'], /通过模型服务提供方重置额度/);
  assert.match(english['quota.resetNotIntegrated'], /Manage its quota with the model service provider/);
  assert.match(chinese['quota.resetOperationError'], /尚未确认.*不要重复发起/);
  assert.match(english['quota.resetOperationError'], /not confirmed.*do not start another reset/i);
  assert.match(chinese['managedSync.warning.catalog_not_ready'], /路由保持不变.*重新同步模型/);
  assert.match(english['managedSync.warning.catalog_not_ready'], /Routes are unchanged.*syncing models again/);
  for (const key of ['quota.resetDiscoveryPending', 'quota.resetDiscoveryFailed', 'quota.resetNotIntegrated', 'settings.noEnabledRouteHint', 'settings.filterAssistantRouteHint'] as const) {
    for (const catalog of [chinese, english]) assert.doesNotMatch(catalog[key], /重置能力|展开此处|尚未接入|提供商声明|上游探测|not yet integrated|provider declaration|this control never probes/i);
  }
});

test('provider settings explain user choices through shared bilingual guidance', async () => {
  const template = await readFile(new URL('../src/operator/UpstreamFormTemplates.tsx', import.meta.url), 'utf8');
  for (const key of ['timeouts', 'timeoutsHint', 'advancedConnection', 'advancedConnectionHint']) assert.ok(template.includes(`copy.${key}`));
  for (const locale of ['zh-CN', 'en'] as const) {
    const catalog = translationCatalogs[locale];
    const copy = formJourneyCopy(locale);
    assert.doesNotMatch([copy.advancedConnectionHint, copy.timeoutsHint, catalog['connection.capabilitiesHint'], catalog['quota.readUnsupported']].join('\n'), /现有字段完整保留|校验失败会自动展开|尚无可用的额度读取能力|Existing fields are preserved|validation errors reveal|not yet available/i);
    assert.doesNotMatch(catalog['connection.capabilitiesSection'], /契约|contracts/i);
  }
  assert.equal(translationCatalogs['zh-CN']['routes.catalogReady'], '选择或输入要使用的模型。');
  assert.equal(translationCatalogs.en['routes.catalogReady'], 'Select or enter the model to use.');
  assert.match(formJourneyCopy('zh-CN').advancedConnectionHint, /不确定时保持原值/);
  assert.match(formJourneyCopy('en').advancedConnectionHint, /Keep the current values if unsure/);
  assert.match(translationCatalogs['zh-CN']['connection.capabilitiesHint'], /收起后会保留已填写内容/);
  assert.match(translationCatalogs.en['connection.capabilitiesHint'], /Collapsing keeps your entered values/);
  assert.match(translationCatalogs['zh-CN']['quota.readUnsupported'], /前往模型服务提供方查看/);
  assert.match(translationCatalogs.en['quota.readUnsupported'], /Check it with the model service provider/);
});

test('tenant copy stays action-focused and its active locale keys are not orphaned', async () => {
  const operator = await readFile(new URL('../src/operator/Operator.tsx', import.meta.url), 'utf8');
  const manager = await readFile(new URL('../src/operator/TenantManager.tsx', import.meta.url), 'utf8');
  const staleCopy = /创建客户端凭据不会创建租户|Creating a client credential never creates a tenant|aggregate view|未指定租户的写入|writes with no explicit tenant/i;
  for (const [locale, catalog] of Object.entries(translationCatalogs)) {
    assert.doesNotMatch(Object.values(catalog).join('\n'), staleCopy, `${locale} tenant copy must not expose product-design discussion`);
  }
  assert.match(operator, /\{ route: 'tenants', label: 'nav\.tenants', domId: 'tenants' \}/);
  assert.match(operator, /navigation\.map\(\(item\).*t\(item\.label\)/s);
  for (const key of [
    'tenants.title', 'tenants.description', 'tenants.create', 'tenants.name', 'tenants.rename', 'tenants.archive', 'tenants.restore', 'tenants.delete',
    'tenants.renameTitle', 'tenants.archiveTitle', 'tenants.restoreTitle', 'tenants.deleteTitle',
    'tenants.renameImpact', 'tenants.archiveImpact', 'tenants.restoreImpact', 'tenants.deleteImpact',
  ]) assert.match(manager, new RegExp(`t\\('${key.replace('.', '\\.')}'\\)`));
  assert.doesNotMatch(manager, /window\.(?:confirm|prompt)/);
});

test('Chinese usage copy consistently uses 词元 instead of Token or Tokens', () => {
  // Interpolation identifiers are protocol-independent keys, not visible copy.
  const exposed = Object.values(translationCatalogs['zh-CN']).filter((value) => /\bTokens?\b/i.test(value.replace(/\{\{\w+\}\}/g, '')));
  assert.deepEqual(exposed, []);
  assert.equal(translationCatalogs['zh-CN']['usage.tokens'], '词元');
  assert.equal(translationCatalogs.en['usage.tokens'], 'Tokens');
});

test('default tenant is localized for display without changing external IDs or custom names', () => {
  assert.equal(tenantDisplayName('default', 'zh-CN'), '默认');
  assert.equal(tenantDisplayName('default', 'en'), 'default');
  assert.equal(tenantDisplayName('Default', 'zh-CN'), 'Default');
  assert.equal(tenantDisplayName('default-project', 'zh-CN'), 'default-project');
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
    const sseLimitTitle = locale === 'zh-CN' ? '最大 SSE 事件（字节）' : 'Maximum SSE event (bytes)';
    assert.equal(catalog['schema.Maximum SSE event (bytes)'], sseLimitTitle);
    const exposed = Object.entries(catalog)
      .filter(([, value]) => /\bSSE\b|\bepoch\b|stable cursor|indexed fields|JSON Schema|Wasmtime|稳定游标|索引字段|毫秒 epoch/i.test(value))
      .filter(([key, value]) => key !== 'schema.Maximum SSE event (bytes)' || value !== sseLimitTitle);
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
