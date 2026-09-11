import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const source = readFileSync(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
const pricing = source.slice(source.indexOf('function Pricing('), source.indexOf('interface RouteDraft'));
const page = source.slice(source.indexOf('export function PricingPage('), source.indexOf('export function RoutesPage('));

test('manual schemas are fetched only when the collapsed editor is opened', () => {
  assert.doesNotMatch(page, /<ResourceBoundary/);
  assert.match(page, /Boolean\(token\) && schemasRequested/);
  assert.match(page, /onRequestSchemas=\{\(\) => setSchemasRequested\(true\)\}/);
  assert.match(page, /schemas=\{resource\.state\.kind === 'ready' \? resource\.state\.value : undefined\}/);
  assert.match(pricing, /onToggle=\{\(event\) => \{ if \(event\.currentTarget\.open\) onRequestSchemas\?\.\(\); \}\}/);
});

test('pricing usage is scope-only and independent from currency price requests', () => {
  const priceLoad = pricing.slice(pricing.indexOf('const load ='), pricing.indexOf('useEffect('));
  assert.doesNotMatch(priceLoad, /usage-summary|setUsage/);
  assert.match(pricing, /token && basePricingScope === scope/);
  assert.match(pricing, /usage-summary[\s\S]*?return \(\) => controller\.abort\(\);\s*\}, \[token, tenant, basePricingScope\]\)/);
  assert.match(priceLoad, /loadModelPricePages\(/);
  assert.match(priceLoad, /\(value\) => \{ if \(current\(\)\) \{ setPrices\(value\)/);
  assert.match(priceLoad, /\.then\(\(value\) => \{ if \(current\(\)\) setGenerationPrices\(value\)/);
});

test('pricing cancels superseded reads, bounds waits and memoizes indexed rows', () => {
  assert.match(pricing, /priceRequest\.current\?\.abort\(\)/);
  assert.match(pricing, /AbortSignal\.timeout\(10_000\)/);
  assert.match(pricing, /scopeRef\.current\.displayCurrency === requestedCurrency/);
  assert.match(pricing, /const rows = useMemo\(/);
  assert.match(pricing, /pricesByModel\.get\(name\)/);
  assert.doesNotMatch(pricing, /prices\.find\(/);
  assert.match(pricing, /\}, \[prices, usage\]\)/);
});

test('manual pricing offers known models without restricting free-form names or inventing attribution', () => {
  assert.match(pricing, /<ModelPicker label=\{t\('pricing\.model'\)\} value=\{model\} onChange=\{setModel\} options=\{modelOptions\} editable \/>/);
  assert.match(pricing, /provider: t\('sessionReplay\.unknown'\), upstream: t\('sessionReplay\.unknown'\)/);
  assert.doesNotMatch(pricing, /\/internal\/v1\/upstreams|\/internal\/v1\/model-routes/);
});
