import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = (path: string) => readFile(new URL(path, import.meta.url), 'utf8');
const [management, hook] = await Promise.all([
  read('../src/operator/pages/ManagementPages.tsx'),
  read('../src/operator/hooks/useOperatorResource.ts'),
]);

test('provider list readiness is independent of bounded account statistics', () => {
  const providers = management.slice(management.indexOf('export function ProvidersPage'), management.indexOf('export function PricingPage'));
  const basic = providers.slice(0, providers.indexOf('const statistics ='));
  assert.match(basic, /return \{ providers, values \}/);
  assert.doesNotMatch(basic, /recentAvailabilityPath|upstreamAvailabilityPath/);
  assert.match(providers, /const statistics = useOperatorResource/);
  assert.match(providers, /resource=\{resource\.state\}/);
  assert.equal((providers.match(/AbortSignal\.any\(\[signal, AbortSignal\.timeout/g) ?? []).length, 4);
});

test('resource lifecycle aborts superseded reads and never exposes stale scope data', () => {
  assert.match(hook, /load: \(signal: AbortSignal\) => Promise<T>/);
  assert.match(hook, /controller\.current\?\.abort\(\)/);
  assert.match(hook, /loadRef\.current\(current\.signal\)/);
  assert.match(hook, /!current\.signal\.aborted && request === sequence\.current/);
  assert.match(hook, /state\.scopeKey === scopeKey/);
});
