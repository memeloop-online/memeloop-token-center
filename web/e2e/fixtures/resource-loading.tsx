import { StrictMode, useEffect, useRef, useState } from 'react';
import { createRoot } from 'react-dom/client';
import { api } from '../../src/api';
import { I18nProvider, useI18n } from '../../src/i18n';
import { Button, LoadingState, MtcFluentProvider, PageLoadingRegion } from '../../src/design-system';
import { ResourceBoundary } from '../../src/operator/ResourceBoundary';
import { Plugins } from '../../src/operator/Plugins';
import { useOperatorResource } from '../../src/operator/hooks/useOperatorResource';
import type { PluginManifest } from '../../src/types';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const manifests: PluginManifest[] = ['first', 'second', 'third'].map((id) => ({
  id, version: '1.0.0', wit_version: '1', capabilities: [],
  contributions: { configuration: { default: {}, schema: { type: 'object', properties: { mode: { type: 'string', title: 'Mode' } } } } },
}));

function Fixture() {
  const [tenant, setTenant] = useState('alpha');
  const [show, setShow] = useState(true);
  const resource = useOperatorResource(true, tenant, (signal) => api<{ tenant: string }>(`/fixture/resource?tenant=${tenant}`, 'fixture-service', { signal }), 'Failed');
  return <main>
    <button onClick={() => setTenant('beta')}>Switch tenant</button>
    <button onClick={() => setShow((value) => !value)}>Toggle plugins</button>
    <output>{resource.state.kind === 'ready' ? resource.state.value.tenant : resource.state.kind}</output>
    {show && <Plugins token="fixture-service" tenant={tenant} values={manifests} />}
  </main>;
}
interface FixtureResource {
  tenant: string;
  credential: string;
  records: string[];
}

