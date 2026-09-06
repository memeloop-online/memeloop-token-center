import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';
import { tenantForCredential } from '../src/credentialStorage.js';
import { operatorRouteKeys } from '../src/operator/scope/operatorRoutes.js';

const operator = readFileSync(new URL('../src/operator/Operator.tsx', import.meta.url), 'utf8');
const scope = readFileSync(new URL('../src/operator/hooks/useOperatorScope.ts', import.meta.url), 'utf8');
const requestsPage = readFileSync(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
const managementPages = readFileSync(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
const resourceHook = readFileSync(new URL('../src/operator/hooks/useOperatorResource.ts', import.meta.url), 'utf8');

test('operator exposes controlled AppShell routing without coupling credentials to the URL', () => {
  assert.match(operator, /route\?: OperatorRouteKey/);
  assert.match(operator, /onRouteChange\?: \(route: OperatorRouteKey\)/);
  assert.match(operator, /embedded\?: boolean/);
  assert.match(operator, /showNavigation\?: boolean/);
  assert.doesNotMatch(operator, /URLSearchParams|location\.|history\./);
});

test('sessions are a first-class operator route and all page keys are explicit', () => {
  assert.deepEqual(operatorRouteKeys, [
    'overview', 'requests', 'sessions', 'usage', 'generations', 'providers', 'routes',
    'pricing', 'credentials', 'service-credentials', 'plugins',
  ]);
  assert.match(operator, /case 'sessions': page = <SessionsPage/);
  assert.doesNotMatch(operator, /trafficMode|onModeChange/);
});

test('credential authentication discovers only tenants before a page mounts', () => {
  assert.match(scope, /api<TenantView\[]>\('\/internal\/v1\/tenants'/);
  assert.doesNotMatch(scope, /provider-types|plugins|upstreams|requests|schemas/);
});

test('operator restores an allowed tenant and otherwise opens the production default', () => {
  const tenants = [{ external_id: 'archive' }, { external_id: 'default' }];
  assert.equal(tenantForCredential(tenants, 'archive'), 'archive');
  assert.equal(tenantForCredential(tenants, 'missing'), 'default');
  assert.equal(tenantForCredential([{ external_id: 'only' }], ''), 'only');
  assert.equal(tenantForCredential([{ external_id: 'one' }, { external_id: 'two' }], ''), '');
});

test('credential review defaults to active and all-tenant routes remain visible', () => {
  assert.match(managementPages, /useState\('active'\)/);
  assert.match(managementPages, /if \(!loadToken\) \{ setRoutes\(\[\]\); setCredentials\(\[\]\); return; \}/);
  assert.doesNotMatch(managementPages, /if \(!loadToken \|\| !loadTenant\) \{ setRoutes\(\[\]\); setCredentials\(\[\]\); return; \}/);
});

test('request filters hide stale rows and cursors while a replacement query is pending or fails', () => {
  assert.match(requestsPage, /if \(!older\) \{ setRequests\(\[\]\); setHasOlder\(false\); setDetail\(undefined\); \}/);
  assert.match(requestsPage, /setUpstreamError\(messageOf/);
  assert.match(requestsPage, /select disabled=\{!upstreamsAvailable\}/);
});

test('pricing sync and resource refreshes retain only current operation results', () => {
  assert.match(managementPages, /const syncSequence = useRef\(0\)/);
  assert.match(managementPages, /sequence !== syncSequence\.current/);
  assert.match(managementPages, /syncSequence\.current \+= 1; setSyncing\(false\)/);
  assert.match(resourceHook, /refreshError\?: string/);
  assert.match(resourceHook, /refreshError: action\.message/);
});
