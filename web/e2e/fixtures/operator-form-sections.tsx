import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import Form from '@rjsf/core/lib/components/Form.js';
import { I18nProvider } from '../../src/i18n';
import { safeValidator } from '../../src/safeValidator';
import { upstreamFormTemplates } from '../../src/operator/UpstreamFormTemplates';
import { schemaFormTemplates } from '../../src/SchemaTemplates';
import { useInlineEditorFocus } from '../../src/operator/hooks/useInlineEditorFocus';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';
import '../../src/operator/upstreamConnection.css';

window.fetch = async () => { throw new Error('No network is allowed in the form layout fixture'); };
const schema = { type: 'object' as const, properties: {
  name: { type: 'string' as const, title: 'Connection name' },
  config: { type: 'object' as const, required: ['video_api', 'network_scope'], properties: {
    base_url: { type: 'string' as const, title: 'Base URL' },
    timeout_seconds: { type: 'integer' as const, title: 'Timeout seconds', minimum: 1, maximum: 120 },
    network_scope: { type: 'string' as const, title: 'Required network scope', const: 'public', readOnly: true, default: 'public' },
    image_main_model: { type: 'string' as const, title: 'Image model' },
    video_api: { type: 'string' as const, title: 'Required video interface', default: 'video-v1' },
    plugin_extension: { type: 'string' as const, title: 'Plugin extension' },
  } },
} };
function FocusLifecycle() {
  const [editing, setEditing] = useState<string>();
  const [busy, setBusy] = useState(false);
  const [generation, setGeneration] = useState(0);
  const { container, rememberTrigger } = useInlineEditorFocus(editing, busy, 'fixture-scope');
  return <section ref={container}>
    <h2>Editor focus lifecycle</h2>
    {['Alpha', 'Beta'].map((id) => <button key={`${id}-${generation}`} type="button" disabled={busy} data-inline-edit-trigger={id} onClick={(event) => { rememberTrigger(id, event.currentTarget); setEditing(id); }}>Edit {id}</button>)}
    {editing && <form className="inline-editor" onSubmit={(event) => { event.preventDefault(); setBusy(true); }}>
      <label>Edit value<input required disabled={busy} /></label>
      <button type="submit" disabled={busy}>Save edit</button>
      <button type="button" disabled={busy} onClick={() => setEditing(undefined)}>Cancel edit</button>
    </form>}
    <button type="button" disabled={!busy} onClick={() => setBusy(false)}>Reject save</button>
    <button type="button" disabled={!busy} onClick={() => { setEditing(undefined); setGeneration((value) => value + 1); }}>Accept save and refresh rows</button>
    <button type="button" disabled={!busy || Boolean(editing)} onClick={() => setBusy(false)}>Finish refresh</button>
  </section>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><main style={{ padding: 24, maxWidth: 760, margin: '0 auto' }}>
  <h1>Upstream connection</h1>
  <div className="form-panel"><Form schema={schema} formData={{ name: 'Research workspace', config: { base_url: 'https://provider.example.invalid/v1', timeout_seconds: 0 } }} validator={safeValidator} templates={upstreamFormTemplates} onSubmit={() => {}}><button type="submit">Save fixture</button></Form></div>
  <FocusLifecycle />
  <div className="form-panel" data-layout-contract>
    <Form schema={{ type: 'object', properties: {
      future: { title: 'Future setting', default: 'schema-default-must-not-render', examples: ['schema-example-must-not-render'] },
      entries: { type: 'array', title: 'LongUnbrokenPluginArrayTitle'.repeat(12), items: { type: 'string' } },
    } }} formData={{ future: 'existing-value-must-not-render', entries: ['editable'] }} validator={safeValidator} templates={schemaFormTemplates}
      onSubmit={({ formData }) => { Object.assign(window, { formLayoutSubmission: formData }); }}>
      <button type="submit">Save extended fields</button>
    </Form>
  </div>
</main></I18nProvider>);
