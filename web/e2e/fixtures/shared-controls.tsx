import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import { Button, Checkbox, Combobox, Dropdown, Input, MtcFluentProvider, Option, Select, Switch, Textarea } from '../../src/design-system';
import sharedStyles from '../../src/styles.css?inline';
import themeStyles from '../../src/theme.css?inline';
import operatorStyles from '../../src/operator/operator.css?inline';
import managementStyles from '../../src/operator/managementSurfaces.css?inline';

const parameters = new URLSearchParams(location.search);
const locale = parameters.get('locale') === 'zh-CN' ? 'zh-CN' : 'en';
const labels = locale === 'zh-CN'
  ? { refresh: '刷新', save: '保存', compact: '查看', remove: '移除', long: '刷新所选账户并重新读取全部可用模型与最新配额状态' }
  : { refresh: 'Refresh', save: 'Save', compact: 'View', remove: 'Remove', long: 'Refresh the selected account and reload all available models and the latest quota status' };
document.documentElement.dataset.theme = parameters.get('theme') === 'light' ? 'light' : 'dark';
const stylesheet = document.createElement('style');
stylesheet.textContent = parameters.has('baseline') ? themeStyles : [sharedStyles, themeStyles, operatorStyles, managementStyles].join('\n');
document.head.append(stylesheet);

const surfaces = [
  { name: 'providers', className: 'provider-list-actions' },
  { name: 'accounts', className: 'account-meta', child: 'row-actions' },
  { name: 'credentials', className: 'credential-row-actions row-actions' },
  { name: 'routes', className: 'resource-list-status-filter' },
  { name: 'pricing', className: 'pricing-heading-actions' },
  { name: 'plugins', className: 'managed-resource-header', child: 'row-actions' },
  { name: 'system-settings', className: 'form-panel button-row' },
  { name: 'requests', className: 'filter-actions' },
  { name: 'sessions', className: 'filter-actions' },
  { name: 'usage-presets', className: 'usage-presets' },
  { name: 'usage-tabs', className: 'usage-tabs' },
  { name: 'group-list', className: 'group-list' },
  { name: 'typed-filter', className: 'typed-filter-row-actions' },
  { name: 'portal', className: 'credential button-row' },
] as const;

function Controls({ surface }: { surface: string }) {
  const [count, setCount] = useState(0);
  return <>
    <Button data-control="refresh" appearance="secondary" onClick={() => setCount(value => value + 1)}>{labels.refresh}</Button>
    <Button data-control="save" appearance="primary">{labels.save}</Button>
    <Button data-control="compact" size="small" appearance="subtle" className="compact-button">{labels.compact}</Button>
    <Button data-control="icon" size="small" appearance="subtle" icon={<span aria-hidden="true">+</span>} aria-label={labels.compact} />
    <Button data-control="disabled" appearance="secondary" disabled>{labels.refresh}</Button>
    <output data-count={surface} style={{ display: 'none' }}>{count}</output>
  </>;
}

function Preview() {
  return <main style={{ padding: 16, maxWidth: 960, margin: '0 auto' }}>
    <h1>Shared control surfaces</h1>
    <section data-controls="reference" style={{ display: 'flex', flexWrap: 'wrap', gap: 8 }}><Controls surface="reference" /></section>
    {surfaces.map(surface => <section key={surface.name} className="app-main-content" data-surface="operator" data-route={surface.name} style={{ marginBlock: 16, minWidth: 0 }}>
      <h2>{surface.name}</h2>
      <div className={surface.className} data-controls={surface.name}>
        {'child' in surface ? <div className={surface.child}><Controls surface={surface.name} /></div> : <Controls surface={surface.name} />}
      </div>
    </section>)}
    <section data-controls="long-label" className="row-actions" style={{ maxWidth: '100%' }}>
      <Button appearance="secondary" data-control="long">{labels.long}</Button>
      <Button appearance="primary" data-control="save">{labels.save}</Button>
    </section>
    <section data-controls="native" className="row-actions"><button type="button">{labels.refresh}</button><button type="button" className="secondary">{labels.save}</button><button type="button" className="danger">{labels.remove}</button></section>
    <section className="usage-presets"><Button data-control="selected" appearance="secondary" aria-pressed="true">{labels.refresh}</Button></section>
    <section data-controls="fields" className="form-panel" style={{ display: 'grid', gap: 16 }}>
      <Input aria-label="Account name" defaultValue="Example account" />
      <Select aria-label="Account status" defaultValue="active"><option value="active">Active</option></Select>
      <Textarea aria-label="Description" defaultValue="Example description" />
      <Combobox aria-label="Model" defaultValue="Example model"><Option>Example model</Option></Combobox>
      <Dropdown aria-label="Workspace" defaultValue="Example workspace"><Option>Example workspace</Option></Dropdown>
      <Checkbox label="Enabled account" defaultChecked />
      <Switch label="Enable service" defaultChecked />
    </section>
  </main>;
}

createRoot(document.getElementById('root')!).render(<MtcFluentProvider><Preview /></MtcFluentProvider>);
