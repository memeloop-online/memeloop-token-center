import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = (path: string) => readFile(new URL(path, import.meta.url), 'utf8');
const [management, shell, pages, plugins, hook] = await Promise.all([
  read('../src/operator/pages/ManagementPages.tsx'), read('../src/operator/Operator.tsx'),
  read('../src/operator/pages/OperatorPages.tsx'), read('../src/operator/Plugins.tsx'),
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

test('one credential-scoped plugin catalog feeds shell registration and the plugin page', () => {
  assert.equal((shell.match(/\/internal\/v1\/plugins'/g) ?? []).length, 1);
  assert.match(shell, /scope\.activeCredential,\s*\(signal\)/);
  assert.match(shell, /catalog=\{pluginCatalog\.state\}/);
  const pluginPage = pages.slice(pages.indexOf('export function PluginsPage'));
  assert.doesNotMatch(pluginPage, /\/internal\/v1\/plugins/);
  assert.match(pluginPage, /catalog\.scopeKey !== token/);
});

test('plugin configuration reads are explicit on-demand single-flight and scope-fenced', () => {
  assert.doesNotMatch(plugins, /Promise\.all|configurable\.map/);
  assert.match(plugins, /if \(!token \|\| pending\.current \|\| \(!force && configuration\)\) return/);
  assert.match(plugins, /onToggle=.*event\.currentTarget\.open/);
  assert.match(plugins, /pending\.current\?\.abort\(\)/);
  assert.match(plugins, /key=\{`\$\{token\}\\0\$\{tenant\}\\0\$\{writeTenant\}\\0\$\{plugin\.id\}`\}/);
  assert.match(plugins, /if \(writeTenant === tenant\) setConfiguration\(saved\)/);
});

test('resource lifecycle aborts superseded reads and never exposes stale scope data', () => {
  assert.match(hook, /load: \(signal: AbortSignal\) => Promise<T>/);
  assert.match(hook, /controller\.current\?\.abort\(\)/);
  assert.match(hook, /loadRef\.current\(current\.signal\)/);
  assert.match(hook, /!current\.signal\.aborted && request === sequence\.current/);
  assert.match(hook, /state\.scopeKey === scopeKey/);
});
