import type { RJSFSchema } from '@rjsf/utils';

export const builtinApiKeyCredential: RJSFSchema = {
  title: 'API key', type: 'object', additionalProperties: false, required: ['type', 'value'],
  properties: {
    type: { const: 'api_key', title: 'Credential type' },
    value: { type: 'string', minLength: 1, writeOnly: true, title: 'Credential value' },
    header: { type: 'string', default: 'authorization' },
    prefix: { type: 'string', default: 'Bearer ' },
  },
};
