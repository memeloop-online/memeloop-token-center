import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const tenants = readFileSync(new URL('../src/operator/TenantManager.tsx', import.meta.url), 'utf8');
const settings = readFileSync(new URL('../src/operator/pages/SystemSettingsPage.tsx', import.meta.url), 'utf8');

test('operator tenant management distinguishes initial loading from a recoverable list failure', () => {
  assert.match(tenants, /const \[loading, setLoading\] = useState\(false\)/);
  assert.match(tenants, /setLoading\(true\)/);
  assert.match(tenants, /setLoading\(false\)/);
  assert.match(tenants, /aria-busy=\{loading \|\| Boolean\(busy\)\}/);
  assert.match(tenants, /className="empty tenant-list-state"/);
  assert.match(tenants, /onClick=\{\(\) => void load\(true\)\}/);
});

test('operator tenant and settings writes retain native form submission', () => {
  assert.match(tenants, /<form className="tenant-create-row" onSubmit=/);
  assert.match(tenants, /id="tenant-external-id" name="tenant_external_id" autoComplete="off"/);
  assert.match(tenants, /<button type="submit" disabled=\{loading \|\| busy === 'create' \|\| !name\.trim\(\)\}/);
  assert.match(settings, /<form className="system-settings-form" onSubmit=/);
  assert.match(settings, /<button type="submit" disabled=\{saving \|\| !selectedRouteId \|\| !selectedRouteHasAvailableCandidate\}/);
});

test('system-settings load failure cannot be rendered as the no-enabled-route empty state', () => {
  assert.match(settings, /const \[loadError, setLoadError\] = useState\(''\)/);
  assert.match(settings, /messageOf\(reason, t\('common\.requestFailed'\)\)/);
  assert.match(settings, /loading \? <div className="empty" role="status">/);
  assert.match(settings, /loadError \? <div className="settings-empty" role="alert"><b>\{t\('settings\.filterAssistantLoadFailed'\)\}<\/b><span>\{loadError\}<\/span><button type="button" className="secondary" onClick=\{\(\) => void load\(\)\}>\{t\('common\.retry'\)\}/);
  assert.match(settings, /assistantOptions\.length === 0 \? <div className="settings-empty"><b>\{t\('settings\.noEnabledRoute'\)\}/);
});
