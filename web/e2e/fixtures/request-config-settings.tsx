import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { I18nProvider, useI18n } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system/MtcFluentProvider';
import { SystemSettingsPage } from '../../src/operator/pages/SystemSettingsPage';
import { requestConfigFields, requestConfigPurposes, type HttpSettings, type RequestConfigSnapshot } from '../../src/operator/requestConfig';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

function withEffective(revision: number, settings: HttpSettings): RequestConfigSnapshot {
  const effective = {} as RequestConfigSnapshot['effective'];
  for (const purpose of requestConfigPurposes) {
    effective[purpose] = {} as RequestConfigSnapshot['effective'][typeof purpose];
    for (const field of requestConfigFields) {
      const overrides = settings.purposes[purpose] ?? {};
      const source = field.key in overrides ? 'purpose_override' : field.key in settings.defaults ? 'global_default' : 'source_default';
      const value = source === 'purpose_override' ? overrides[field.key] : source === 'global_default' ? settings.defaults[field.key] : field.nullable ? null : 8;
      effective[purpose][field.key] = { value: value ?? null, source, unit: field.unit };
    }
  }
  return { revision, settings, effective };
}

const control = {
  failLoad: false, unavailable: false, forbidden: false, delaySave: false, failSave: false, conflict: false, reads: 0, writes: 0,
  lastBody: null as { expected_revision: number; settings: HttpSettings } | null,
  releaseSave: () => {},
  state: withEffective(7, {
    defaults: { connect_timeout_ms: 30000, read_timeout_ms: 15000, future_default: { retain: true } } as HttpSettings['defaults'],
    purposes: {
      codex: { connect_timeout_ms: 60000, future_purpose_field: 'retain' } as HttpSettings['defaults'],
      oauth: { request_timeout_ms: 90000 },
      future_purpose: { request_timeout_ms: 1200 },
    } as HttpSettings['purposes'],
  }),
};
Object.assign(window, { requestConfigFixture: control });

globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url, location.origin);
  if (url.pathname === '/internal/v1/system-settings/http') {
    if (url.search) throw new Error('Global settings must not include a tenant query');
    if (control.forbidden) return Response.json({ error: { message: 'Forbidden' } }, { status: 403 });
    if (init?.method === 'PUT') {
      control.writes += 1;
      if (control.delaySave) await new Promise<void>((resolve) => { control.releaseSave = resolve; });
      if (control.failSave) return Response.json({ error: { message: 'Save rejected' } }, { status: 503 });
      const body = JSON.parse(String(init.body)) as { expected_revision: number; settings: HttpSettings };
      control.lastBody = body;
      if (control.conflict || body.expected_revision !== control.state.revision) return Response.json({ error: { message: 'Changed elsewhere' } }, { status: 409 });
      control.state = withEffective(control.state.revision + 1, body.settings);
      return Response.json(control.state);
    }
    if (control.failLoad) return Response.json({ error: { message: 'Store unavailable' } }, { status: 503 });
    if (control.unavailable) return Response.json({ error: { message: 'Unknown route' } }, { status: 404 });
    control.reads += 1;
    return Response.json(control.state);
  }
  if (url.pathname.endsWith('/model-picker-options')) return Response.json({ contract_version: 'model_picker_projection_v1', generated_at: Date.now(), data: [], next_cursor: null });
  if (url.pathname.endsWith('/filter-assistant/settings')) return Response.json(null);
  return Response.json({ error: { message: 'Unknown route' } }, { status: 404 });
};

function Fixture() {
  const { locale, setLocale } = useI18n();
  const [generation, setGeneration] = useState(0);
  const [tenant, setTenant] = useState('tenant');
  return <main style={{ padding: 12 }}>
    <button type="button" onClick={() => setLocale(locale === 'en' ? 'zh-CN' : 'en')}>Switch language</button>
    <button type="button" onClick={() => setGeneration((previous) => previous + 1)}>Reload settings</button>
    <button type="button" onClick={() => setTenant(tenant ? '' : 'tenant')}>Switch tenant</button>
    <SystemSettingsPage token={`fixture-${generation}`} tenant={tenant} />
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
