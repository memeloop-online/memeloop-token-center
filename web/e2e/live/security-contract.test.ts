import assert from 'node:assert/strict';
import { chmod, mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import {
  assertReadOnlyMethod, assertSecureLiveURL, credentialsAreBoundToDestination,
  headersContainCredential, isAllowedLiveDestination, isReadOnlyMethod, readCredentialFile,
  urlContainsCredential,
} from './security.js';

test('live browser method guard allows only read-only HTTP methods', () => {
  for (const method of ['GET', 'HEAD', 'OPTIONS', 'get']) assert.equal(isReadOnlyMethod(method), true);
  for (const method of ['POST', 'PUT', 'PATCH', 'DELETE', 'CONNECT', 'TRACE']) {
    assert.equal(isReadOnlyMethod(method), false);
    assert.throws(() => assertReadOnlyMethod(method), /read-only guard rejected/);
  }
});

test('live browser destination guard allows only configured HTTPS origins', () => {
  const origins = new Set(['https://control.example.test', 'https://gateway.example.test']);
  assert.equal(isAllowedLiveDestination(
    'https://control.example.test/operator', origins, 'https://control.example.test',
  ), true);
  assert.equal(isAllowedLiveDestination(
    'https://gateway.example.test/v1/models', origins, 'https://gateway.example.test',
  ), true);
  assert.equal(isAllowedLiveDestination(
    'https://gateway.example.test/v1/models', origins, 'https://control.example.test',
  ), false);
  assert.equal(isAllowedLiveDestination(
    'https://control.example.test.evil.test/operator', origins, 'https://control.example.test',
  ), false);
  assert.equal(isAllowedLiveDestination(
    'https://attacker.example.test/?credential=leak', origins, 'https://control.example.test',
  ), false);
  assert.equal(isAllowedLiveDestination(
    'http://control.example.test/operator', origins, 'https://control.example.test',
  ), false);
  assert.equal(isAllowedLiveDestination('not a URL', origins, 'https://control.example.test'), false);
  assert.doesNotThrow(() => assertSecureLiveURL('control', new URL('https://control.example.test')));
  assert.throws(() => assertSecureLiveURL('control', new URL('http://control.example.test')), /must use HTTPS/);
  assert.equal(urlContainsCredential('https://control.example.test/requests?cursor=next', ['mts_secret']), false);
  assert.throws(() => assertSecureLiveURL('control', new URL('https://control.example.test/operator')), /without a path/);
  assert.throws(() => assertSecureLiveURL('control', new URL('https://control.example.test/?next=operator')), /without a query/);
  assert.equal(urlContainsCredential('https://control.example.test/?key=mts_secret', ['mts_secret']), true);
  assert.equal(urlContainsCredential('https://control.example.test/mts_secret/history', ['mts_secret']), true);
  assert.equal(urlContainsCredential('https://control.example.test/mts%5Fsecret/history', ['mts_secret']), true);
});

test('live browser credentials may appear in headers only at their bound origin', () => {
  const bindings = [
    { credential: 'svc_contract_secret', origin: 'https://control.example.test' },
    { credential: 'mts_contract_secret', origin: 'https://gateway.example.test' },
    { credential: 'provider_contract_canary', origin: '' },
  ];
  assert.equal(headersContainCredential({ authorization: 'Bearer svc_contract_secret' }, 'svc_contract_secret'), true);
  assert.equal(credentialsAreBoundToDestination(
    'https://control.example.test/internal/v1/requests',
    { authorization: 'Bearer svc_contract_secret' },
    bindings,
  ), true);
  assert.equal(credentialsAreBoundToDestination(
    'https://gateway.example.test/self/v1/requests',
    { authorization: 'Bearer svc_contract_secret' },
    bindings,
  ), false);
  assert.equal(credentialsAreBoundToDestination(
    'https://control.example.test/internal/v1/requests',
    { authorization: 'Bearer mts_contract_secret' },
    bindings,
  ), false);
  assert.equal(credentialsAreBoundToDestination(
    'https://gateway.example.test/self/v1/requests',
    { authorization: 'Bearer mts_contract_secret' },
    bindings,
  ), true);
  assert.equal(credentialsAreBoundToDestination(
    'https://control.example.test/internal/v1/upstreams',
    { 'x-debug': 'provider_contract_canary' },
    bindings,
  ), false);
});

test('credential reader requires a current-user regular 0600 file and never follows symlinks', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'mtc-live-security-'));
  const secure = join(directory, 'secure.json');
  const loose = join(directory, 'loose.txt');
  const linked = join(directory, 'linked.txt');
  try {
    await writeFile(secure, JSON.stringify({ key: 'contract-fixture', key_id: 'stable-contract-id' }), { mode: 0o600 });
    assert.deepEqual(await readCredentialFile(secure), {
      credential: 'contract-fixture',
      expectedKeyId: 'stable-contract-id',
    });

    await writeFile(loose, 'contract-fixture', { mode: 0o600 });
    await chmod(loose, 0o640);
    await assert.rejects(readCredentialFile(loose), /exactly 0600/);

    await symlink(secure, linked);
    await assert.rejects(readCredentialFile(linked), /must not be a symbolic link/);
  } finally {
    await rm(directory, { recursive: true, force: true });
  }
});
