import { createRoot } from 'react-dom/client';
import Form from '@rjsf/core/lib/components/Form.js';
import { I18nProvider } from '../../src/i18n';
import { safeValidator } from '../../src/safeValidator';
import { upstreamFormTemplates } from '../../src/operator/UpstreamFormTemplates';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';
import '../../src/operator/upstreamConnection.css';

window.fetch = async () => { throw new Error('No network is allowed in the form layout fixture'); };
const schema = { type: 'object' as const, properties: {
  name: { type: 'string' as const, title: 'Connection name' },
  config: { type: 'object' as const, required: ['video_api'], properties: {
    base_url: { type: 'string' as const, title: 'Base URL' },
    timeout_seconds: { type: 'integer' as const, title: 'Timeout seconds', minimum: 1, maximum: 120 },
    image_main_model: { type: 'string' as const, title: 'Image model' },
    video_api: { type: 'string' as const, title: 'Required video interface', default: 'video-v1' },
    plugin_extension: { type: 'string' as const, title: 'Plugin extension' },
  } },
} };
createRoot(document.getElementById('root')!).render(<I18nProvider><main style={{ padding: 24, maxWidth: 760, margin: '0 auto' }}>
  <h1>Upstream connection</h1>
  <div className="form-panel"><Form schema={schema} formData={{ name: 'Research workspace', config: { base_url: 'https://provider.example.invalid/v1', timeout_seconds: 0 } }} validator={safeValidator} templates={upstreamFormTemplates} onSubmit={() => {}}><button type="submit">Save fixture</button></Form></div>
</main></I18nProvider>);
