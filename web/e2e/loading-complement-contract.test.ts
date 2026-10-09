import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = (path: string) => readFile(new URL(path, import.meta.url), 'utf8');
const [overview, plugins, requests, sessions, portal, main, selfRequests, selfUsage, selfOverview, selfGenerations, selfGenerate] = await Promise.all([
  read('../src/operator/pages/OperatorPages.tsx'),
  read('../src/operator/pages/PluginsPage.tsx'),
  read('../src/operator/pages/RequestsPage.tsx'),
  read('../src/operator/pages/SessionsPage.tsx'),
  read('../src/self/SelfPortal.tsx'),
  read('../src/main.tsx'),
  read('../src/self/RequestsPage.tsx'),
  read('../src/self/UsagePage.tsx'),
  read('../src/self/OverviewPage.tsx'),
  read('../src/self/GenerationsPage.tsx'),
  read('../src/self/GeneratePage.tsx'),
]);

test('page loading regions retain complete authority and route scopes without DOM credentials', () => {
  assert.ok(overview.includes('scopeKey={`${token}\\0${tenant}\\0overview`}'));
  assert.ok(overview.includes('scopeKey={`${token}\\0${tenant}\\0usage`}'));
  assert.ok(plugins.includes('scopeKey={`${token}\\0${tenant}\\0${writeTenant}\\0plugins`}'));
  assert.ok(requests.includes('scopeKey={`${token}\\0${tenant}\\0${writeTenant}\\0requests`}'));
  assert.ok(sessions.includes('scopeKey={`${scopeKey}\\0sessions`}'));
  assert.ok(sessions.includes('const scopeKey = `${tenant}\\0${token}`'));
  assert.ok(portal.includes('scopeKey={`${credential}\\0${credentialScopeKey}\\0${activeRoute}`}'));
  for (const source of [overview, plugins, requests, sessions, portal]) {
    assert.match(source, /import \{[^}]*LoadingState[^}]*PageLoadingRegion[^}]*\} from/);
    assert.doesNotMatch(source, /data-[\w-]+=\{[^}]*\b(?:token|credential|scopeKey)\b/);
  }
});

test('overview prerequisites and optional metadata do not share a request barrier', () => {
  const monitoring = overview.slice(overview.indexOf('function OverviewMonitoringSection'), overview.indexOf('function OverviewRecentRequestsSection'));
  const recent = overview.slice(overview.indexOf('function OverviewRecentRequestsSection'), overview.indexOf('export function OverviewPage'));
  const usage = overview.slice(overview.indexOf('export function UsagePage'), overview.indexOf('export function GenerationsPage'));
  assert.match(monitoring, /<LoadingState[^>]*level="page"/);
  assert.match(monitoring, /<LoadingProgress active=\{state\.refreshing === true\}/);
  assert.match(recent, /<LoadingState label=\{t\('common.loading'\)\} \/>/);
  assert.match(usage, /<LoadingState[^>]*variant="compact"/);
  assert.doesNotMatch(recent + usage, /level="page"|Promise\.all/);
  assert.match(overview, /state\.disabled/);
  assert.match(overview, /state\.refreshError/);
  assert.match(usage, /<UsageAnalysis token=\{token\} tenant=\{tenant\}/);
});

test('request initial loading and pagination progress preserve filters, cancellation and busy buttons', () => {
  assert.match(requests, /loading && requests\.length === 0 \? <LoadingState[^>]*level="page"/);
  assert.match(requests, /<LoadingProgress active=\{loading && requests\.length > 0\}/);
  assert.match(requests, /className="panel request-page-surface" aria-busy=\{loading\}/);
  assert.match(requests, /typedRequestQueryBody\(tenant, nextFilters, older \? before : undefined\)/);
  assert.match(requests, /controller\.signal\.aborted \|\| request !== sequence\.current/);
  assert.match(requests, /latest\.token !== currentScope\.token \|\| latest\.tenant !== currentScope\.tenant/);
  assert.match(requests, /detailResult\?\.token === token && detailResult\.tenant === tenant/);
  assert.match(requests, /disabled=\{loading\} onClick=\{onLoadOlder\}>\{loading \? t\('common.loading'\) : t\('traffic.loadOlder'\)\}/);
});

