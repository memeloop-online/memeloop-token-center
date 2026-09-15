import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const pageSource = await readFile(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
const providersSource = pageSource.slice(
  pageSource.indexOf('export function ProvidersPage'),
  pageSource.indexOf('export function PricingPage'),
);

test('Providers defers non-critical statistics until its account directory is ready', () => {
  assert.match(
    providersSource,
    /const statistics = useOperatorResource\(\s*Boolean\(token\) && resource\.state\.kind === 'ready'/,
  );
  assert.match(providersSource, /api<OperatorMonitoringSnapshot>\(recentAvailabilityPath/);
  assert.match(providersSource, /api<UpstreamAvailabilityWindow>\(upstreamAvailabilityPath/);
  assert.match(
    providersSource,
    /const \[providers, values\] = await Promise\.all\(\[[\s\S]*\/internal\/v1\/provider-types[\s\S]*\/internal\/v1\/upstreams/,
  );
});
