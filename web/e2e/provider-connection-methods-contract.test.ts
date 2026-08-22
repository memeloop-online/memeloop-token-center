import assert from 'node:assert/strict';
import test from 'node:test';

import { directCredentialSchema, supportsDirectConnection } from '../src/operator/providerConnectionMethods.js';
import type { ProviderType } from '../src/types.js';

const dualMethodProvider: ProviderType = {
  id: 'dual-method',
  display_name: 'Dual method provider',
  protocols: ['openai'],
  modalities: ['text'],
  config_schema: { type: 'object' },
  credential_schema: {
    oneOf: [
      { title: 'API key', properties: { type: { const: 'api_key' }, value: { type: 'string' } } },
      { title: 'OAuth', properties: { type: { const: 'oauth' }, access_token: { type: 'string' } } },
    ],
  },
  oauth_adapter: {
    api_version: 'oauth-adapter-v1',
    flow_kind: 'openai_device',
    login_url: 'https://authorization.example.test/start',
    poll_url: 'https://authorization.example.test/poll',
    refresh_url: 'https://authorization.example.test/refresh',
  },
  source: 'contract-test',
};

test('one provider can offer direct API credentials and account authorization', () => {
  assert.equal(supportsDirectConnection(dualMethodProvider), true);
  assert.ok(dualMethodProvider.oauth_adapter, 'authorization method must remain available');
  const direct = directCredentialSchema(dualMethodProvider.credential_schema);
  assert.deepEqual((direct?.oneOf as Array<{ properties: { type: { const: string } } }>).map((variant) => variant.properties.type.const), ['api_key']);
});

test('OAuth-only providers are not shown as direct credential providers', () => {
  const oauthOnly = {
    ...dualMethodProvider,
    credential_schema: { properties: { type: { const: 'oauth' }, access_token: { type: 'string' } } },
  };
  assert.equal(supportsDirectConnection(oauthOnly), false);
  assert.equal(directCredentialSchema(oauthOnly.credential_schema), undefined);
});