test('plugin loading keeps principal capability checks separate from selected tenant and catalog refresh', () => {
  assert.match(plugins, /Boolean\(token\), token,/);
  assert.match(plugins, /plugins\/runtime-access/);
  assert.match(plugins, /access\.state\.scopeKey !== token \? null/);
  assert.match(plugins, /access\.state\.value\.can_view_runtime/);
  assert.match(plugins, /canManage=\{access\.state\.value\.can_manage_runtime\}/);
  assert.match(plugins, /catalog\.scopeKey !== token \|\| catalog\.kind === 'idle' \|\| catalog\.kind === 'loading'/);
  assert.match(plugins, /catalog\.disabled/);
  assert.match(plugins, /busy=\{refreshing\}/);
  assert.match(plugins, /refreshError=\{catalog\.refreshError\}/);
  assert.match(plugins, /onClick=\{\(\) => void reloadCatalog\(\)\}/);
});

test('session detail loading remains optional with existing scope and latest-request guards', () => {
  const pending = sessions.slice(sessions.indexOf("scopedSelection.phase === 'loading'"), sessions.indexOf("scopedSelection.phase === 'failed'"));
  assert.match(pending, /<LoadingState label=\{t\('common.loading'\)\} variant="detail" \/>/);
  assert.doesNotMatch(pending, /level="page"|aria-live|Spinner/);
  assert.match(sessions, /requestDetailSelectionInScope\(requestSelection, scopeKey\)/);
  assert.match(sessions, /pending\.isCurrent\(\)/);
  assert.match(sessions, /signal: pending\.signal/);
  assert.match(sessions, /onClick=\{retryRequestDetail\}/);
});

test('portal lazy and authentication loading preserve authority reset and button feedback', () => {
  assert.match(portal, /busy=\{authenticating && !credentialView\}/);
  assert.match(portal, /<Suspense key=\{credentialScopeKey\} fallback=\{<LoadingState[^>]*level="page"/);
  assert.match(portal, /\{authenticating \? t\('common.loading'\) : t\('common.load'\)\}/);
  assert.match(portal, /sequence !== authSequence\.current \|\| controller\.signal\.aborted/);
  assert.match(portal, /expectedScope === credentialScopeRef\.current/);
  assert.match(portal, /credentialView\.key_id.*credentialView\.credential_generation.*credentialScopeGeneration/);
});

test('root and self-page initial loading reuse the shared state without adding page regions', () => {
  for (const source of [main, selfRequests, selfUsage, selfOverview, selfGenerations, selfGenerate]) {
    assert.match(source, /<LoadingState[^>]*level="page"/);
    assert.doesNotMatch(source, /PageLoadingRegion/);
    assert.doesNotMatch(source, /<div className="(?:boot|empty)"(?: role="status")?>\{t\('common.loading'\)\}/);
  }
  assert.match(selfUsage, /<Suspense fallback=\{<LoadingState[^>]*level="section"[^>]*variant="detail"/);
  assert.match(selfUsage, /scopedRemote\.status === 'error'/);
  assert.match(selfUsage, /if \(!stats\) return <div className="empty">\{t\('common.noData'\)\}/);
});

test('self loading migration preserves button busy text and generation safety', () => {
  assert.match(selfRequests, /\{loading \? t\('common.loading'\) : t\('traffic.applyFilters'\)\}/);
  assert.match(selfRequests, /\{loading \? t\('common.loading'\) : t\('traffic.loadOlder'\)\}/);
  assert.match(selfRequests, /sequence !== requestSequence\.current \|\| controller\.signal\.aborted/);
  assert.match(selfGenerations, /\{loading \? t\('common.loading'\) : t\('self.refreshGenerations'\)\}/);
  assert.match(selfGenerations, /cancellingIds\.has\(job\.job_id\) \? t\('common.loading'\) : t\('self.cancelGeneration'\)/);
  assert.match(selfGenerations, /scope === scopeGeneration\.current && current === refreshSequence\.current/);
  assert.match(selfGenerate, /\{submitting \? t\('common.loading'\) : t\('self.submitGeneration'\)\}/);
  assert.match(selfGenerate, /current !== submitSequence\.current \|\| controller\.signal\.aborted/);
  assert.match(selfGenerate, /'Idempotency-Key': crypto\.randomUUID\(\)/);
});
