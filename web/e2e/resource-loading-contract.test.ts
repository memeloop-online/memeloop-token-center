import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const read = (path: string) => readFile(new URL(path, import.meta.url), 'utf8');
const [management, hook, loading, boundary, styles] = await Promise.all([
  read('../src/operator/pages/ManagementPages.tsx'),
  read('../src/operator/hooks/useOperatorResource.ts'),
  read('../src/design-system/loading.tsx'),
  read('../src/operator/ResourceBoundary.tsx'),
  read('../src/design-system/loading.css'),
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

test('shared initial and refresh presentations use Fluent skeletons and one scoped page announcement', () => {
  assert.match(loading, /import \{ ProgressBar, Skeleton, SkeletonItem \} from '@fluentui\/react-components'/);
  assert.match(loading, /data-page-loading-announcement role="status" aria-live="polite" aria-atomic="true"/);
  assert.match(loading, /NestedLoadingRegion key=\{scopeKey\}/);
  assert.match(loading, /PageLoadingRoot key=\{scopeKey\}/);
  assert.match(loading, /active && level === 'page'/);
  assert.match(loading, /Skeleton animation="pulse" aria-hidden="true"/);
  assert.doesNotMatch(loading, /data-(?:loading-)?scope|100vh|position: ?fixed/);
  assert.match(styles, /\.mtc-page-loading-announcement[\s\S]*block-size: 32px/);
  assert.match(styles, /prefers-reduced-motion: reduce/);
});

test('disabled or mismatched scope never exposes prior data while same-scope refresh retains its mounted workspace', () => {
  assert.match(hook, /refreshing: true, refreshError: undefined/);
  assert.match(hook, /refreshing: false, refreshError: action.message/);
  assert.match(hook, /const visibleState: ResourceState<T> = !enabled/);
  assert.match(boundary, /const matchesScope = resource.scopeKey === scopeKey/);
  assert.match(boundary, /matchesScope && resource.kind === 'ready'/);
  assert.match(boundary, /resource.refreshing === true/);
  assert.match(boundary, /resource.disabled === true/);
  assert.match(boundary, /children\(resource.value\)/);
  assert.doesNotMatch(boundary, /previous\.current|if \(!value\)/);
  assert.match(boundary, /role="alert"/);
  assert.match(management, /import \{ ResourceBoundary \} from '..\/ResourceBoundary'/);
  assert.doesNotMatch(management, /function ResourceBoundary/);
});

test('local read surfaces share skeletons without adding loading live regions or changing read policy', async () => {
  for (const path of ['SessionViews.tsx', 'ModelPicker.tsx', 'ArchiveContentReader.tsx', 'operator/OverviewTrends.tsx', 'operator/UpstreamAvailability.tsx', 'operator/UpstreamQuotaPanel.tsx']) {
    const source = await read(`../src/${path}`);
    assert.match(source, /LoadingState/);
    assert.doesNotMatch(source, /<[^>]*role="status"[^>]*>\{t\('common.loading'\)\}/);
  }
  const archive = await read('../src/ArchiveContentReader.tsx');
  assert.match(archive, /sessionReplay.readBytes/);
  assert.match(archive, /controller.current\?\.abort\(\)/);
  const quota = await read('../src/operator/UpstreamQuotaPanel.tsx');
  assert.match(quota, /UPSTREAM_QUOTA_READ_TIMEOUT_MILLIS/);
  assert.match(quota, /disabled=\{busy \|\| !tenant \|\| refreshDisabled\}/);
});

test('resource lifecycle aborts superseded reads and never exposes stale scope data', () => {
  assert.match(hook, /load: \(signal: AbortSignal\) => Promise<T>/);
  assert.match(hook, /controller\.current\?\.abort\(\)/);
  assert.match(hook, /loadRef\.current\(current\.signal\)/);
  assert.match(hook, /!current\.signal\.aborted && request === sequence\.current/);
  assert.match(hook, /state\.scopeKey === scopeKey/);
});

test('loading evidence is deterministic, isolated and bound to exact integrated source', async () => {
  const browser = await read('./unified-loading-browser-contract.test.ts');
  assert.match(browser, /width: 1440, height: 1000/);
  assert.match(browser, /width: 390, height: 844/);
  assert.match(browser, /const locales = \['en', 'zh-CN'\]/);
  assert.match(browser, /deviceScaleFactor: 1, reducedMotion: 'reduce'/);
  assert.match(browser, /page\.clock\.setFixedTime/);
  assert.match(browser, /document\.fonts\.ready/);
  assert.match(browser, /page\.mouse\.move\(0, 0\)/);
  assert.match(browser, /e2e-artifacts\/ui-system\/loading\//);
  assert.match(browser, /integrated_head_sha: integrated/);
  assert.match(browser, /process\.env\.GITHUB_SHA/);
  assert.match(browser, /owner_heads:/);
  assert.match(browser, /applied_source_commits: commits/);
  assert.match(browser, /file_sha256: createHash\('sha256'\)\.update\(bytes\)/);
  assert.match(browser, /generation_status: 'missing'/);
  assert.match(browser, /finally \{\s*await saveManifest\(\)/);
  assert.match(browser, /measured\.button_height_delta > 1/);
  assert.match(browser, /button\.width >= 44 && button\.height >= 44/);
  assert.match(browser, /assert\.deepEqual\(geometryFailures, \[\]/);
  for (const scenario of ['ready', 'empty', 'error', 'permission-failure', 'initial-loading', 'background-refresh']) {
    assert.ok(browser.includes(`await screenshot('${scenario}'`), `${scenario} captures completed contracts, not only failures`);
  }
});

test('last owned operator loading surfaces reuse skeletons without changing mutation feedback', async () => {
  for (const path of ['GenerationWorkspace.tsx', 'UsageAnalysis.tsx', 'TenantManager.tsx', 'TypedFilterBuilder.tsx', 'pages/SystemSettingsPage.tsx']) {
    const source = await read(`../src/operator/${path}`);
    assert.match(source, /LoadingState/);
    assert.doesNotMatch(source, /<[^>]*(?:className="(?:empty|notice|muted)"|role="status")[^>]*>\{t\('common.loading'\)\}<\//);
  }
  assert.match(loading, /variant !== 'inline'/);
  assert.match(loading, /variant === 'inline' \? 1/);
  assert.match(styles, /\.mtc-loading-inline[\s\S]*min-block-size: 20px/);
  const tenants = await read('../src/operator/TenantManager.tsx');
  assert.match(tenants, /busy \? t\('common.loading'\) : dialogAction/);
  const settings = await read('../src/operator/pages/SystemSettingsPage.tsx');
  assert.match(settings, /billingLoading && <LoadingState[^>]*variant="inline"/);
  assert.match(settings, /saving \? t\('common.loading'\) : t\('common.save'\)/);
});
