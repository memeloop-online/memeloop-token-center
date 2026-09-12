import { createRoot } from 'react-dom/client';
import { useState } from 'react';
import Form from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { I18nProvider } from '../../src/i18n';
import { safeValidator } from '../../src/safeValidator';
import { schemaFormTemplates } from '../../src/SchemaTemplates';
import { credentialCreateSchema, credentialCreateUiSchema, credentialFormFields } from '../../src/operator/CredentialForm';
import '../../src/styles.css';
import '../../src/theme.css';
import '../../src/operator/operator.css';

const schema: RJSFSchema = {
  type: 'object', required: ['alias', 'policy', 'route_ids'], properties: {
    alias: { type: 'string', title: 'Alias' },
    route_ids: { type: 'array', title: 'Model routes', items: { type: 'string' } },
    route_group_ids: { type: 'array', title: 'Route groups', items: { type: 'string' } },
    policy: { type: 'object', title: 'Policy', properties: {
      enforcement_mode: { default: 'prepaid', oneOf: [
        { const: 'prepaid', title: 'Prepaid' }, { const: 'metered_unlimited', title: 'Metered unlimited' },
      ] },
      requests_per_minute: { type: 'integer', title: 'Requests per minute', default: 60, minimum: 1 },
    } },
    extension: { type: 'string', title: 'Extension field' },
  },
};
function Fixture() {
  const [result, setResult] = useState('');
  return <main style={{ padding: 24, maxWidth: 760, margin: '0 auto' }}><div className="form-panel">
    <Form schema={credentialCreateSchema(schema)} uiSchema={credentialCreateUiSchema} fields={credentialFormFields}
      validator={safeValidator} templates={schemaFormTemplates} onSubmit={({ formData }) => setResult(JSON.stringify(formData))}>
      <button type="submit">Create fixture</button>
    </Form><output style={{ overflowWrap: 'anywhere' }}>{result}</output>
  </div></main>;
}
createRoot(document.getElementById('root')!).render(<I18nProvider><Fixture /></I18nProvider>);
