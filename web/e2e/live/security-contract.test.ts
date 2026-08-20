import assert from 'node:assert/strict';
import { chmod, mkdtemp, rm, symlink, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import test from 'node:test';

import { assertReadOnlyMethod, isReadOnlyMethod, readCredentialFile } from './security.js';

test('live browser method guard allows only read-only HTTP methods', () => {
  for (const method of ['GET', 'HEAD', 'OPTIONS', 'get']) assert.equal(isReadOnlyMethod(method), true);
  for (const method of ['POST', 'PUT', 'PATCH', 'DELETE', 'CONNECT', 'TRACE']) {
    assert.equal(isReadOnlyMethod(method), false);
    assert.throws(() => assertReadOnlyMethod(method), /read-only guard rejected/);
  }
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