function UnifiedFixture() {
  const { t, locale } = useI18n();
  const copy = locale === 'zh-CN' ? {
    title: '资源工作区', tenant: '切换租户', credential: '切换凭据', disable: '停用读取权限', restore: '恢复读取权限',
    refresh: '刷新数据', fail: '模拟刷新失败', empty: '读取空结果', primary: '主要资源', secondary: '统计信息', tertiary: '历史记录',
    noMatches: '没有符合条件的资源', draft: '工作区草稿', action: '可用操作',
  } : {
    title: 'Resource workspace', tenant: 'Switch tenant', credential: 'Switch credential', disable: 'Disable read access', restore: 'Restore read access',
    refresh: 'Refresh data', fail: 'Simulate refresh failure', empty: 'Read empty results', primary: 'Primary resources', secondary: 'Statistics', tertiary: 'History',
    noMatches: 'No matching resources', draft: 'Workspace draft', action: 'Available action',
  };
  const [tenant, setTenant] = useState('alpha');
  const [credential, setCredential] = useState('one');
  const [enabled, setEnabled] = useState(true);
  const [refresh, setRefresh] = useState({ revision: 0, outcome: 'ready' });
  const scopeKey = `${tenant}\0${credential}`;
  const secondaryLevel = new URLSearchParams(location.search).get('secondary-page') === '1' ? 'page' : 'section';
  const read = (resource: string, signal: AbortSignal) => api<FixtureResource>(`/fixture/loading?${new URLSearchParams({ tenant, credential, resource, revision: String(refresh.revision), outcome: resource === 'primary' ? refresh.outcome : 'ready' })}`, 'fixture-service', { signal });
  const primary = useOperatorResource(enabled, scopeKey, (signal) => read('primary', signal), t('common.requestFailed'));
  const secondary = useOperatorResource(enabled, scopeKey, (signal) => read('secondary', signal), t('common.requestFailed'));
  const tertiary = useOperatorResource(enabled, scopeKey, (signal) => read('tertiary', signal), t('common.requestFailed'));
  const [actions, setActions] = useState(0);
  const reload = useRef(primary.reload);
  reload.current = primary.reload;
  useEffect(() => {
    if (refresh.revision > 0) void reload.current();
  }, [refresh.revision]);
  return <main style={{ maxWidth: 1200, margin: '0 auto', padding: 16 }}>
    <h1>{copy.title}</h1>
    <div style={{ display: 'flex', flexWrap: 'wrap', gap: 8, marginBottom: 16 }}>
      <Button data-fixture-action="tenant" onClick={() => setTenant((current) => current === 'alpha' ? 'beta' : 'alpha')}>{copy.tenant}</Button>
      <Button data-fixture-action="credential" onClick={() => setCredential((current) => current === 'one' ? 'two' : 'one')}>{copy.credential}</Button>
      <Button data-fixture-action="access" onClick={() => setEnabled((current) => !current)}>{enabled ? copy.disable : copy.restore}</Button>
      <Button data-fixture-action="refresh" onClick={() => setRefresh((current) => ({ revision: current.revision + 1, outcome: 'ready' }))}>{copy.refresh}</Button>
      <Button data-fixture-action="fail" onClick={() => setRefresh((current) => ({ revision: current.revision + 1, outcome: 'failed' }))}>{copy.fail}</Button>
      <Button data-fixture-action="empty" onClick={() => setRefresh((current) => ({ revision: current.revision + 1, outcome: 'empty' }))}>{copy.empty}</Button>
    </div>
    <span data-resource-state>{primary.state.kind === 'ready' ? `${primary.state.value.tenant}-${primary.state.value.credential}` : primary.state.kind}</span>
    <PageLoadingRegion scopeKey={scopeKey} label={t('common.loading')}>
      <div style={{ display: 'grid', gap: 16, gridTemplateColumns: 'repeat(auto-fit, minmax(min(100%, 280px), 1fr))' }}>
        <section data-loading-surface="primary"><h2>{copy.primary}</h2>
          <ResourceBoundary resource={primary.state} scopeKey={scopeKey} onRetry={() => void primary.reload()}>{(value) => <div data-ready-resource="primary" data-ready-scope={`${value.tenant}-${value.credential}`}>
            <p>{value.records.length ? value.records.join(', ') : copy.noMatches}</p>
            <label>{copy.draft}<input data-workspace-draft aria-label={copy.draft} defaultValue="" /></label>
            <Button data-fixture-action="available" onClick={() => setActions((current) => current + 1)}>{copy.action}</Button><span data-action-count>{actions}</span>
          </div>}</ResourceBoundary>
        </section>
        <PageLoadingRegion scopeKey={scopeKey} label={t('common.loading')}>
          <section data-loading-surface="secondary"><h2>{copy.secondary}</h2>
            <ResourceBoundary resource={secondary.state} scopeKey={scopeKey} level={secondaryLevel}>{(value) => <div data-ready-resource="secondary" data-ready-scope={`${value.tenant}-${value.credential}`}>{value.records.join(', ')}</div>}</ResourceBoundary>
          </section>
        </PageLoadingRegion>
        <section data-loading-surface="tertiary"><h2>{copy.tertiary}</h2>
          <ResourceBoundary resource={tertiary.state} scopeKey={scopeKey} level="section">{(value) => <div data-ready-resource="tertiary" data-ready-scope={`${value.tenant}-${value.credential}`}>{value.records.join(', ')}</div>}</ResourceBoundary>
        </section>
      </div>
    </PageLoadingRegion>
  </main>;
}

const unified = new URLSearchParams(location.search).get('mode') === 'unified';
const inline = new URLSearchParams(location.search).get('mode') === 'inline';
function InlineFixture() {
  const { t } = useI18n();
  return <main style={{ padding: 16 }}><div data-inline-column style={{ width: 'min(100%, 280px)' }}><LoadingState label={t('common.loading')} variant="inline" /></div></main>;
}
if (unified && new URLSearchParams(location.search).get('ignore-abort') === '1') {
  const fetch = globalThis.fetch.bind(globalThis);
  globalThis.fetch = (input, init) => fetch(input, { ...init, signal: undefined });
}
createRoot(document.getElementById('root')!).render(<I18nProvider>{inline
  ? <MtcFluentProvider><InlineFixture /></MtcFluentProvider>
  : unified
  ? <StrictMode><MtcFluentProvider><UnifiedFixture /></MtcFluentProvider></StrictMode>
  : <Fixture />}</I18nProvider>);
