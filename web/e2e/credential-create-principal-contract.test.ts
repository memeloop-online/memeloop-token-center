import assert from 'node:assert/strict';
import test from 'node:test';

import { credentialCreatePrincipal } from '../src/operator/credentialCreatePrincipal.js';

test('credential creation derives the principal from the alias unless an existing principal is bound', () => {
  assert.equal(credentialCreatePrincipal('研发工作区', ''), '研发工作区', 'an empty override keeps the readable alias as the principal');
  assert.equal(credentialCreatePrincipal('Research workspace', '   '), 'Research workspace', 'a whitespace-only override never blanks the principal');
  assert.equal(credentialCreatePrincipal('Second key', ' tenant-a-principal '), ' tenant-a-principal ', 'an explicit override is submitted exactly; identities are never rewritten');
  assert.equal(credentialCreatePrincipal('Second key', 'tenant-a-principal'), 'tenant-a-principal');
  assert.equal(credentialCreatePrincipal(undefined, 'owner-app'), 'owner-app', 'the override stands alone while the alias awaits validation');
  assert.equal(credentialCreatePrincipal(undefined, ''), '', 'schema validation still rejects a missing name before submit');
});
