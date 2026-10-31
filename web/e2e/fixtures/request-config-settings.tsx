import { createRoot } from 'react-dom/client';
import { I18nProvider, useI18n } from '../../src/i18n';
import { MtcFluentProvider } from '../../src/design-system/MtcFluentProvider';
import { SystemSettingsPage } from '../../src/operator/pages/SystemSettingsPage';
import { requestConfigFields, requestConfigPurposes, type RequestConfigSnapshot } from '../../src/operator/requestConfig';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

// Fixture-side implementation of the draft request-config contract.
function withEffective(state: RequestConfigSnapshot): RequestConfigSnapshot {
  const purposes: RequestConfigSnapshot['purposes'] = {};
  for (const purpose of requestConfigPurposes) {
    const overrides = state.purposes[purpose]?.overrides ?? {};
    const effective: Record<string, { value: number | null; source: 'default' | 'override' }> = {};
    for (const field of requestConfigFields) {
      effective[field.key] = field.key in overrides
        ? { value: overrides[field.key], source: 'override' }
        : { value: state.defaults[field.key] ?? null, source: 'default' };
    }
    purposes[purpose] = { overrides: { ...overrides }, effective };
  }
  return { ...state, purposes };
}

const control = {
  failLoad: false, unavailable: false, delaySave: false, failSave: false, writes: 0,
  releaseSave: () => {},
  state: withEffective({
    contract_version: 'request_config_v1',
    defaults: { timeout_ms: 30_000, stream_idle_timeout_ms: 15_000, max_request_bytes: null },
    purposes: { text: { overrides: { timeout_ms: 60_000 }, effective: {} } },
    updated_at: 7,
  }),
};
Object.assign(window, { requestConfigFixture: control });

globalThis.fetch = async (input: RequestInfo | URL, init?: RequestInit) => {
  const url = new URL(typeof input === 'string' ? input : input instanceof URL ? input.toString() : input.url, location.origin);
  if (url.pathname.endsWith('/request-config/settings')) {
    if (init?.method === 'PUT') {
      control.writes += 1;
      if (control.delaySave) await new Promise<void>((resolve) => { control.releaseSave = resolve; });
      if (control.failSave) return Response.json({ error: { message: 'Save rejected' } }, { status: 503 });
      const body = JSON.parse(String(init.body)) as { scope: 'default' | 'purpose'; purpose?: string; values: Record<string, number | null>; expected_updated_at: number | null };
      if (body.expected_updated_at !== control.state.updated_at) return Response.json({ error: { code: 'conflict', message: 'Request configuration changed elsewhere' } }, { status: 409 });
      if (body.scope === 'default') {
        for (const field of requestConfigFields) if (field.key in body.values) control.state.defaults[field.key] = body.values[field.key];
      } else if (body.purpose) {
        const entry = control.state.purposes[body.purpose] ?? { overrides: {}, effective: {} };
        for (const field of requestConfigFields) {
          if (!(field.key in body.values)) continue;
          if (body.values[field.key] === null) delete entry.overrides[field.key];
          else entry.overrides[field.key] = body.values[field.key] as number;
        }
        control.state.purposes[body.purpose] = entry;
      }
      control.state = withEffective({ ...control.state, updated_at: control.state.updated_at + 1 });
      return Response.json(control.state);
    }
    if (control.failLoad) return Response.json({ error: { message: 'Store unavailable' } }, { status: 503 });
    if (control.unavailable) return Response.json({ error: { message: 'Unknown route' } }, { status: 404 });
    return Response.json(control.state);
  }
  if (url.pathname.endsWith('/model-picker-options')) return Response.json({ contract_version: 'model_picker_projection_v1', generated_at: Date.now(), data: [], next_cursor: null });
  if (url.pathname.endsWith('/filter-assistant/settings')) return Response.json(null);
  return Response.json({ error: { message: 'Unknown route' } }, { status: 404 });
};

function Fixture() {
  const { locale, setLocale } = useI18n();
  return <main style={{ padding: 12 }}>
    <button type="button" data-switch-locale onClick={() => setLocale(locale === 'en' ? 'zh-CN' : 'en')}>Switch language</button>
    <SystemSettingsPage token="fixture" tenant="tenant" />
  </main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><MtcFluentProvider><Fixture /></MtcFluentProvider></I18nProvider>);
